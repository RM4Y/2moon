//! RTSP handshake with the host: OPTIONS, DESCRIBE, SETUP x3, ANNOUNCE, PLAY.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use moon_proto::rtsp::{self, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

/// Stream parameters the client asks for in ANNOUNCE.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub packet_size: u32,
    /// 0 = H.264, 1 = HEVC, 2 = AV1
    pub video_format: u32,
    pub audio_channels: u32,
    pub audio_mask: u32,
    pub audio_packet_ms: u32,
    pub min_fec_packets: u32,
    /// Sunshine encryption flags we request (bit 0 = control v2, 1 = video, 2 = audio)
    pub encryption_requested: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_kbps: 10_000,
            packet_size: 1392,
            video_format: 0,
            audio_channels: 2,
            audio_mask: 0x3,
            audio_packet_ms: 5,
            min_fec_packets: 2,
            encryption_requested: 1 | 4,
        }
    }
}

/// Result of a successful handshake.
#[derive(Debug, Clone, Default)]
pub struct Negotiated {
    pub session: String,
    pub video_port: u16,
    pub audio_port: u16,
    pub control_port: u16,
    pub ping_payload_video: Option<String>,
    pub ping_payload_audio: Option<String>,
    pub control_connect_data: Option<u32>,
    pub describe_sdp: String,
    pub feature_flags: u32,
    pub encryption_supported: u32,
    pub encryption_requested: u32,
    /// Flags we told the host to use.
    pub encryption_enabled: u32,
}

impl Negotiated {
    /// Audio is encrypted when negotiated, and always on hosts that predate the
    /// negotiable audio encryption flag.
    pub fn audio_encrypted(&self) -> bool {
        self.encryption_enabled & 4 != 0 || self.encryption_supported & 4 == 0
    }
}

pub struct RtspClient {
    host: String,
    port: u16,
    url: String,
    cseq: u32,
    session: Option<String>,
}

impl RtspClient {
    pub fn new(session_url: &str, fallback_host: &str, fallback_port: u16) -> Self {
        let (host, port) = session_url
            .strip_prefix("rtsp://")
            .or_else(|| session_url.strip_prefix("rtspenc://"))
            .and_then(|r| r.rsplit_once(':'))
            .map(|(h, p)| (h.trim_matches(['[', ']']).to_string(), p.trim_end_matches('/').parse().unwrap_or(fallback_port)))
            .unwrap_or((fallback_host.to_string(), fallback_port));
        let url = if host.contains(':') { format!("rtsp://[{host}]:{port}") } else { format!("rtsp://{host}:{port}") };
        Self { host, port, url, cseq: 0, session: None }
    }

    /// Send one request on its own TCP connection (what modern hosts expect) and read the reply.
    async fn exchange(&mut self, mut req: Message) -> Result<Message> {
        self.cseq += 1;
        req.headers.insert(0, ("CSeq".into(), self.cseq.to_string()));
        req.headers.push(("X-GS-ClientVersion".into(), "14".into()));
        req.headers.push(("Host".into(), self.host.clone()));
        if let Some(s) = &self.session {
            req.headers.push(("Session".into(), s.clone()));
        }
        debug!("RTSP > {}", String::from_utf8_lossy(&req.encode()));
        let mut sock = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((self.host.as_str(), self.port))).await.context("RTSP connect timeout")??;
        sock.set_nodelay(true)?;
        sock.write_all(&req.encode()).await?;
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.context("RTSP read timeout")??;
            if n == 0 {
                // Some hosts close without Content-Length; parse what we have.
                if buf.is_empty() {
                    bail!("RTSP connection closed without response");
                }
                if !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    buf.extend_from_slice(b"\r\n\r\n");
                }
                let (mut m, used) = Message::parse(&buf).map_err(|e| anyhow::anyhow!(e))?.context("truncated RTSP response")?;
                m.body.extend_from_slice(&buf[used..]);
                return Ok(m);
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some((m, used)) = Message::parse(&buf).map_err(|e| anyhow::anyhow!(e))? {
                // DESCRIBE bodies may come without Content-Length: drain until close.
                if m.get("Content-Length").is_none() && m.method().is_none() {
                    let mut m = m;
                    let mut rest = buf[used..].to_vec();
                    while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), sock.read(&mut tmp)).await {
                        if n == 0 {
                            break;
                        }
                        rest.extend_from_slice(&tmp[..n]);
                    }
                    m.body = rest;
                    debug!("RTSP < {:?} body {} bytes", m.start, m.body.len());
                    return Ok(m);
                }
                debug!("RTSP < {:?} {:?}", m.start, m.headers);
                return Ok(m);
            }
        }
    }

    async fn ok(&mut self, req: Message) -> Result<Message> {
        let what = format!("{} {}", req.method().unwrap_or(""), req.target().unwrap_or(""));
        let r = self.exchange(req).await?;
        match r.code() {
            Some(200) => Ok(r),
            c => bail!("RTSP {what} failed with status {c:?}"),
        }
    }

    pub async fn handshake(&mut self, cfg: &StreamConfig) -> Result<Negotiated> {
        let mut n = Negotiated::default();
        self.ok(Message::request("OPTIONS", &self.url.clone())).await?;
        let d = self
            .ok(Message::request("DESCRIBE", &self.url.clone()).header("Accept", "application/sdp").header("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT"))
            .await?;
        n.describe_sdp = String::from_utf8_lossy(&d.body).into_owned();
        let attrs = rtsp::sdp_attributes(&n.describe_sdp);
        n.feature_flags = attrs.get("x-ss-general.featureFlags").and_then(|v| v.parse().ok()).unwrap_or(0);
        n.encryption_supported = attrs.get("x-ss-general.encryptionSupported").and_then(|v| v.parse().ok()).unwrap_or(0);
        n.encryption_requested = attrs.get("x-ss-general.encryptionRequested").and_then(|v| v.parse().ok()).unwrap_or(0);

        for (stream, port_slot) in [("audio/0/0", 0), ("video/0/0", 1), ("control/13/0", 2)] {
            let r = self
                .ok(Message::request("SETUP", &format!("streamid={stream}"))
                    .header("Transport", "unicast;X-GS-ClientPort=50000-50001")
                    .header("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT"))
                .await?;
            if self.session.is_none() {
                let s = r.get("Session").context("SETUP response without Session")?;
                self.session = Some(s.split(';').next().unwrap_or(s).trim().to_string());
            }
            let port = r.get("Transport").and_then(rtsp::transport_server_port).context("SETUP response without server_port")?;
            match port_slot {
                0 => {
                    n.audio_port = port;
                    n.ping_payload_audio = r.get("X-SS-Ping-Payload").map(str::to_string);
                }
                1 => {
                    n.video_port = port;
                    n.ping_payload_video = r.get("X-SS-Ping-Payload").map(str::to_string);
                }
                _ => {
                    n.control_port = port;
                    n.control_connect_data = r.get("X-SS-Connect-Data").and_then(|v| v.trim().parse().ok());
                }
            }
        }
        n.session = self.session.clone().unwrap_or_default();

        n.encryption_enabled = n.encryption_supported & (cfg.encryption_requested | n.encryption_requested);
        let sdp = announce_sdp(cfg, &self.host, n.encryption_enabled);
        self.ok(Message::request("ANNOUNCE", "streamid=control/13/0").header("Content-type", "application/sdp").header("Content-length", sdp.len()).with_body(sdp.into_bytes())).await?;
        self.ok(Message::request("PLAY", "/")).await?;
        Ok(n)
    }
}

