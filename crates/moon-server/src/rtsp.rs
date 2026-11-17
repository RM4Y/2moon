//! RTSP negotiation server (OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY).

use std::sync::Arc;

use anyhow::Result;
use moon_proto::rtsp::{sdp_attributes, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::state::Host;
use crate::stream::StreamParams;

const SESSION_ID: &str = "DEADBEEFCAFE";

pub async fn run(host: Arc<Host>) -> Result<()> {
    let l = TcpListener::bind(("0.0.0.0", host.cfg.rtsp_port)).await?;
    info!("RTSP on :{}", host.cfg.rtsp_port);
    loop {
        let (sock, peer) = l.accept().await?;
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(host, sock).await {
                debug!("RTSP connection {peer}: {e:#}");
            }
        });
    }
}

async fn connection(host: Arc<Host>, mut sock: TcpStream) -> Result<()> {
    sock.set_nodelay(true)?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some((req, _)) = Message::parse(&buf).map_err(|e| anyhow::anyhow!(e))? {
            let resp = handle(&host, &req);
            sock.write_all(&resp.encode()).await?;
            // Clients read the response until the connection closes: one request per connection.
            sock.shutdown().await?;
            return Ok(());
        }
    }
}

fn handle(host: &Host, req: &Message) -> Message {
    let method = req.method().unwrap_or("");
    let target = req.target().unwrap_or("");
    debug!("RTSP {method} {target}");
    let cseq = req.cseq().unwrap_or(0);
    let ok = || Message::response(200, "OK").header("CSeq", cseq);
    let Some(session) = host.current_session() else {
        warn!("RTSP {method} without a launched session");
        return Message::response(503, "Service Unavailable").header("CSeq", cseq);
    };
    match method {
        "OPTIONS" => ok(),
        "DESCRIBE" => {
            // Advertise control-stream encryption v2 and the audio layouts we can encode.
            let mut sdp = String::new();
            sdp.push_str("a=x-ss-general.featureFlags:0\r\n");
            sdp.push_str("a=x-ss-general.encryptionSupported:5\r\n");
            sdp.push_str("a=x-ss-general.encryptionRequested:1\r\n");
            sdp.push_str("a=fmtp:97 surround-params=21101\r\n");
            ok().header("Content-Type", "application/sdp").with_body(sdp.into_bytes())
        }
        "SETUP" => {
            let (port, extra): (u16, Option<(&str, String)>) = if target.contains("streamid=audio") {
                (host.cfg.audio_port, Some(("X-SS-Ping-Payload", String::from_utf8_lossy(&session.ss_ping_payload).into_owned())))
            } else if target.contains("streamid=video") {
                (host.cfg.video_port, Some(("X-SS-Ping-Payload", String::from_utf8_lossy(&session.ss_ping_payload).into_owned())))
            } else if target.contains("streamid=control") {
                (host.cfg.control_port, Some(("X-SS-Connect-Data", session.control_connect_data.to_string())))
            } else {
                return Message::response(404, "Not Found").header("CSeq", cseq);
            };
            let mut r = ok().header("Session", format!("{SESSION_ID};timeout = 90")).header("Transport", format!("server_port={port}"));
            if let Some((k, v)) = extra {
                r = r.header(k, v);
            }
            r
        }
        "ANNOUNCE" => {
            let a = sdp_attributes(&String::from_utf8_lossy(&req.body));
            let num = |k: &str, d: u32| a.get(k).and_then(|v| v.trim().parse::<i64>().ok()).map(|v| v.max(0) as u32).unwrap_or(d);
            let mut p = StreamParams {
                width: num("x-nv-video[0].clientViewportWd", session.width),
                height: num("x-nv-video[0].clientViewportHt", session.height),
                fps: num("x-nv-video[0].maxFPS", session.fps),
                packet_size: num("x-nv-video[0].packetSize", 1392),
                bitrate_kbps: num("x-nv-vqos[0].bw.maximumBitrateKbps", 10_000),
                min_fec_packets: num("x-nv-vqos[0].fec.minRequiredFecPackets", 2),
                fec_percent: 20,
                video_format: num("x-nv-vqos[0].bitStreamFormat", 0),
                audio_channels: num("x-nv-audio.surround.numChannels", 2),
                audio_packet_ms: num("x-nv-aqos.packetDuration", 5),
                encryption: num("x-ss-general.encryptionEnabled", 0),
                encoder: host.cfg.encoder.clone(),
            };
            if let Some(cb) = a.get("x-ml-video.configuredBitrateKbps").and_then(|v| v.trim().parse::<u32>().ok()) {
                if cb > 0 {
                    p.bitrate_kbps = cb;
                }
            }
            // Make sure the dimensions are even (4:2:0 chroma).
            p.width &= !1;
            p.height &= !1;
            info!("stream requested: {}x{}@{} {} kbps, packet {} B, codec {}, audio {}ch/{}ms, encryption flags {}", p.width, p.height, p.fps, p.bitrate_kbps, p.packet_size, p.video_format, p.audio_channels, p.audio_packet_ms, p.encryption);
            host.set_stream_params(p);
            ok().header("Session", SESSION_ID)
        }
        "PLAY" => match host.start_stream() {
            Ok(()) => ok().header("Session", SESSION_ID),
            Err(e) => {
                warn!("cannot start stream: {e:#}");
                Message::response(500, "Internal Server Error").header("CSeq", cseq)
            }
        },
        "TEARDOWN" => {
            host.stop_stream();
            ok()
        }
        _ => Message::response(501, "Not Implemented").header("CSeq", cseq),
    }
}
