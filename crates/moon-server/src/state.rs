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

use crate::capture::{AudioSource, PulseMonitor, SineSource, TestPattern, VideoSource};
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
    pub surround_info: u32,
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
    pub state_dir: PathBuf,
    pub preset_pin: Option<String>,
    pub apps: Vec<App>,
    pub video_source: VideoSourceKind,
    pub audio_source: AudioSourceKind,
    pub encoder: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum VideoSourceKind {
    /// Animated synthetic test pattern.
    Test,
    /// The desktop, through the XDG ScreenCast portal + PipeWire (Wayland).
    Portal,
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
            state_dir,
            preset_pin: None,
            apps: vec![App { id: 1, title: "Desktop".into(), hdr_supported: false }],
            video_source: VideoSourceKind::Test,
            audio_source: AudioSourceKind::Sine,
            encoder: "auto".into(),
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
    sockets: Arc<StreamSockets>,
    pub input_events: AtomicU64,
    pub portal: std::sync::OnceLock<Arc<crate::portal::PortalCapture>>,
}

impl Host {
    pub fn load(cfg: Config) -> Result<Arc<Self>> {
        let (cfg_video_port, cfg_audio_port) = (cfg.video_port, cfg.audio_port);
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
            sockets: Arc::new(StreamSockets::bind(cfg_video_port, cfg_audio_port)?),
            input_events: AtomicU64::new(0),
            portal: std::sync::OnceLock::new(),
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
        *self.session.lock().unwrap() = s;
        *self.stream_peer.lock().unwrap() = None;
        self.session_gen.send_modify(|g| *g += 1);
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
        let video: Box<dyn VideoSource> = match self.cfg.video_source {
            VideoSourceKind::Test => Box::new(TestPattern::new()),
            VideoSourceKind::Portal => {
                let cap = self.portal.get().ok_or_else(|| anyhow::anyhow!("screen capture is not initialised"))?;
                Box::new(crate::portal::PipeWireSource::new(cap.clone()))
            }
        };
        let audio: Option<Box<dyn AudioSource>> = match self.cfg.audio_source {
            AudioSourceKind::None => None,
            AudioSourceKind::Sine => Some(Box::new(SineSource::new(48000))),
            AudioSourceKind::Desktop => match PulseMonitor::new(48000) {
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
        })?;
        *self.stream.lock().unwrap() = Some(h);
        Ok(())
    }

    pub fn stop_stream(&self) {
        let h = self.stream.lock().unwrap().take();
        if let Some(h) = h {
            let st = h.stats.clone();
            h.stop();
            let frames = st.frames.load(Ordering::Relaxed);
            tracing::info!(
                "stream stopped: {frames} frames ({} IDR), {:.1} MB, avg encode {:.2} ms, {} audio packets",
                st.idr_frames.load(Ordering::Relaxed),
                st.bytes.load(Ordering::Relaxed) as f64 / 1e6,
                if frames > 0 { st.encode_us.load(Ordering::Relaxed) as f64 / frames as f64 / 1000.0 } else { 0.0 },
                st.audio_packets.load(Ordering::Relaxed)
            );
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
