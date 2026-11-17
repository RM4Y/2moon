//! A running streaming session: launches the app, negotiates RTSP, opens
//! the control stream and runs the video and audio receive pipelines on
//! dedicated threads.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use moon_media::decoder::VideoDecoder;
use moon_media::encoder::Codec;
use moon_media::opus::{OpusDecoder, StreamLayout};
use moon_media::pulse::PulseStream;
use moon_proto::audio::{AudioCipher, AudioReassembler};
use moon_proto::video::{Event, Reassembler};
use tracing::{debug, info, warn};

use crate::control::{self, ControlEvent, ControlSender};
use crate::http::{GsClient, LaunchParams};
use crate::rtsp::{Negotiated, RtspClient, StreamConfig};

pub use moon_media::decoder::Decoded;
pub use moon_media::frame::I420Frame;

#[derive(Default)]
pub struct Stats {
    pub frames_received: AtomicU64,
    pub frames_decoded: AtomicU64,
    pub frames_lost: AtomicU64,
    pub shards_recovered: AtomicU64,
    pub idr_requests: AtomicU64,
    /// Reference frame invalidation requests (RFI).
    pub rfi_requests: AtomicU64,
    pub video_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_recovered: AtomicU64,
    pub audio_lost: AtomicU64,
    /// Microphone packets sent to the host.
    pub mic_packets: AtomicU64,
    pub decode_us_total: AtomicU64,
    /// Frames thrown away because the decoder fell behind.
    pub frames_dropped: AtomicU64,
    /// Decoder in use (set once the session starts).
    pub decoder: Mutex<String>,
}

