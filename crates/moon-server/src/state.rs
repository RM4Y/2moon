//! Host state: identity, paired clients, pending pairings and the active session.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use moon_proto::crypto::Identity;
use moon_proto::pairing::ServerPairing;
use moon_proto::xml::App;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{oneshot, watch};

use crate::capture::{AudioSource, SineSource, TestPattern, VideoSource};
use crate::stream::{self, StartArgs, StreamHandle, StreamParams, StreamSockets};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    pub name: String,
    pub cert_pem: String,
    #[serde(skip)]
    pub cert_der: Vec<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    uuid: String,
    clients: Vec<PairedClient>,
}

/// Parameters negotiated by `/launch` (and later refined by RTSP ANNOUNCE).
#[derive(Debug, Clone)]
pub struct LaunchSession {
    pub app_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rikey: [u8; 16],
    pub rikeyid: u32,
    pub client_ip: std::net::IpAddr,
    pub ss_ping_payload: [u8; 16],
    pub control_connect_data: u32,
}

pub struct Config {
    pub hostname: String,
    pub http_port: u16,
    pub https_port: u16,
    pub rtsp_port: u16,
    pub video_port: u16,
    pub control_port: u16,
    pub audio_port: u16,
    /// Client microphone (2moon extension); None when disabled.
    pub mic_port: Option<u16>,
    pub state_dir: PathBuf,
    pub preset_pin: Option<String>,
    pub apps: Vec<App>,
    pub video_source: VideoSourceKind,
    pub audio_source: AudioSourceKind,
    pub encoder: String,
    pub nvenc_preset: String,
    /// Output captured with --video kwin (name substring; empty = first).
    pub kwin_output: String,
    /// Codecs the host can encode (filled at startup by probing).
    pub hevc: bool,
    pub av1: bool,
    /// Reference frame invalidation available (direct NVENC).
    pub rfi: bool,
    /// 4:4:4 encoding available for H.264 / HEVC (direct NVENC).
    pub yuv444_h264: bool,
    pub yuv444_hevc: bool,
    /// 10-bit HDR10 encoding available (direct NVENC).
    pub hdr10_hevc: bool,
    pub hdr10_av1: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum VideoSourceKind {
    /// Animated synthetic test pattern.
    Test,
    /// The desktop, through the XDG ScreenCast portal + PipeWire (Wayland, asks once).
    Portal,
    /// An existing PipeWire video node (see --pw-node); no dialog, works headless.
    Pipewire,
    /// KDE Plasma: dedicated KWin screencast of an output (see --output); no dialog, headless.
    Kwin,
    /// Windows: a display through DXGI Desktop Duplication (see --output).
    Dxgi,
    /// macOS: a display through ScreenCaptureKit (see --output: 1 = first display; empty = main).
    #[cfg(target_os = "macos")]
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AudioSourceKind {
    /// No audio stream.
    None,
    /// 440 Hz test tone.
    Sine,
    /// What the computer is playing (default sink monitor).
    Desktop,
}

impl Config {
    pub fn with_base_port(base: u16, state_dir: PathBuf, hostname: String) -> Self {
        // Default GameStream ports: 47989 HTTP, 47984 HTTPS, 48010 RTSP, 47998 video,
        // 47999 control, 48000 audio. They are all derived from the HTTP port.
        Self {
            hostname,
            http_port: base,
            https_port: base - 5,
            rtsp_port: base + 21,
            video_port: base + 9,
            control_port: base + 10,
            audio_port: base + 11,
            mic_port: Some(base + 12),
            state_dir,
            preset_pin: None,
            apps: vec![App { id: 1, title: "Desktop".into(), hdr_supported: false }],
            video_source: VideoSourceKind::Test,
            audio_source: AudioSourceKind::Sine,
            encoder: "auto".into(),
            nvenc_preset: "p4".into(),
            kwin_output: String::new(),
            hevc: false,
            av1: false,
            rfi: false,
            yuv444_h264: false,
            yuv444_hevc: false,
            hdr10_hevc: false,
            hdr10_av1: false,
        }
    }
}

pub struct PendingPair {
    pub pairing: ServerPairing,
    pub device_name: String,
}

pub struct Host {
    pub cfg: Config,
    pub identity: Identity,
    pub uuid: String,
    paired: Mutex<Vec<PairedClient>>,
    pub pending: Mutex<HashMap<String, PendingPair>>,
    /// Pairing requests waiting for the operator to type a PIN.
    pub pin_waiters: Mutex<Vec<oneshot::Sender<String>>>,
    pub session: Mutex<Option<LaunchSession>>,
    /// Bumped whenever the session changes, so streaming tasks can stop.
    pub session_gen: watch::Sender<u64>,
    pub stream_peer: Mutex<Option<SocketAddr>>,
    params: Mutex<StreamParams>,
    stream: Mutex<Option<StreamHandle>>,
    mic_socket: Option<Arc<std::net::UdpSocket>>,
    #[cfg(target_os = "linux")]
    mic: Mutex<Option<crate::mic::MicReceiver>>,
    sockets: Arc<StreamSockets>,
    pub input_events: AtomicU64,
    #[cfg(target_os = "linux")]
    pub portal: std::sync::OnceLock<Arc<crate::portal::PortalCapture>>,
    pub live: Arc<Mutex<crate::stream::LiveStats>>,
    /// Pairing requests waiting for a PIN: (device name, address).
    pub pending_requests: Mutex<Vec<(String, String)>>,
    pub started_at: std::time::Instant,
    /// Applications offered to clients (editable from the admin panel).
    pub apps: crate::apps::Apps,
    /// Settings edited from the admin panel.
    pub settings: crate::settings::Settings,
    /// Remote access can be used (not disabled with --no-remote).
    pub remote_available: std::sync::atomic::AtomicBool,
    pub remote_status: Mutex<crate::remote::Status>,
    remote_code: Mutex<Option<String>>,
}

impl Host {
    pub fn load(cfg: Config) -> Result<Arc<Self>> {
        let (cfg_video_port, cfg_audio_port) = (cfg.video_port, cfg.audio_port);
        // The client microphone needs a virtual microphone (PipeWire, Linux only).
        #[cfg(target_os = "linux")]
        let mic_socket = match cfg.mic_port.map(crate::mic::bind).transpose() {
            Ok(s) => s.map(Arc::new),
            Err(e) => {
                tracing::warn!("client microphone disabled: {e:#}");
                None
            }
        };
        #[cfg(not(target_os = "linux"))]
        let mic_socket = None;
        let identity = Identity::load_or_generate(&cfg.state_dir, "host", "2moon GameStream Host")?;
        let path = cfg.state_dir.join("state.json");
        let mut persisted: Persisted = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)?,
            Err(_) => Persisted::default(),
        };
        if persisted.uuid.is_empty() {
            persisted.uuid = uuid_v4();
        }
        for c in &mut persisted.clients {
            c.cert_der = moon_proto::crypto::pem_decode_cert(&c.cert_pem)?;
        }
        let apps = crate::apps::Apps::load(&cfg.state_dir, &cfg.apps);
        let cfg_state_dir = cfg.state_dir.clone();
        let host = Arc::new(Self {
            cfg,
            identity,
            uuid: persisted.uuid.clone(),
            paired: Mutex::new(persisted.clients),
            pending: Mutex::new(HashMap::new()),
            pin_waiters: Mutex::new(Vec::new()),
            session: Mutex::new(None),
            session_gen: watch::channel(0).0,
            stream_peer: Mutex::new(None),
            params: Mutex::new(StreamParams::default()),
            stream: Mutex::new(None),
            mic_socket,
            #[cfg(target_os = "linux")]
            mic: Mutex::new(None),
            sockets: Arc::new(StreamSockets::bind(cfg_video_port, cfg_audio_port)?),
            input_events: AtomicU64::new(0),
            #[cfg(target_os = "linux")]
            portal: std::sync::OnceLock::new(),
            live: Arc::new(Mutex::new(Default::default())),
            pending_requests: Mutex::new(Vec::new()),
            started_at: std::time::Instant::now(),
            apps,
            settings: crate::settings::Settings::load(&cfg_state_dir),
            remote_available: std::sync::atomic::AtomicBool::new(false),
            remote_status: Mutex::new(Default::default()),
            remote_code: Mutex::new(None),
        });
        host.save()?;
        Ok(host)
    }

