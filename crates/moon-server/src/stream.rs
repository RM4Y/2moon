//! Host streaming pipelines: video (capture -> encode -> packetize -> UDP)
//! and audio (capture -> Opus -> RTP+FEC -> UDP). Each UDP socket also
//! receives the client's pings, which tell us where to send.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use moon_media::ffenc::EncoderOptions;
use moon_media::frame::{Csc, I420Frame};
use moon_media::encoder::{Codec, VideoEncoder};
use moon_media::glconv::{GpuConverter, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888};
use moon_media::nvenc::CudaInput;
use moon_media::opus::{OpusEncoder, StreamLayout};
use moon_proto::audio::{AudioCipher, AudioPacketizer};
use moon_proto::video::{Packetizer, FRAME_TYPE_IDR, FRAME_TYPE_P, FRAME_TYPE_RFI_RECOVERY};
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
    /// Client asked for 4:4:4 chroma (x-ss-video[0].chromaSamplingType = 1).
    pub yuv444: bool,
    /// Client asked for HDR (x-nv-video[0].dynamicRangeMode = 1): 10-bit HDR10.
    pub hdr: bool,
    /// Client's `encoderCscMode`.
    pub csc_mode: u32,
    pub nvenc_preset: String,
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
            yuv444: false,
            hdr: false,
            csc_mode: 0,
            nvenc_preset: "p4".into(),
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
    /// Losses recovered by reference frame invalidation (no IDR needed).
    pub rfi: AtomicU64,
    pub bytes: AtomicU64,
    pub encode_us: AtomicU64,
    pub idr_frames: AtomicU64,
    pub audio_packets: AtomicU64,
}

pub struct StreamHandle {
    pub stop: Arc<AtomicBool>,
    pub idr: Arc<AtomicBool>,
    /// Frames the client lost, to invalidate as references (RFI).
    pub invalidate: Arc<Mutex<Option<(u32, u32)>>>,
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
    pub live: Arc<Mutex<LiveStats>>,
}

/// Snapshot of the running stream, refreshed every second (admin panel).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LiveStats {
    pub active: bool,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    pub bitrate_kbps: u32,
    pub codec: String,
    pub encoder: String,
    pub source: String,
    pub audio_channels: u32,
    pub fps: f64,
    pub mbps: f64,
    pub encode_ms: f64,
    pub frames: u64,
    pub idr_frames: u64,
    pub peer: Option<String>,
}

