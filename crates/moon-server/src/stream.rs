//! Host streaming pipelines: video (capture -> encode -> packetize -> UDP)
//! and audio (capture -> Opus -> RTP+FEC -> UDP). Each UDP socket also
//! receives the client's pings, which tell us where to send.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use moon_media::frame::I420Frame;
use moon_media::encoder::VideoEncoder;
use moon_media::opus::{OpusEncoder, StreamLayout};
use moon_proto::audio::{AudioCipher, AudioPacketizer};
use moon_proto::video::Packetizer;
use tracing::{debug, info, warn};

use crate::capture::{AudioSource, VideoSource};

/// Parameters from the client's RTSP ANNOUNCE.
#[derive(Debug, Clone)]
pub struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub packet_size: u32,
    pub bitrate_kbps: u32,
    pub min_fec_packets: u32,
    pub fec_percent: u32,
    pub video_format: u32,
    pub audio_channels: u32,
    pub audio_packet_ms: u32,
    pub encryption: u32,
    pub encoder: String,
    /// Client's `maxNumReferenceFrames` (0 = unconstrained).
    pub max_ref_frames: u32,
}

impl Default for StreamParams {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 60,
            packet_size: 1392,
            bitrate_kbps: 10_000,
            min_fec_packets: 2,
            fec_percent: 20,
            video_format: 0,
            audio_channels: 2,
            audio_packet_ms: 5,
            encryption: 0,
            encoder: "auto".into(),
            max_ref_frames: 1,
        }
    }
}

/// Where the client's pings came from, per stream.
#[derive(Default)]
pub struct Peer(Mutex<Option<SocketAddr>>);

impl Peer {
    pub fn get(&self) -> Option<SocketAddr> {
        *self.0.lock().unwrap()
    }
    fn set(&self, a: SocketAddr) {
        let mut g = self.0.lock().unwrap();
        if *g != Some(a) {
            info!("stream peer {a}");
            *g = Some(a);
        }
    }
}

#[derive(Default)]
pub struct StreamStats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub idr_frames: AtomicU64,
    pub audio_packets: AtomicU64,
}

pub struct StreamHandle {
    pub stop: Arc<AtomicBool>,
    pub idr: Arc<AtomicBool>,
    pub stats: Arc<StreamStats>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl StreamHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Persistent UDP sockets bound at startup.
pub struct StreamSockets {
    pub video: UdpSocket,
    pub audio: UdpSocket,
}

impl StreamSockets {
    pub fn bind(video_port: u16, audio_port: u16) -> Result<Self> {
        let mk = |port: u16| -> Result<UdpSocket> {
            let s = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
            s.set_only_v6(false)?;
            let _ = s.set_send_buffer_size(8 << 20);
            s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
            let u: UdpSocket = s.into();
            u.set_read_timeout(Some(Duration::from_millis(100)))?;
            Ok(u)
        };
        Ok(Self { video: mk(video_port)?, audio: mk(audio_port)? })
    }
}

/// Listen for pings carrying our payload (or legacy "PING") and record the sender.
fn ping_listener(sock: UdpSocket, payload: [u8; 16], client_ip: std::net::IpAddr, peer: Arc<Peer>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let from_ip = match from.ip() {
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(from.ip()),
            ip => ip,
        };
        let ok = (n >= 16 && buf[..16] == payload) || (n == 4 && &buf[..4] == b"PING" && from_ip == client_ip);
        if ok {
            peer.set(from);
        } else {
            debug!("ignoring {n}-byte datagram from {from}");
        }
    }
}

pub struct StartArgs {
    pub params: StreamParams,
    pub sockets: Arc<StreamSockets>,
    pub ping_payload: [u8; 16],
    pub client_ip: std::net::IpAddr,
    pub rikey: [u8; 16],
    pub rikeyid: u32,
    pub video: Box<dyn VideoSource>,
    pub audio: Option<Box<dyn AudioSource>>,
}