    fn save(&self) -> Result<()> {
        let p = Persisted { uuid: self.uuid.clone(), clients: self.paired.lock().unwrap().clone() };
        std::fs::create_dir_all(&self.cfg.state_dir)?;
        std::fs::write(self.cfg.state_dir.join("state.json"), serde_json::to_string_pretty(&p)?)?;
        Ok(())
    }

    pub fn is_paired(&self, cert_der: &[u8]) -> bool {
        self.paired.lock().unwrap().iter().any(|c| c.cert_der == cert_der)
    }

    pub fn add_paired(&self, name: String, cert_der: Vec<u8>) -> Result<()> {
        {
            let mut p = self.paired.lock().unwrap();
            p.retain(|c| c.cert_der != cert_der);
            p.push(PairedClient { name, cert_pem: moon_proto::crypto::pem_encode_cert(&cert_der), cert_der });
        }
        self.save()
    }

    pub fn remove_paired(&self, cert_der: &[u8]) -> Result<()> {
        self.paired.lock().unwrap().retain(|c| c.cert_der != cert_der);
        self.save()
    }

    pub fn paired_clients(&self) -> Vec<PairedClient> {
        self.paired.lock().unwrap().clone()
    }

    /// Deliver a PIN to every pairing request currently waiting for one.
    /// SHA-256 fingerprint (hex) identifying a paired client.
    pub fn fingerprint(cert_der: &[u8]) -> String {
        hex::encode(moon_proto::crypto::sha256(&[cert_der]))
    }