/// Build the ANNOUNCE body describing the stream we want.
pub fn announce_sdp(c: &StreamConfig, host: &str, encryption: u32) -> String {
    let mut a: Vec<(String, String)> = Vec::new();
    let mut push = |k: &str, v: String| a.push((k.to_string(), v));
    push("x-nv-general.serverAddress", format!("rtsp://{host}:48010"));
    push("x-nv-video[0].clientViewportWd", c.width.to_string());
    push("x-nv-video[0].clientViewportHt", c.height.to_string());
    push("x-nv-video[0].maxFPS", c.fps.to_string());
    push("x-nv-video[0].packetSize", c.packet_size.to_string());
    push("x-nv-video[0].rateControlMode", "4".into());
    push("x-nv-video[0].timeoutLengthMs", "7000".into());
    push("x-nv-video[0].framesWithInvalidRefThreshold", "0".into());
    push("x-nv-video[0].initialBitrateKbps", c.bitrate_kbps.to_string());
    push("x-nv-video[0].initialPeakBitrateKbps", c.bitrate_kbps.to_string());
    push("x-nv-vqos[0].bw.minimumBitrateKbps", c.bitrate_kbps.to_string());
    push("x-nv-vqos[0].bw.maximumBitrateKbps", c.bitrate_kbps.to_string());
    push("x-ml-video.configuredBitrateKbps", c.bitrate_kbps.to_string());
    push("x-nv-vqos[0].fec.enable", "1".into());
    push("x-nv-vqos[0].videoQualityScoreUpdateTime", "5000".into());
    push("x-nv-vqos[0].qosTrafficType", "5".into());
    push("x-nv-aqos.qosTrafficType", "4".into());
    push("x-nv-general.featureFlags", "135".into());
    push("x-ml-general.featureFlags", "3".into());
    push("x-ss-general.encryptionEnabled", encryption.to_string());
    push("x-nv-vqos[0].fec.minRequiredFecPackets", c.min_fec_packets.to_string());
    push("x-nv-vqos[0].bllFec.enable", "0".into());
    push("x-nv-vqos[0].drc.enable", "0".into());
    push("x-nv-general.enableRecoveryMode", "0".into());
    push("x-nv-video[0].videoEncoderSlicesPerFrame", "1".into());
    push("x-nv-clientSupportHevc", ((c.video_format == 1) as u32).to_string());
    push("x-nv-vqos[0].bitStreamFormat", c.video_format.to_string());
    push("x-nv-video[0].dynamicRangeMode", "0".into());
    push("x-nv-video[0].maxNumReferenceFrames", "1".into());
    push("x-nv-video[0].encoderCscMode", "0".into());
    push("x-ss-video[0].chromaSamplingType", "0".into());
    push("x-ss-video[0].intraRefresh", "0".into());
    push("x-nv-audio.surround.numChannels", c.audio_channels.to_string());
    push("x-nv-audio.surround.channelMask", c.audio_mask.to_string());
    push("x-nv-audio.surround.enable", ((c.audio_channels > 2) as u32).to_string());
    push("x-nv-audio.surround.AudioQuality", "0".into());
    push("x-nv-aqos.packetDuration", c.audio_packet_ms.to_string());
    push("x-nv-general.useReliableUdp", "13".into());
    push("x-nv-ri.useControlChannel", "1".into());
    let mut s = String::from("v=0\r\no=android 0 14 IN IPv4 0.0.0.0\r\ns=NVIDIA Streaming Client\r\n");
    for (k, v) in a {
        s.push_str(&format!("a={k}:{v} \r\n"));
    }
    s.push_str("t=0 0\r\nm=video 47998  \r\n");
    s
}
