//! A running streaming session: launches the app, negotiates RTSP, opens
//! the control stream and runs the video and audio receive pipelines on
//! dedicated threads.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use moon_media::h264::H264Decoder;
use moon_media::opus::{OpusDecoder, StreamLayout};
use moon_media::pulse::PulseStream;
use moon_proto::audio::{AudioCipher, AudioReassembler};
use moon_proto::video::{Event, Reassembler};
use tracing::{debug, info, warn};

use crate::control::{self, ControlEvent, ControlSender};
use crate::http::{GsClient, LaunchParams};
use crate::rtsp::{Negotiated, RtspClient, StreamConfig};

/// A decoded picture ready for display (packed 0x00RRGGBB).
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
    pub frame_index: u32,
    pub received_at: Instant,
}

#[derive(Default)]
pub struct Stats {
    pub frames_received: AtomicU64,
    pub frames_decoded: AtomicU64,
    pub frames_lost: AtomicU64,
    pub shards_recovered: AtomicU64,
    pub idr_requests: AtomicU64,
    pub video_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_recovered: AtomicU64,
    pub audio_lost: AtomicU64,
    pub decode_us_total: AtomicU64,
}

impl Stats {
    pub fn summary(&self) -> String {
        let dec = self.frames_decoded.load(Ordering::Relaxed);
        format!(
            "frames: {} received, {} decoded, {} lost, {} FEC shards rebuilt, {} IDR requests | video {:.1} MB | audio: {} packets, {} recovered, {} lost | avg decode {:.2} ms",
            self.frames_received.load(Ordering::Relaxed),
            dec,
            self.frames_lost.load(Ordering::Relaxed),
            self.shards_recovered.load(Ordering::Relaxed),
            self.idr_requests.load(Ordering::Relaxed),
            self.video_bytes.load(Ordering::Relaxed) as f64 / 1e6,
            self.audio_packets.load(Ordering::Relaxed),
            self.audio_recovered.load(Ordering::Relaxed),
            self.audio_lost.load(Ordering::Relaxed),
            if dec > 0 { self.decode_us_total.load(Ordering::Relaxed) as f64 / dec as f64 / 1000.0 } else { 0.0 },
        )
    }
}

pub struct SessionOptions {
    pub stream: StreamConfig,
    pub app_id: Option<u32>,
    /// Play audio on the local output device.
    pub audio_output: bool,
    /// Decode video (false only for pure network tests).
    pub decode: bool,
}

pub type FrameSink = Box<dyn Fn(Picture) + Send>;