    pub fn unpair_fingerprint(&self, fp: &str) -> Result<bool> {
        let cert = self.paired.lock().unwrap().iter().find(|c| Self::fingerprint(&c.cert_der) == fp).map(|c| c.cert_der.clone());
        match cert {
            Some(c) => {
                self.remove_paired(&c)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn submit_pin(&self, pin: &str) -> usize {
        let waiters: Vec<_> = self.pin_waiters.lock().unwrap().drain(..).collect();
        let n = waiters.len();
        for w in waiters {
            let _ = w.send(pin.to_string());
        }
        n
    }

    pub fn set_session(&self, s: Option<LaunchSession>) {
        self.stop_stream();
        // Session over: the captured display gets its own mode back.
        if s.is_none() {
            tokio::task::block_in_place(crate::display::restore);
        }
        *self.session.lock().unwrap() = s;
        *self.stream_peer.lock().unwrap() = None;
        self.session_gen.send_modify(|g| *g += 1);
    }

    /// Name shown to clients (admin panel setting, else --name / system host name).
    /// The remote access code, when remote access is on.
    pub fn remote_id(&self) -> Option<String> {
        if !self.remote_available.load(Ordering::Relaxed) || !self.settings.get().remote {
            return None;
        }
        let mut code = self.remote_code.lock().unwrap();
        if code.is_none() {
            *code = crate::remote::load_or_create_key(&self.cfg.state_dir).ok().map(|k| moon_tunnel::id_of(&k).to_string());
        }
        code.clone()
    }

    /// Forget the cached code (after a new key was written).
    pub fn reset_remote_code(&self) {
        *self.remote_code.lock().unwrap() = None;
    }

    pub fn hostname(&self) -> String {
        let n = self.settings.get().host_name;
        if n.is_empty() { self.cfg.hostname.clone() } else { n }
    }

    /// NVENC preset for the next stream (admin panel setting, else --nvenc-preset).
    pub fn nvenc_preset(&self) -> String {
        let p = self.settings.get().nvenc_preset;
        if p.is_empty() { self.cfg.nvenc_preset.clone() } else { p }
    }

    pub fn current_session(&self) -> Option<LaunchSession> {
        self.session.lock().unwrap().clone()
    }

    pub fn set_stream_params(&self, p: StreamParams) {
        *self.params.lock().unwrap() = p;
    }

    pub fn stream_params(&self) -> StreamParams {
        self.params.lock().unwrap().clone()
    }

    /// Start (or restart) the media pipelines for the current session.
    pub fn start_stream(&self) -> Result<()> {
        let session = self.current_session().ok_or_else(|| anyhow::anyhow!("no session"))?;
        self.stop_stream();
        // Native resolution: the display takes the client's mode before the capture starts.
        if self.native_resolution_available() && self.settings.get().native_resolution {
            let p = self.stream_params();
            tokio::task::block_in_place(|| crate::display::apply(&self.cfg.kwin_output, p.width, p.height, p.fps));
        }
        let video: Box<dyn VideoSource> = match self.cfg.video_source {
            VideoSourceKind::Test => Box::new(TestPattern::new()),
            #[cfg(target_os = "linux")]
            VideoSourceKind::Kwin => Box::new(crate::portal::PipeWireSource::kwin(self.cfg.kwin_output.clone())),
            #[cfg(target_os = "linux")]
            VideoSourceKind::Portal | VideoSourceKind::Pipewire => {
                let cap = self.portal.get().ok_or_else(|| anyhow::anyhow!("screen capture is not initialised"))?;
                Box::new(crate::portal::PipeWireSource::new(cap.clone()))
            }
            #[cfg(windows)]
            VideoSourceKind::Dxgi => Box::new(crate::win::capture::DxgiSource::new(self.cfg.kwin_output.clone())),
            #[cfg(target_os = "macos")]
            VideoSourceKind::Screen => Box::new(crate::mac::capture::ScreenSource::new(self.cfg.kwin_output.clone())),
            #[allow(unreachable_patterns)]
            other => anyhow::bail!("video source {other:?} is not available on this system"),
        };
        let ch = match self.stream_params().audio_channels {
            6 => 6,
            8 => 8,
            _ => 2,
        };
        let audio: Option<Box<dyn AudioSource>> = match self.cfg.audio_source {
            AudioSourceKind::None => None,
            AudioSourceKind::Sine => Some(Box::new(SineSource::new(48000, ch as usize))),
            #[cfg(target_os = "linux")]
            AudioSourceKind::Desktop => match crate::capture::PulseMonitor::new(48000, ch) {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    tracing::warn!("desktop audio capture unavailable: {e:#}");
                    None
                }
            },
            #[cfg(windows)]
            AudioSourceKind::Desktop => match crate::win::audio::LoopbackSource::new(48000, ch) {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    tracing::warn!("desktop audio capture unavailable: {e:#}");
                    None
                }
            },
            #[cfg(target_os = "macos")]
            AudioSourceKind::Desktop => match crate::mac::audio::ScreenAudio::new(48000, ch) {
                Ok(m) => Some(Box::new(m)),
                Err(e) => {
                    tracing::warn!("desktop audio capture unavailable: {e:#}");
                    None
                }
            },
        };
        let h = stream::start(StartArgs {
            params: self.stream_params(),
            sockets: self.sockets.clone(),
            ping_payload: session.ss_ping_payload,
            client_ip: session.client_ip,
            rikey: session.rikey,
            rikeyid: session.rikeyid,
            video,
            audio,
            live: self.live.clone(),
        })?;
        *self.stream.lock().unwrap() = Some(h);
        #[cfg(target_os = "linux")]
        if let Some(sock) = &self.mic_socket {
            match crate::mic::MicReceiver::start(sock.clone(), session.client_ip, session.rikey, session.rikeyid) {
                Ok(m) => *self.mic.lock().unwrap() = Some(m),
                Err(e) => tracing::warn!("client microphone: {e:#}"),
            }
        }
        Ok(())
    }

    /// The display can follow the client's resolution: a real screen capture (not the test pattern).
    pub fn native_resolution_available(&self) -> bool {
        self.cfg.video_source != VideoSourceKind::Test
    }

    /// The current stream is HDR10 (client asked, and the host can encode it).
    pub fn hdr_active(&self) -> bool {
        let p = self.stream_params();
        p.hdr
            && match moon_media::encoder::Codec::from_format(p.video_format) {
                moon_media::encoder::Codec::Hevc => self.cfg.hdr10_hevc,
                moon_media::encoder::Codec::Av1 => self.cfg.hdr10_av1,
                moon_media::encoder::Codec::H264 => false,
            }
    }

    /// Whether the client microphone extension is offered (its port).
    pub fn mic_port(&self) -> Option<u16> {
        self.mic_socket.as_ref().and(self.cfg.mic_port)
    }

    pub fn stop_stream(&self) {
        // Removes the virtual microphone and restores the default input.
        #[cfg(target_os = "linux")]
        drop(self.mic.lock().unwrap().take());
        let h = self.stream.lock().unwrap().take();
        if let Some(h) = h {
            let st = h.stats.clone();
            h.stop();
            let frames = st.frames.load(Ordering::Relaxed);
            tracing::info!(
                "stream stopped: {frames} frames ({} IDR, {} RFI recoveries), {:.1} MB, avg encode {:.2} ms, {} audio packets",
                st.idr_frames.load(Ordering::Relaxed),
                st.rfi.load(Ordering::Relaxed),
                st.bytes.load(Ordering::Relaxed) as f64 / 1e6,
                if frames > 0 { st.encode_us.load(Ordering::Relaxed) as f64 / frames as f64 / 1000.0 } else { 0.0 },
                st.audio_packets.load(Ordering::Relaxed)
            );
        }
    }

    /// The client lost frames `first..=last` (RFI): invalidate them as references.
    pub fn invalidate_ref_frames(&self, first: u32, last: u32) {
        if let Some(h) = self.stream.lock().unwrap().as_ref() {
            let mut p = h.invalidate.lock().unwrap();
            // Merge with a pending request: keep the oldest first frame.
            *p = Some(match *p {
                Some((f, l)) => (if (first.wrapping_sub(f) as i32) < 0 { first } else { f }, last.max(l)),
                None => (first, last),
            });
        }
    }

    pub fn request_idr(&self) {
        if let Some(h) = self.stream.lock().unwrap().as_ref() {
            h.idr.store(true, Ordering::Relaxed);
        }
    }
}

fn uuid_v4() -> String {
    let mut b: [u8; 16] = moon_proto::crypto::random_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode_upper(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Bind a TCP listener on all IPv4 and IPv6 addresses.
pub async fn listen_any(port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let bind = || -> std::io::Result<tokio::net::TcpListener> {
        let sock = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        sock.set_only_v6(false)?;
        // Unix: rebind at once after a restart. Not on Windows, where SO_REUSEADDR lets
        // another program take over a port that is in use.
        #[cfg(not(windows))]
        sock.set_reuse_address(true)?;
        sock.bind(&std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into())?;
        sock.listen(128)?;
        sock.set_nonblocking(true)?;
        tokio::net::TcpListener::from_std(sock.into())
    };
    bind().map_err(|e| anyhow::anyhow!("cannot use TCP port {port} ({e}); is another streaming host running?"))
}