pub fn start(a: StartArgs) -> Result<StreamHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let idr = Arc::new(AtomicBool::new(true));
    let stats = Arc::new(StreamStats::default());
    let vpeer = Arc::new(Peer::default());
    let apeer = Arc::new(Peer::default());
    let mut threads = Vec::new();

    for (sock, peer, name) in [(&a.sockets.video, vpeer.clone(), "video-ping"), (&a.sockets.audio, apeer.clone(), "audio-ping")] {
        let s = sock.try_clone()?;
        let (payload, ip, stop) = (a.ping_payload, a.client_ip, stop.clone());
        threads.push(std::thread::Builder::new().name(name.into()).spawn(move || ping_listener(s, payload, ip, peer, stop))?);
    }

    {
        let sock = a.sockets.video.try_clone()?;
        let (p, stop, idr, stats, src) = (a.params.clone(), stop.clone(), idr.clone(), stats.clone(), a.video);
        threads.push(std::thread::Builder::new().name("video-enc".into()).spawn(move || {
            if let Err(e) = video_loop(p, src, sock, vpeer, stop, idr, stats) {
                warn!("video pipeline stopped: {e:#}");
            }
        })?);
    }

    if let Some(src) = a.audio {
        let sock = a.sockets.audio.try_clone()?;
        let (p, stop, stats) = (a.params.clone(), stop.clone(), stats.clone());
        let cipher = (a.params.encryption & 4 != 0).then_some(AudioCipher { key: a.rikey, key_id: a.rikeyid });
        threads.push(std::thread::Builder::new().name("audio-enc".into()).spawn(move || {
            if let Err(e) = audio_loop(p, src, sock, apeer, cipher, stop, stats) {
                warn!("audio pipeline stopped: {e:#}");
            }
        })?);
    }
    Ok(StreamHandle { stop, idr, stats, threads })
}

fn video_loop(p: StreamParams, mut src: Box<dyn VideoSource>, sock: UdpSocket, peer: Arc<Peer>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, stats: Arc<StreamStats>) -> Result<()> {
    if p.video_format != 0 {
        warn!("client asked for codec {} but only H.264 is implemented; sending H.264", p.video_format);
    }
    let fps = p.fps.clamp(1, 240);
    let mut enc = VideoEncoder::open(&p.encoder, p.width as usize, p.height as usize, fps, p.bitrate_kbps, if p.max_ref_frames == 0 { 1 } else { p.max_ref_frames })?;
    let mut pk = Packetizer::new(p.packet_size as usize, p.fec_percent, p.min_fec_packets as usize);
    let mut frame = I420Frame::new(p.width as usize, p.height as usize);
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let start = Instant::now();
    let mut next = start;
    let mut index: u32 = 0;
    info!("video: {}x{}@{} {} kbps from {} via {}", p.width, p.height, fps, p.bitrate_kbps, src.name(), enc.name());
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        next += period;
        if Instant::now() > next + period * 4 {
            next = Instant::now() + period; // fell far behind: resync instead of bursting
        }
        let Some(dest) = peer.get() else { continue };
        let t0 = Instant::now();
        src.grab(&mut frame, p.width as usize, p.height as usize)?;
        if idr.swap(false, Ordering::Relaxed) {
            enc.request_idr();
        }
        let ts = (start.elapsed().as_micros() * 90 / 1000) as u32;
        let e = enc.encode(&frame, ts as i64)?;
        if e.data.is_empty() {
            continue;
        }
        index = index.wrapping_add(1);
        let elapsed = t0.elapsed();
        pk.processing_latency = (elapsed.as_micros() / 100).min(u16::MAX as u128) as u16;
        let is_idr = e.idr;
        let pkts = pk.packetize(index, ts, is_idr, e.data);
        stats.encode_us.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        stats.frames.fetch_add(1, Ordering::Relaxed);
        if is_idr {
            stats.idr_frames.fetch_add(1, Ordering::Relaxed);
        }
        for pkt in &pkts {
            stats.bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
            if let Err(e) = sock.send_to(pkt, dest) {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    debug!("video send: {e}");
                }
            }
        }
    }
    Ok(())
}

fn audio_loop(p: StreamParams, mut src: Box<dyn AudioSource>, sock: UdpSocket, peer: Arc<Peer>, cipher: Option<AudioCipher>, stop: Arc<AtomicBool>, stats: Arc<StreamStats>) -> Result<()> {
    let ms = p.audio_packet_ms.clamp(5, 20);
    let layout = StreamLayout::stereo();
    let mut enc = OpusEncoder::new(layout, 48000, ms, 128_000, true)?;
    let mut pk = AudioPacketizer::new(ms);
    let encrypted = cipher.is_some();
    pk.cipher = cipher;
    let mut pcm = vec![0i16; enc.frame_size * 2];
    let mut opus = Vec::new();
    info!("audio: stereo opus {ms} ms packets{}", if encrypted { ", encrypted" } else { "" });
    while !stop.load(Ordering::Relaxed) {
        src.read(&mut pcm)?;
        let Some(dest) = peer.get() else { continue };
        enc.encode(&pcm, &mut opus)?;
        for pkt in pk.packetize(&opus) {
            let _ = sock.send_to(&pkt, dest);
        }
        stats.audio_packets.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}