pub fn start(a: StartArgs) -> Result<StreamHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let idr = Arc::new(AtomicBool::new(true));
    let invalidate: Arc<Mutex<Option<(u32, u32)>>> = Default::default();
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
        let (p, stop, idr, stats, src, live, inval) = (a.params.clone(), stop.clone(), idr.clone(), stats.clone(), a.video, a.live.clone(), invalidate.clone());
        threads.push(std::thread::Builder::new().name("video-enc".into()).spawn(move || {
            if let Err(e) = video_loop(p, src, sock, vpeer, stop, idr, inval, stats, live.clone()) {
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
    Ok(StreamHandle { stop, idr, invalidate, stats, threads })
}

#[allow(clippy::too_many_arguments)]
fn video_loop(p: StreamParams, mut src: Box<dyn VideoSource>, sock: UdpSocket, peer: Arc<Peer>, stop: Arc<AtomicBool>, idr: Arc<AtomicBool>, invalidate: Arc<Mutex<Option<(u32, u32)>>>, stats: Arc<StreamStats>, live: Arc<Mutex<LiveStats>>) -> Result<()> {
    let fps = p.fps.clamp(1, 240);
    let mut enc = VideoEncoder::open(
        &p.encoder,
        Codec::from_format(p.video_format),
        p.width as usize,
        p.height as usize,
        fps,
        p.bitrate_kbps,
        // maxNumReferenceFrames=0: the client uses RFI and lets us keep several references.
        &EncoderOptions { max_refs: if p.max_ref_frames == 0 { 1 } else { p.max_ref_frames }, csc: Csc::from_mode(p.csc_mode), preset: p.nvenc_preset.clone(), rfi: p.max_ref_frames == 0, yuv444: p.yuv444 && !p.hdr, hdr10: p.hdr && Codec::from_format(p.video_format) != Codec::H264 },
    )?;
    let mut pk = Packetizer::new(p.packet_size as usize, p.fec_percent, p.min_fec_packets as usize);
    // 4:4:4 only if the encoder could really be opened that way.
    let mut frame = I420Frame::with_format(p.width as usize, p.height as usize, enc.yuv444());
    let mut frame10 = enc.hdr10().then(|| moon_media::hdr::P010Frame::new(p.width as usize, p.height as usize));
    frame.csc = Csc::from_mode(p.csc_mode);
    *live.lock().unwrap() = LiveStats {
        active: true,
        width: p.width,
        height: p.height,
        target_fps: fps,
        bitrate_kbps: p.bitrate_kbps,
        codec: format!("{:?}", Codec::from_format(p.video_format)),
        encoder: enc.name().to_string(),
        source: src.name().to_string(),
        audio_channels: p.audio_channels,
        ..Default::default()
    };
    let mut windows = 0u64;
    let (mut win_start, mut win_frames, mut win_bytes, mut win_idr, mut win_new) = (Instant::now(), 0u64, 0u64, 0u64, 0u64);
    let period = Duration::from_secs_f64(1.0 / fps as f64);
    let (tx, rx) = std::sync::mpsc::sync_channel::<(SocketAddr, Vec<Vec<u8>>)>(2);
    let sender = {
        let sock = sock.try_clone()?;
        let kbps = p.bitrate_kbps;
        std::thread::Builder::new().name("video-pacer".into()).spawn(move || pacer(sock, rx, period, kbps))?
    };
    // GPU path, as Sunshine: DMA-BUF capture -> OpenGL conversion -> NVENC CUDA input.
    // Declared after `enc` so it is dropped first (it uses the encoder's CUDA context).
    let mut gpu: Option<(GpuConverter, CudaInput)> = None;
    let mut dmabuf_formats = Vec::new();
    // MOON_CPU_CONVERT=1 forces the former path (memory buffers, CPU conversion).
    if frame10.is_none() && std::env::var_os("MOON_CPU_CONVERT").is_none() {
        if let Some(nv) = enc.nvenc_mut() {
            match nv.cuda_input().and_then(|input| GpuConverter::new().map(|c| (c, input))) {
                Ok((c, input)) => {
                    dmabuf_formats = [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888].into_iter().map(|f| (f, c.modifiers(f))).filter(|(_, m)| !m.is_empty()).collect();
                    gpu = Some((c, input));
                }
                Err(e) => warn!("GPU conversion unavailable, converting on the CPU: {e:#}"),
            }
        }
    }
    src.start(fps, &dmabuf_formats)?;
    let path = if gpu.is_some() && src.is_dmabuf() { "DMA-BUF, GPU conversion" } else { "memory copies, CPU conversion" };
    let start = Instant::now();
    let mut index: u32 = 0;
    info!("video: {}x{}@{} {} kbps {} from {} ({path}) via {}", p.width, p.height, fps, p.bitrate_kbps, if enc.hdr10() { "10-bit HDR10" } else if enc.yuv444() { "4:4:4" } else { "4:2:0" }, src.name(), enc.name());
    // Each new picture is encoded as soon as it arrives (Sunshine samples on a steady
    // clock instead, which makes a picture wait half a frame interval on average).
    // When the compositor may send pictures faster than the client's rate (KWin's
    // virtual output at 120 Hz for a 60 fps stream), at most `fps` pictures a second
    // are encoded: one that comes too soon after the last encode waits for the next.
    // A static screen is re-sent a few times a second so losses still get repaired.
    let fast_source = src.source_fps().is_some_and(|r| r == 0.0 || r > fps as f64 * 1.05);
    info!("video pacing: each picture as it arrives{}", if fast_source { format!(", at most {fps} per second (compositor up to {:.0} fps)", src.source_fps().unwrap_or(0.0)) } else { String::new() });
    let idle_repeat = Duration::from_millis(100).max(period);
    let mut last_encode = Instant::now() - period;
    let mut grid = EncodeGrid::new(period, Instant::now());
    let mut received_at_window = src.pictures_received();
    let mut have_picture = false;
    let mut generation = u64::MAX;
    while !stop.load(Ordering::Relaxed) {
        let new = if fast_source {
            // One encode per slot of a steady `fps` grid (see `EncodeGrid`).
            let earliest = grid.earliest();
            let now = Instant::now();
            if earliest > now {
                std::thread::sleep(earliest - now);
            }
            // An older picture is the compositor's extra one: prefer the next, fresher
            // picture if it comes before the slot's deadline.
            if src.wait_arrival_after(grid.fresh(), grid.deadline()) || src.pending_arrival().is_some() {
                grid.encoded(Instant::now());
                true
            } else {
                // Nothing new: wait for the next picture, re-sending the last one now and then.
                let new = src.wait_frame(last_encode + idle_repeat, last_encode + idle_repeat);
                if !new && have_picture && last_encode.elapsed() < idle_repeat {
                    continue;
                }
                grid.encoded(Instant::now());
                new
            }
        } else {
            let earliest = last_encode + period.mul_f64(0.75);
            let now = Instant::now();
            if earliest > now {
                std::thread::sleep(earliest - now);
            }
            src.wait_frame(last_encode + period, last_encode + idle_repeat)
        };
        if !new && !have_picture {
            continue;
        }
        last_encode = Instant::now();
        let Some(dest) = peer.get() else { continue };
        let t0 = Instant::now();
        // Host latency reported to the client: from the picture's arrival, not just the encode.
        let arrival = if new { src.pending_arrival().filter(|a| *a <= t0) } else { None }.unwrap_or(t0);
        let use_gpu = gpu.is_some() && src.is_dmabuf();
        if use_gpu {
            if new {
                let (conv, input) = gpu.as_mut().unwrap();
                let csc = frame.csc;
                let r = src.with_dmabuf(&mut |f, g| {
                    if g != generation {
                        conv.forget_buffers();
                        generation = g;
                    }
                    conv.convert(f, input, csc)
                });
                match r {
                    Some(Ok(())) => have_picture = true,
                    Some(Err(e)) => {
                        // Cannot read this kind of buffer: capture into memory instead.
                        warn!("GPU conversion failed, switching to CPU conversion: {e:#}");
                        gpu = None;
                        have_picture = false;
                        src.start(fps, &[])?;
                        continue;
                    }
                    None => {}
                }
            }
            if !have_picture {
                continue;
            }
        } else {
            match frame10.as_mut() {
                Some(f) => src.grab_hdr10(f, p.width as usize, p.height as usize)?,
                None => src.grab(&mut frame, p.width as usize, p.height as usize)?,
            }
            have_picture = true;
        }
        if idr.swap(false, Ordering::Relaxed) {
            enc.request_idr();
        }
        // Reference frame invalidation: everything from the first lost frame up to
        // the last one we sent references lost data on the client.
        let mut frame_type = None;
        if let Some((first, _last)) = invalidate.lock().unwrap().take() {
            if (index.wrapping_sub(first) as i32) >= 0 && enc.invalidate(first, index) {
                debug!("invalidated frames {first}..={index}");
                frame_type = Some(FRAME_TYPE_RFI_RECOVERY);
                stats.rfi.fetch_add(1, Ordering::Relaxed);
            } else {
                enc.request_idr();
            }
        }
        let ts = (start.elapsed().as_micros() * 90 / 1000) as u32;
        let e = match (use_gpu, frame10.as_ref()) {
            (true, _) => enc.nvenc_mut().expect("GPU path needs direct NVENC").encode_cuda(index.wrapping_add(1) as u64)?,
            (false, Some(f)) => enc.encode_p010(f, index.wrapping_add(1))?,
            (false, None) => enc.encode(&frame, ts as i64, index.wrapping_add(1))?,
        };
        if e.data.is_empty() {
            continue;
        }
        index = index.wrapping_add(1);
        let elapsed = t0.elapsed();
        pk.processing_latency = (arrival.elapsed().as_micros() / 100).min(u16::MAX as u128) as u16;
        let is_idr = e.idr;
        let ftype = if is_idr { FRAME_TYPE_IDR } else { frame_type.unwrap_or(FRAME_TYPE_P) };
        let pkts = pk.packetize_typed(index, ts, ftype, e.data);
        stats.encode_us.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
        stats.frames.fetch_add(1, Ordering::Relaxed);
        if is_idr {
            stats.idr_frames.fetch_add(1, Ordering::Relaxed);
        }
        win_frames += 1;
        win_new += new as u64;
        win_idr += is_idr as u64;
        win_bytes += pkts.iter().map(|p| p.len() as u64).sum::<u64>();
        if win_start.elapsed() >= Duration::from_secs(1) {
            let secs = win_start.elapsed().as_secs_f64();
            {
                let mut l = live.lock().unwrap();
                l.fps = win_frames as f64 / secs;
                l.mbps = win_bytes as f64 * 8.0 / secs / 1e6;
                l.encode_ms = stats.encode_us.load(Ordering::Relaxed) as f64 / stats.frames.load(Ordering::Relaxed).max(1) as f64 / 1000.0;
                l.frames = stats.frames.load(Ordering::Relaxed);
                l.idr_frames = stats.idr_frames.load(Ordering::Relaxed);
                l.peer = Some(dest.to_string());
            }
            windows += 1;
            let received = src.pictures_received();
            let source = match (received, received_at_window) {
                (Some(r), Some(r0)) => format!(", compositor {:.1}/s", r.saturating_sub(r0) as f64 / secs),
                _ => String::new(),
            };
            received_at_window = received;
            if windows % 5 == 0 { info!(
                "video: {:.1} fps ({:.1} new pictures/s{source}), {:.1} Mbit/s, {} IDR, encode avg {:.2} ms",
                win_frames as f64 / secs,
                win_new as f64 / secs,
                win_bytes as f64 * 8.0 / secs / 1e6,
                win_idr,
                stats.encode_us.load(Ordering::Relaxed) as f64 / stats.frames.load(Ordering::Relaxed).max(1) as f64 / 1000.0
            ); }
            (win_start, win_frames, win_bytes, win_idr, win_new) = (Instant::now(), 0, 0, 0, 0);
        }
        stats.bytes.fetch_add(pkts.iter().map(|p| p.len() as u64).sum::<u64>(), Ordering::Relaxed);
        // Hand the frame to the pacing thread (bounded queue: if the network is
        // slower than the encoder, the encoder waits instead of piling up latency).
        if tx.send((dest, pkts)).is_err() {
            break;
        }
    }
    drop(tx);
    let _ = sender.join();
    live.lock().unwrap().active = false;
    Ok(())
}

/// Packet pacer: sends each frame at a steady rate well above the stream's bitrate
/// instead of in one burst, which avoids overflowing switch / Wi-Fi / client socket
/// buffers, without holding the frame back: the client can only decode it once the
/// last packet is in. (Spreading every frame over a fixed share of the frame interval
/// delayed a 40 KB frame by ~10 ms at 60 fps where 1 ms is enough.)
/// Encode slots on a steady `fps` grid, for a compositor that sends pictures faster
/// than the stream's rate (KWin's virtual output at 120 Hz, 90-110 pictures a second,
/// irregular, for a 60 fps stream). Each slot takes the freshest picture around it.
/// Spacing encodes from the previous encode instead lost rate: with pictures every
/// ~10 ms, gaps alternated 16 and 20 ms and the stream got ~55 fps out of a 100 fps
/// source. On a grid, a picture taken late in its slot does not delay the next slot.
struct EncodeGrid {
    period: Duration,
    /// Time of the next slot.
    next: Instant,
}

impl EncodeGrid {
    fn new(period: Duration, now: Instant) -> Self {
        Self { period, next: now }
    }

    /// No encode before this.
    fn earliest(&self) -> Instant {
        self.next - self.period.mul_f64(0.3)
    }

    /// A picture that arrived before this belongs to the previous slot.
    fn fresh(&self) -> Instant {
        self.next - self.period.mul_f64(0.4)
    }

    /// Wait for a fresh picture until then, else take what is there.
    fn deadline(&self) -> Instant {
        self.next + self.period.mul_f64(0.25)
    }

    /// A picture was encoded at `now`: move to the next slot. After a pause (static
    /// screen) the grid restarts from `now` rather than catching up with a burst.
    fn encoded(&mut self, now: Instant) {
        self.next += self.period;
        if self.next + self.period.mul_f64(0.5) < now {
            self.next = now + self.period;
        }
    }
}

fn pacer(sock: UdpSocket, rx: std::sync::mpsc::Receiver<(SocketAddr, Vec<Vec<u8>>)>, period: Duration, bitrate_kbps: u32) {
    const BATCH: usize = 8;
    // Never spread a frame over more than 60% of the interval (huge IDR frames).
    let window = period.mul_f64(0.6);
    // Sending rate: 300 Mbit/s, or 3x the stream's bitrate if higher (MOON_PACING_MBPS overrides).
    let mbps = std::env::var("MOON_PACING_MBPS").ok().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0).unwrap_or_else(|| (bitrate_kbps as f64 * 3.0 / 1000.0).max(300.0));
    let bytes_per_sec = mbps * 1e6 / 8.0;
    info!("video packet pacing: {mbps:.0} Mbit/s, at most {:.1} ms per frame", window.as_secs_f64() * 1000.0);
    while let Ok((dest, pkts)) = rx.recv() {
        let n = pkts.len();
        let start = Instant::now();
        let mut bytes = 0usize;
        for (i, chunk) in pkts.chunks(BATCH).enumerate() {
            for pkt in chunk {
                bytes += pkt.len();
                if let Err(e) = sock.send_to(pkt, dest) {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        debug!("video send: {e}");
                    }
                }
            }
            let sent = ((i + 1) * BATCH).min(n);
            if n > BATCH * 2 && sent < n {
                let at_rate = Duration::from_secs_f64(bytes as f64 / bytes_per_sec);
                let due = start + at_rate.min(window.mul_f64(sent as f64 / n as f64));
                let now = Instant::now();
                if due > now + Duration::from_micros(100) {
                    std::thread::sleep(due - now);
                }
            }
        }
    }
}


fn audio_loop(p: StreamParams, mut src: Box<dyn AudioSource>, sock: UdpSocket, peer: Arc<Peer>, cipher: Option<AudioCipher>, stop: Arc<AtomicBool>, stats: Arc<StreamStats>) -> Result<()> {
    let ms = p.audio_packet_ms.clamp(5, 20);
    let layout = StreamLayout::for_channels(p.audio_channels);
    let ch = layout.channels as usize;
    // ~128 kbps per stereo pair
    let mut enc = OpusEncoder::new(layout, 48000, ms, 64_000 * ch as i32, true)?;
    let mut pk = AudioPacketizer::new(ms);
    let encrypted = cipher.is_some();
    pk.cipher = cipher;
    let mut pcm = vec![0i16; enc.frame_size * ch];
    let mut opus = Vec::new();
    info!("audio: {ch}-channel opus {ms} ms packets{}", if encrypted { ", encrypted" } else { "" });
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

#[cfg(test)]
mod grid_tests {
    use super::*;

    /// Simulates the video loop over a source sending `arrivals` (ms from start);
    /// returns the encode times (ms).
    fn simulate(arrivals: &[f64], fps: f64, until: f64) -> Vec<f64> {
        let t0 = Instant::now();
        let ms = |i: Instant| i.saturating_duration_since(t0).as_secs_f64() * 1000.0;
        let at = |m: f64| t0 + Duration::from_secs_f64(m / 1000.0);
        let mut grid = EncodeGrid::new(Duration::from_secs_f64(1.0 / fps), t0);
        let (mut now, mut used, mut out) = (0.0f64, -1.0f64, Vec::new());
        while now < until {
            now = now.max(ms(grid.earliest()));
            let (fresh, deadline) = (ms(grid.fresh()), ms(grid.deadline()));
            // Newest picture not used yet, and the first fresh one.
            let latest_by = |t: f64| arrivals.iter().copied().filter(|&a| a > used && a <= t).last();
            let wait_fresh = arrivals.iter().copied().find(|&a| a > used && a >= fresh && a <= deadline);
            let take = match wait_fresh {
                Some(a) if a <= now => latest_by(now),
                Some(a) => {
                    now = a;
                    Some(a)
                }
                None => latest_by(deadline.max(now)).map(|a| {
                    now = now.max(deadline);
                    a
                }),
            };
            match take {
                Some(a) => {
                    used = a;
                    out.push(now);
                    grid.encoded(at(now));
                }
                None => now += 1.0,
            }
        }
        out
    }

    #[test]
    fn full_rate_from_an_irregular_fast_source() {
        // ~100 pictures a second with jitter, like KWin's virtual output at 120 Hz.
        let mut arrivals = Vec::new();
        let mut t = 0.0;
        let mut k = 0u32;
        while t < 10_000.0 {
            arrivals.push(t);
            k = k.wrapping_mul(1103515245).wrapping_add(12345);
            t += 8.3 + (k >> 16) as f64 % 4.0;
        }
        let enc = simulate(&arrivals, 60.0, 10_000.0);
        let fps = enc.len() as f64 / 10.0;
        assert!(fps >= 59.5 && fps <= 60.5, "{fps} fps");
    }

    #[test]
    fn no_burst_after_a_pause() {
        let mut arrivals: Vec<f64> = (0..60).map(|i| i as f64 * 8.33).collect();
        arrivals.extend((0..120).map(|i| 2000.0 + i as f64 * 8.33));
        let enc = simulate(&arrivals, 60.0, 3000.0);
        let after: Vec<f64> = enc.iter().copied().filter(|&t| t >= 2000.0).collect();
        let min_gap = after.windows(2).map(|w| w[1] - w[0]).fold(f64::MAX, f64::min);
        assert!(min_gap >= 10.0, "encodes {min_gap} ms apart after the pause");
    }
}