impl Stats {
    pub fn summary(&self) -> String {
        let dec = self.frames_decoded.load(Ordering::Relaxed);
        format!(
            "decoder {} | frames: {} received, {} decoded, {} dropped (late), {} lost, {} FEC shards rebuilt, {} IDR requests, {} RFI requests | video {:.1} MB | audio: {} packets, {} recovered, {} lost | mic: {} packets sent | avg decode {:.2} ms",
            self.decoder.lock().unwrap(),
            self.frames_received.load(Ordering::Relaxed),
            dec,
            self.frames_dropped.load(Ordering::Relaxed),
            self.frames_lost.load(Ordering::Relaxed),
            self.shards_recovered.load(Ordering::Relaxed),
            self.idr_requests.load(Ordering::Relaxed),
            self.rfi_requests.load(Ordering::Relaxed),
            self.video_bytes.load(Ordering::Relaxed) as f64 / 1e6,
            self.audio_packets.load(Ordering::Relaxed),
            self.audio_recovered.load(Ordering::Relaxed),
            self.audio_lost.load(Ordering::Relaxed),
            self.mic_packets.load(Ordering::Relaxed),
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
    /// Requested codec (None = best supported by both sides).
    pub codec: Option<Codec>,
    /// Use the GPU decoder (NVDEC / VAAPI) when available.
    pub hw_decode: bool,
    /// Send the default microphone to the host (if the host supports it).
    pub mic: bool,
    /// Ask for 4:4:4 chroma when the host and this machine can do it.
    pub yuv444: bool,
    /// Accept HDR (10-bit) when the host and this machine can do it; shown
    /// tone-mapped to SDR.
    pub hdr: bool,
    /// Zero-copy: let GPU-decoded pictures stay on the GPU while this is true
    /// (the sink must handle [`Decoded::Vaapi`] and clear it if it cannot).
    pub zero_copy: Option<Arc<AtomicBool>>,
}

/// Receives every decoded picture on the decode thread, with its frame index.
/// A CPU picture is only borrowed: copy what you need, quickly. GPU pictures
/// ([`Decoded::Vaapi`]) only come when [`SessionOptions::zero_copy`] is set.
pub type FrameSink = Box<dyn FnMut(Decoded<'_>, u32) + Send>;

pub struct Session {
    pub control: ControlSender,
    pub control_events: mpsc::Receiver<ControlEvent>,
    pub stats: Arc<Stats>,
    pub negotiated: Negotiated,
    pub width: u32,
    pub height: u32,
    pub codec: Codec,
    /// Microphone streaming to the host (false if disabled or unsupported by the host).
    pub mic_active: bool,
    /// Mute switch for the microphone.
    pub mic_muted: Arc<AtomicBool>,
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
        // Pick the codec: the best one both the host and this machine support.
        let host_codecs = |c: Codec| match c {
            Codec::H264 => true,
            Codec::Hevc => info.codec_support & 0x300 != 0,
            Codec::Av1 => info.codec_support & 0x30000 != 0,
        };
        let local = moon_media::decoder::supported(opts.hw_decode);
        let codec = match opts.codec {
            Some(c) if !host_codecs(c) => anyhow::bail!("the host cannot encode {c:?}"),
            Some(c) if opts.decode && !local.contains(&c) => {
                anyhow::bail!("this computer cannot decode {c:?} (no GPU decoder for it, and FFmpeg lacks a CPU decoder); choose another codec or \"auto\"")
            }
            Some(c) => c,
            None => local.iter().copied().find(|c| host_codecs(*c)).unwrap_or(Codec::H264),
        };
        let mut opts = opts;
        opts.stream.video_format = codec as u32;
        // 4:4:4: Sunshine-extension bits of ServerCodecModeSupport.
        let host_444 = match codec {
            Codec::H264 => info.codec_support & 0x40000 != 0,
            Codec::Hevc => info.codec_support & 0x80000 != 0,
            Codec::Av1 => info.codec_support & 0x200000 != 0,
        };
        // HDR (10-bit): HEVC Main10 / AV1 Main10 bits.
        let host_10bit = match codec {
            Codec::H264 => false,
            Codec::Hevc => info.codec_support & 0x200 != 0,
            Codec::Av1 => info.codec_support & 0x20000 != 0,
        };
        opts.stream.hdr = opts.hdr && host_10bit && moon_media::decoder::supports_10bit(codec, opts.hw_decode);
        if opts.hdr && !opts.stream.hdr {
            info!("HDR not available ({}); using SDR", if host_10bit { "this computer cannot decode 10-bit" } else { "not offered by the host for this codec" });
        }
        opts.stream.yuv444 = !opts.stream.hdr && opts.yuv444 && host_444 && moon_media::decoder::supports_444(codec, opts.hw_decode);
        if opts.yuv444 && !opts.stream.yuv444 {
            info!("4:4:4 not available ({}); using 4:2:0", if host_444 { "this computer cannot decode it" } else { "not offered by the host for this codec" });
        }
        info!("video codec: {codec:?}{}", if opts.stream.hdr { " 10-bit HDR" } else if opts.stream.yuv444 { " 4:4:4" } else { "" });
        let s = &opts.stream;
        let lp = LaunchParams { app_id, width: s.width, height: s.height, fps: s.fps, rikey, rikeyid, audio_channels: s.audio_channels };
        let resume = info.current_game != 0 && info.current_game == app_id;
        if info.current_game != 0 && !resume {
            info!("another app is running on the host, quitting it first");
            gs.cancel().await?;
        }
        let url = gs.launch(&lp, resume).await?;
        info!("{} app {app_id}; RTSP at {url}", if resume { "resumed" } else { "launched" });
        let mut rtsp = RtspClient::new(&url, &gs.host, info.http_port.wrapping_add(21)).with_key(&url, rikey);
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
            let rfi = neg.rfi;
            threads.push(std::thread::Builder::new().name("video-rx".into()).spawn(move || {
                if let Err(e) = video_rx(addr, ping, ftx, stop, stats, ctl, rfi) {
                    warn!("video receiver: {e:#}");
                }
            })?);
        }
        if opts.decode {
            let hw = opts.hw_decode;
            // No zero-copy for 4:4:4 (three full-size planes go through the CPU path).
            let zero_copy = if opts.stream.yuv444 || opts.stream.hdr { None } else { opts.zero_copy.clone() };
            let yuv444 = opts.stream.yuv444;
            let stats = stats.clone();
            let ctl = ctrl.sender.clone();
            let stop = stop.clone();
            threads.push(std::thread::Builder::new().name("video-dec".into()).spawn(move || {
                if let Err(e) = video_decode(frx, sink, stats, ctl, stop, codec, hw, zero_copy, yuv444) {
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

        // Microphone (2moon extension, only if the host advertises it).
        let mic_muted = Arc::new(AtomicBool::new(false));
        let mut mic_active = false;
        if opts.mic {
            match moon_proto::audio::mic::port_from_sdp(&neg.describe_sdp) {
                Some(port) => {
                    let addr = resolve(port)?;
                    let (stop, stats, muted) = (stop.clone(), stats.clone(), mic_muted.clone());
                    let cipher = moon_proto::audio::mic::cipher(rikey, rikeyid);
                    threads.push(std::thread::Builder::new().name("mic-tx".into()).spawn(move || {
                        if let Err(e) = mic_tx(addr, cipher, stop, stats, muted) {
                            warn!("microphone: {e:#}");
                        }
                    })?);
                    mic_active = true;
                    info!("microphone sent to the host (UDP {port})");
                }
                None => info!("the host does not accept a microphone (not a 2moon host, or disabled)"),
            }
        }

        Ok(Session { control: ctrl.sender, control_events: ctrl.events, stats, negotiated: neg, width: s.width, height: s.height, codec, mic_active, mic_muted, stop, threads })
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

/// What the receiver waits for after a loss before feeding the decoder again.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Recovery {
    None,
    /// A keyframe (IDR).
    Idr,
    /// A frame encoded without the invalidated references (RFI), or an IDR.
    Rfi,
}

/// Frames newer than a missing one before it is declared lost. Packets are paced
/// in frame order, so 1 (≈ one frame interval) is enough and recovers much
/// faster than waiting several frames.
const LOSS_WINDOW: u32 = 1;
/// Give up on a pending recovery and (re-)request an IDR after this long.
const RECOVERY_TIMEOUT: Duration = Duration::from_millis(500);

fn video_rx(addr: SocketAddr, ping: Option<String>, frames: mpsc::SyncSender<moon_proto::video::Frame>, stop: Arc<AtomicBool>, stats: Arc<Stats>, ctl: ControlSender, rfi: bool) -> Result<()> {
    let sock = udp_socket(addr)?;
    let mut r = Reassembler::new();
    r.window = LOSS_WINDOW;
    let mut buf = vec![0u8; 65536];
    let mut events = Vec::new();
    let mut ping_seq = 0u32;
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut last_idr = Instant::now() - Duration::from_secs(1);
    let mut got_any = false;
    // The stream starts with an IDR; nothing before it can be decoded.
    let mut recovery = Recovery::Idr;
    let mut recovery_since = Instant::now();
    if rfi {
        info!("reference frame invalidation enabled");
    }
    // Test hook: drop this percentage of received video packets (simulated network loss).
    let test_loss: f64 = std::env::var("MOON_TEST_VIDEO_LOSS").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    if test_loss > 0.0 {
        warn!("MOON_TEST_VIDEO_LOSS: dropping {test_loss}% of video packets");
    }
    let request_idr = |last_idr: &mut Instant, stats: &Stats| {
        if last_idr.elapsed() > Duration::from_millis(250) {
            ctl.request_idr();
            stats.idr_requests.fetch_add(1, Ordering::Relaxed);
            *last_idr = Instant::now();
        }
    };
    while !stop.load(Ordering::Relaxed) {
        // Keep pinging until traffic flows, then slowly as keep-alive (NAT mappings).
        let period = if got_any { Duration::from_millis(500) } else { Duration::from_millis(100) };
        if last_ping.elapsed() >= period {
            ping_seq += 1;
            let _ = sock.send(&ping_packet(&ping, ping_seq));
            last_ping = Instant::now();
        }
        // A recovery that takes too long (request or recovery frame lost): ask for an IDR.
        if recovery != Recovery::None && got_any && recovery_since.elapsed() > RECOVERY_TIMEOUT {
            debug!("no {recovery:?} recovery frame after {:?}, requesting IDR", recovery_since.elapsed());
            recovery = Recovery::Idr;
            recovery_since = Instant::now();
            request_idr(&mut last_idr, &stats);
        }
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => continue,
            Err(e) => return Err(e.into()),
        };
        got_any = true;
        if test_loss > 0.0 && rand::random::<f64>() * 100.0 < test_loss {
            continue;
        }
        stats.video_bytes.fetch_add(n as u64, Ordering::Relaxed);
        r.push(&buf[..n], &mut events);
        for ev in events.drain(..) {
            match ev {
                Event::Frame(f) => {
                    stats.frames_received.fetch_add(1, Ordering::Relaxed);
                    stats.shards_recovered.store(r.stats.shards_recovered, Ordering::Relaxed);
                    let usable = match recovery {
                        Recovery::None => true,
                        Recovery::Idr => f.idr,
                        Recovery::Rfi => f.is_rfi_recovery_point(),
                    };
                    if !usable {
                        // References a lost frame: decoding it would show garbage.
                        stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if recovery != Recovery::None {
                        debug!("recovered on frame {} (type {}) after {:?}", f.index, f.frame_type, recovery_since.elapsed());
                        recovery = Recovery::None;
                    }
                    if rfi && f.ltr {
                        ctl.ack_ltr_frame(f.index);
                    }
                    if frames.send(f).is_err() {
                        return Ok(());
                    }
                }
                Event::Lost { first, last } => {
                    stats.frames_lost.fetch_add((last.wrapping_sub(first) + 1) as u64, Ordering::Relaxed);
                    if rfi && recovery != Recovery::Idr {
                        debug!("frames {first}..={last} lost, invalidating references");
                        ctl.invalidate_ref_frames(first, last);
                        stats.rfi_requests.fetch_add(1, Ordering::Relaxed);
                        if recovery == Recovery::None {
                            recovery_since = Instant::now();
                        }
                        recovery = Recovery::Rfi;
                    } else {
                        debug!("frames {first}..={last} lost, requesting IDR");
                        if recovery == Recovery::None {
                            recovery_since = Instant::now();
                        }
                        recovery = Recovery::Idr;
                        request_idr(&mut last_idr, &stats);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Frames allowed to wait for the decoder before we skip ahead (latency bound).
const MAX_DECODE_BACKLOG: usize = 2;

#[allow(clippy::too_many_arguments)]
fn video_decode(frames: mpsc::Receiver<moon_proto::video::Frame>, mut sink: FrameSink, stats: Arc<Stats>, ctl: ControlSender, stop: Arc<AtomicBool>, codec: Codec, hw: bool, zero_copy: Option<Arc<AtomicBool>>, yuv444: bool) -> Result<()> {
    let mut dec = VideoDecoder::open(codec, hw, zero_copy, yuv444)?;
    let mut decoder_name = dec.name();
    *stats.decoder.lock().unwrap() = decoder_name.to_string();
    let mut need_idr = false;
    let mut last_idr_req = Instant::now() - Duration::from_secs(1);
    let mut backlog = std::collections::VecDeque::new();
    loop {
        // Gather everything that is already waiting.
        if backlog.is_empty() {
            match frames.recv() {
                Ok(f) => backlog.push_back(f),
                Err(_) => break,
            }
        }
        while let Ok(f) = frames.try_recv() {
            backlog.push_back(f);
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Decoder behind the stream: never let latency pile up. Jump to the newest
        // keyframe if one is queued, otherwise drop everything and ask for one.
        if backlog.len() > MAX_DECODE_BACKLOG {
            if let Some(i) = backlog.iter().rposition(|f| f.idr) {
                stats.frames_dropped.fetch_add(i as u64, Ordering::Relaxed);
                backlog.drain(..i);
            } else {
                stats.frames_dropped.fetch_add(backlog.len() as u64, Ordering::Relaxed);
                backlog.clear();
                need_idr = true;
                if last_idr_req.elapsed() > Duration::from_millis(200) {
                    debug!("decoder behind, dropping queued frames and requesting IDR");
                    ctl.request_idr();
                    stats.idr_requests.fetch_add(1, Ordering::Relaxed);
                    last_idr_req = Instant::now();
                }
                continue;
            }
        }
        let f = backlog.pop_front().unwrap();
        if need_idr && !f.idr {
            stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        need_idr = false;
        let t = Instant::now();
        match dec.decode(&f.data) {
            Ok(Some(pic)) => {
                stats.decode_us_total.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                stats.frames_decoded.fetch_add(1, Ordering::Relaxed);
                sink(pic, f.index);
            }
            Ok(None) => {}
            Err(e) => {
                warn!("decode error on frame {}: {e}; requesting IDR", f.index);
                ctl.request_idr();
                stats.idr_requests.fetch_add(1, Ordering::Relaxed);
                need_idr = true;
            }
        }
        // The decoder may change (VAAPI falling back to the CPU): keep the stats right.
        if dec.name() != decoder_name {
            decoder_name = dec.name();
            *stats.decoder.lock().unwrap() = decoder_name.to_string();
        }
    }
    Ok(())
}

/// Capture the default microphone, encode 10 ms Opus frames and send them to the host.
fn mic_tx(addr: SocketAddr, cipher: AudioCipher, stop: Arc<AtomicBool>, stats: Arc<Stats>, muted: Arc<AtomicBool>) -> Result<()> {
    use moon_proto::audio::mic;
    let sock = udp_socket(addr)?;
    // MOON_MIC_DEVICE: a PulseAudio/PipeWire source name instead of the default microphone.
    let device = std::env::var("MOON_MIC_DEVICE").ok().filter(|d| !d.is_empty());
    let mut rec = PulseStream::record(device.as_deref(), mic::SAMPLE_RATE, 1, mic::FRAME_MS)?;
    // Voice at 48 kbit/s VBR: clear speech, small packets.
    let mut enc = moon_media::opus::OpusEncoder::new(StreamLayout::mono(), mic::SAMPLE_RATE, mic::FRAME_MS, 48_000, false)?;
    let mut pcm = vec![0i16; mic::FRAME_SAMPLES];
    let mut opus = Vec::new();
    let (mut seq, mut ts) = (0u16, 0u32);
    let mut was_muted = false;
    while !stop.load(Ordering::Relaxed) {
        // Blocks for one 10 ms frame; keep reading while muted so no stale audio piles up.
        rec.read(&mut pcm)?;
        let m = muted.load(Ordering::Relaxed);
        if m != was_muted {
            info!("microphone {}", if m { "muted" } else { "unmuted" });
            was_muted = m;
        }
        if m {
            continue;
        }
        enc.encode(&pcm, &mut opus)?;
        let _ = sock.send(&mic::seal(&cipher, seq, ts, &opus));
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(mic::FRAME_SAMPLES as u32);
        stats.mic_packets.fetch_add(1, Ordering::Relaxed);
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
                if o.latency_us() > 100_000 {
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
