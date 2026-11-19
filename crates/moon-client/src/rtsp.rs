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
    /// Use reference frame invalidation if the host supports it.
    pub rfi: bool,
    /// Ask for 4:4:4 chroma (only when the host advertises it for the codec).
    pub yuv444: bool,
    /// Ask for HDR (10-bit HDR10; HEVC / AV1 only).
    pub hdr: bool,
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
            rfi: true,
            yuv444: false,
            hdr: false,
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
    /// Reference frame invalidation is in use (host supports it, and we asked
    /// for multiple reference frames).
    pub rfi: bool,
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
    /// Session key when the host asked for encrypted RTSP (`rtspenc://`).
    key: Option<[u8; 16]>,
}

impl RtspClient {
    /// `host` is the address the client already talks to. Only the port is taken from the
    /// session URL: hosts (Sunshine, 2moon) put their own local IP in it, which is not
    /// reachable from outside their network (NAT, port forwarding), so Moonlight ignores
    /// it too.
    pub fn new(session_url: &str, host: &str, fallback_port: u16) -> Self {
        let port = session_url
            .strip_prefix("rtsp://")
            .or_else(|| session_url.strip_prefix("rtspenc://"))
            .and_then(|r| r.rsplit_once(':'))
            .and_then(|(_, p)| p.trim_end_matches('/').parse().ok())
            .unwrap_or(fallback_port);
        let host = host.trim_matches(['[', ']']).to_string();
        let url = if host.contains(':') { format!("rtsp://[{host}]:{port}") } else { format!("rtsp://{host}:{port}") };
        Self { host, port, url, cseq: 0, session: None, key: None }
    }

    /// Enable encrypted RTSP if the session URL requires it.
    pub fn with_key(mut self, session_url: &str, key: [u8; 16]) -> Self {
        if session_url.starts_with("rtspenc://") {
            self.key = Some(key);
        }
        self
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
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];
        if let Some(key) = self.key {
            sock.write_all(&rtsp::enc::seal(&key, self.cseq, false, &req.encode())).await?;
            loop {
                let n = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut tmp)).await.context("RTSP read timeout")??;
                if n == 0 {
                    bail!("RTSP connection closed before a complete encrypted response");
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some((_, plain, _)) = rtsp::enc::open(&key, &buf, true).map_err(|e| anyhow::anyhow!(e))? {
                    let (mut m, used) = Message::parse(&plain).map_err(|e| anyhow::anyhow!(e))?.context("truncated RTSP response")?;
                    if m.get("Content-Length").is_none() {
                        m.body = plain[used..].to_vec();
                    }
                    debug!("RTSP (encrypted) < {:?}", m.start);
                    return Ok(m);
                }
            }
        }
        sock.write_all(&req.encode()).await?;
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
        // RFI (Sunshine): the host advertises it; we then allow multiple reference frames.
        n.rfi = cfg.rfi && n.describe_sdp.contains("x-nv-video[0].refPicInvalidation");
        let sdp = announce_sdp(&StreamConfig { rfi: n.rfi, ..cfg.clone() }, &self.host, n.encryption_enabled);
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
    push("x-nv-video[0].dynamicRangeMode", if c.hdr { "1" } else { "0" }.into());
    // With RFI the encoder may keep several references (0 = its default), so it can
    // recover from a loss with a P-frame; without it, one reference keeps decoders happy.
    push("x-nv-video[0].maxNumReferenceFrames", if c.rfi { "0" } else { "1" }.into());
    // (colour space << 1) | full range: BT.2020 limited for HDR, BT.601 limited otherwise.
    push("x-nv-video[0].encoderCscMode", if c.hdr { "4" } else { "0" }.into());
    push("x-ss-video[0].chromaSamplingType", if c.yuv444 { "1" } else { "0" }.into());
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

#[cfg(test)]
mod tests {
    use super::RtspClient;

    #[test]
    fn session_url_gives_only_the_port() {
        // Host behind NAT: its local IP in the URL must not replace the address we reach it at.
        let c = RtspClient::new("rtspenc://192.168.1.85:48010", "moon.example.org", 1);
        assert_eq!((c.host.as_str(), c.port, c.url.as_str()), ("moon.example.org", 48010, "rtsp://moon.example.org:48010"));
        let c = RtspClient::new("rtsp://[fd00::5]:48110/", "203.0.113.7", 1);
        assert_eq!((c.host.as_str(), c.port), ("203.0.113.7", 48110));
        let c = RtspClient::new("", "[2001:db8::1]", 48010);
        assert_eq!((c.host.as_str(), c.port, c.url.as_str()), ("2001:db8::1", 48010, "rtsp://[2001:db8::1]:48010"));
    }
}