pub struct Session {
    pub control: ControlSender,
    pub control_events: mpsc::Receiver<ControlEvent>,
    pub stats: Arc<Stats>,
    pub negotiated: Negotiated,
    pub width: u32,
    pub height: u32,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Session {
    pub async fn start(gs: &GsClient, opts: SessionOptions, sink: FrameSink) -> Result<Session> {
        let info = gs.server_info().await?;
        if !info.paired {
            anyhow::bail!("not paired with {}", gs.host);
        }
        let apps = gs.app_list().await?;
        let app_id = opts.app_id.or_else(|| apps.first().map(|a| a.id)).context("host has no applications")?;
        let rikey: [u8; 16] = moon_proto::crypto::random_bytes();
        let rikeyid: u32 = rand::random::<u32>() & 0x7fff_ffff;
        let s = &opts.stream;
        let lp = LaunchParams { app_id, width: s.width, height: s.height, fps: s.fps, rikey, rikeyid, audio_channels: s.audio_channels };
        let resume = info.current_game != 0 && info.current_game == app_id;
        if info.current_game != 0 && !resume {
            info!("another app is running on the host, quitting it first");
            gs.cancel().await?;
        }
        let url = gs.launch(&lp, resume).await?;
        info!("{} app {app_id}; RTSP at {url}", if resume { "resumed" } else { "launched" });
        let mut rtsp = RtspClient::new(&url, &gs.host, info.http_port.wrapping_add(21));
        let neg = rtsp.handshake(s).await?;
        debug!("negotiated {neg:?}");

        let resolve = |port: u16| -> Result<SocketAddr> {
            use std::net::ToSocketAddrs;
            (gs.host.as_str(), port).to_socket_addrs()?.next().context("cannot resolve host")
        };
        let ctrl = control::connect(resolve(neg.control_port)?, neg.control_connect_data.unwrap_or(0), rikey, true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let mut threads = Vec::new();

        // Video: network thread -> decode thread -> sink
        let (ftx, frx) = mpsc::sync_channel::<moon_proto::video::Frame>(8);
        {
            let addr = resolve(neg.video_port)?;
            let ping = neg.ping_payload_video.clone();
            let stop = stop.clone();
            let stats = stats.clone();
            let ctl = ctrl.sender.clone();
            threads.push(std::thread::Builder::new().name("video-rx".into()).spawn(move || {
                if let Err(e) = video_rx(addr, ping, ftx, stop, stats, ctl) {
                    warn!("video receiver: {e:#}");
                }
            })?);
        }
        if opts.decode {
            let stats = stats.clone();
            let ctl = ctrl.sender.clone();
            let stop = stop.clone();
            threads.push(std::thread::Builder::new().name("video-dec".into()).spawn(move || {
                if let Err(e) = video_decode(frx, sink, stats, ctl, stop) {
                    warn!("video decoder: {e:#}");
                }
            })?);
        } else {
            threads.push(std::thread::spawn(move || while frx.recv().is_ok() {}));
        }

        // Audio
        {
            let addr = resolve(neg.audio_port)?;
            let ping = neg.ping_payload_audio.clone();
            let stop = stop.clone();
            let stats = stats.clone();
            let layout = surround_layout(&neg.describe_sdp, s.audio_channels);
            let output = opts.audio_output;
            let cipher = neg.audio_encrypted().then_some(AudioCipher { key: rikey, key_id: rikeyid });
            threads.push(std::thread::Builder::new().name("audio".into()).spawn(move || {
                if let Err(e) = audio_rx(addr, ping, layout, cipher, output, stop, stats) {
                    warn!("audio pipeline: {e:#}");
                }
            })?);
        }

        Ok(Session { control: ctrl.sender, control_events: ctrl.events, stats, negotiated: neg, width: s.width, height: s.height, stop, threads })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.control.disconnect();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn surround_layout(describe: &str, channels: u32) -> StreamLayout {
    describe
        .lines()
        .filter_map(|l| l.split("surround-params=").nth(1))
        .filter_map(|p| StreamLayout::from_sdp(p.trim()))
        .find(|l| l.channels as u32 == channels)
        .unwrap_or_else(|| StreamLayout::for_channels(channels))
}

fn udp_socket(addr: SocketAddr) -> Result<UdpSocket> {
    let domain = if addr.is_ipv6() { socket2::Domain::IPV6 } else { socket2::Domain::IPV4 };
    let s = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    let _ = s.set_recv_buffer_size(8 << 20);
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    s.bind(&bind.into())?;
    let u: UdpSocket = s.into();
    u.connect(addr)?;
    u.set_read_timeout(Some(Duration::from_millis(20)))?;
    Ok(u)
}

fn ping_packet(payload: &Option<String>, seq: u32) -> Vec<u8> {
    match payload {
        Some(p) => {
            let mut v = p.as_bytes().to_vec();
            v.extend_from_slice(&seq.to_be_bytes());
            v
        }
        None => b"PING".to_vec(),
    }
}

fn video_rx(addr: SocketAddr, ping: Option<String>, frames: mpsc::SyncSender<moon_proto::video::Frame>, stop: Arc<AtomicBool>, stats: Arc<Stats>, ctl: ControlSender) -> Result<()> {
    let sock = udp_socket(addr)?;
    let mut r = Reassembler::new();
    let mut buf = vec![0u8; 65536];
    let mut events = Vec::new();
    let mut ping_seq = 0u32;
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut last_idr = Instant::now() - Duration::from_secs(1);
    let mut got_any = false;
    while !stop.load(Ordering::Relaxed) {
        // Keep pinging until traffic flows, then slowly as keep-alive (NAT mappings).
        let period = if got_any { Duration::from_millis(500) } else { Duration::from_millis(100) };
        if last_ping.elapsed() >= period {
            ping_seq += 1;
            let _ = sock.send(&ping_packet(&ping, ping_seq));
            last_ping = Instant::now();
        }
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
            Err(e) => return Err(e.into()),
        };
        got_any = true;
        stats.video_bytes.fetch_add(n as u64, Ordering::Relaxed);
        r.push(&buf[..n], &mut events);
        for ev in events.drain(..) {
            match ev {
                Event::Frame(f) => {
                    stats.frames_received.fetch_add(1, Ordering::Relaxed);
                    stats.shards_recovered.store(r.stats.shards_recovered, Ordering::Relaxed);
                    if frames.send(f).is_err() {
                        return Ok(());
                    }
                }
                Event::Lost { first, last } => {
                    stats.frames_lost.fetch_add((last.wrapping_sub(first) + 1) as u64, Ordering::Relaxed);
                    if last_idr.elapsed() > Duration::from_millis(250) {
                        debug!("frames {first}..={last} lost, requesting IDR");
                        ctl.request_idr();
                        stats.idr_requests.fetch_add(1, Ordering::Relaxed);
                        last_idr = Instant::now();
                    }
                }
            }
        }
    }
    Ok(())
}

fn video_decode(frames: mpsc::Receiver<moon_proto::video::Frame>, sink: FrameSink, stats: Arc<Stats>, ctl: ControlSender, stop: Arc<AtomicBool>) -> Result<()> {
    let mut dec = H264Decoder::new()?;
    let mut need_idr = false;
    while let Ok(f) = frames.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if need_idr && !f.idr {
            continue;
        }
        need_idr = false;
        let t = Instant::now();
        match dec.decode(&f.data) {
            Ok(Some(pic)) => {
                let mut px = vec![0u32; pic.width * pic.height];
                pic.to_xrgb(&mut px);
                stats.decode_us_total.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                stats.frames_decoded.fetch_add(1, Ordering::Relaxed);
                sink(Picture { width: pic.width, height: pic.height, pixels: px, frame_index: f.index, received_at: t });
            }
            Ok(None) => {}
            Err(e) => {
                warn!("decode error on frame {}: {e}; requesting IDR", f.index);
                ctl.request_idr();
                stats.idr_requests.fetch_add(1, Ordering::Relaxed);
                need_idr = true;
            }
        }
    }
    Ok(())
}

fn audio_rx(addr: SocketAddr, ping: Option<String>, layout: StreamLayout, cipher: Option<AudioCipher>, output: bool, stop: Arc<AtomicBool>, stats: Arc<Stats>) -> Result<()> {
    if cipher.is_some() {
        debug!("audio stream is encrypted");
    }
    let sock = udp_socket(addr)?;
    let mut dec = OpusDecoder::new(layout.clone(), 48000)?;
    let mut out = if output {
        match PulseStream::playback(48000, layout.channels, 30) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!("audio output unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let mut r = AudioReassembler::new();
    let mut buf = vec![0u8; 4096];
    let mut pkts = Vec::new();
    let mut pcm = Vec::new();
    let mut ping_seq = 0u32;
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut samples_per_packet = 240;
    let out_lock = Mutex::new(());
    while !stop.load(Ordering::Relaxed) {
        if last_ping.elapsed() >= Duration::from_millis(500) {
            ping_seq += 1;
            let _ = sock.send(&ping_packet(&ping, ping_seq));
            last_ping = Instant::now();
        }
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionRefused) => continue,
            Err(e) => return Err(e.into()),
        };
        r.push(&buf[..n], &mut pkts);
        stats.audio_recovered.store(r.stats.recovered, Ordering::Relaxed);
        for p in pkts.drain(..) {
            let got = match &p {
                Some(a) => {
                    stats.audio_packets.fetch_add(1, Ordering::Relaxed);
                    let plain;
                    let opus = match &cipher {
                        Some(c) => match c.decrypt(a.seq, &a.opus) {
                            Some(p) => {
                                plain = p;
                                &plain
                            }
                            None => {
                                stats.audio_lost.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        },
                        None => &a.opus,
                    };
                    let n = dec.decode(Some(opus), 0, &mut pcm)?;
                    samples_per_packet = n;
                    n
                }
                None => {
                    stats.audio_lost.fetch_add(1, Ordering::Relaxed);
                    dec.decode(None, samples_per_packet, &mut pcm)?
                }
            };
            if let (Some(o), true) = (out.as_mut(), got > 0) {
                let _g = out_lock.lock();
                // Drop audio instead of building latency if the device stalls.
                if o.latency_us() > 150_000 {
                    o.flush();
                }
                if let Err(e) = o.write(&pcm) {
                    warn!("audio output: {e:#}");
                    out = None;
                }
            }
        }
    }
    Ok(())
}
