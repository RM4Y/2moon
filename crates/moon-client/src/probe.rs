//! Debug tool: start a session and record the raw RTSP exchange and UDP
//! traffic, so the wire format can be inspected offline.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::net::UdpSocket;

use crate::http::{GsClient, LaunchParams};
use crate::rtsp::{RtspClient, StreamConfig};

pub async fn run(c: &GsClient, out: &Path, seconds: u64) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let rikey: [u8; 16] = moon_proto::crypto::random_bytes();
    let rikeyid: u32 = rand::random::<u32>() & 0x7fff_ffff;
    let env = |k: &str, d: u32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let cfg = StreamConfig {
        width: env("MOON_PROBE_WIDTH", 1280),
        height: env("MOON_PROBE_HEIGHT", 720),
        bitrate_kbps: env("MOON_PROBE_BITRATE", 10_000),
        ..StreamConfig::default()
    };
    let lp = LaunchParams { app_id: c.app_list().await?.first().map(|a| a.id).unwrap_or(1), width: cfg.width, height: cfg.height, fps: cfg.fps, rikey, rikeyid, audio_channels: 2 };
    let info = c.server_info().await?;
    let url = c.launch(&lp, info.current_game != 0).await?;
    println!("session url: {url}");
    let mut rtsp = RtspClient::new(&url, &c.host, c.http_port + 21).with_key(&url, rikey);
    let neg = rtsp.handshake(&cfg).await?;
    std::fs::write(out.join("describe.sdp"), &neg.describe_sdp)?;
    std::fs::write(out.join("keys.txt"), format!("rikey={}\nrikeyid={rikeyid}\n{neg:#?}\n", hex::encode(rikey)))?;
    println!("{neg:#?}");

    let ctrl_addr: std::net::SocketAddr = tokio::net::lookup_host((c.host.as_str(), neg.control_port)).await?.next().unwrap();
    let ctrl = crate::control::connect(ctrl_addr, neg.control_connect_data.unwrap_or(0), rikey, neg.encryption_requested & 1 != 0 || cfg.encryption_requested & 1 != 0 || true)?;
    let v = UdpSocket::bind("0.0.0.0:0").await?;
    let a = UdpSocket::bind("0.0.0.0:0").await?;
    v.connect((c.host.as_str(), neg.video_port)).await?;
    a.connect((c.host.as_str(), neg.audio_port)).await?;
    let mut vf = std::fs::File::create(out.join("video.bin"))?;
    let mut af = std::fs::File::create(out.join("audio.bin"))?;
    let start = Instant::now();
    let mut seq: u32 = 0;
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let (mut nv, mut na) = (0usize, 0usize);
    let mut vb = vec![0u8; 65536];
    let mut ab = vec![0u8; 65536];
    let mut idr_sent = false;
    while start.elapsed() < Duration::from_secs(seconds) {
        if !idr_sent && start.elapsed() > Duration::from_secs(2) {
            ctrl.sender.request_idr();
            idr_sent = true;
            println!("IDR requested after {} video packets", nv);
        }
        if last_ping.elapsed() > Duration::from_millis(200) {
            seq += 1;
            for (sock, payload) in [(&v, &neg.ping_payload_video), (&a, &neg.ping_payload_audio)] {
                let mut p = match payload {
                    Some(pp) => pp.as_bytes().to_vec(),
                    None => b"PING".to_vec(),
                };
                if payload.is_some() {
                    p.extend_from_slice(&seq.to_be_bytes());
                }
                let _ = sock.send(&p).await;
            }
            last_ping = Instant::now();
        }
        tokio::select! {
            r = v.recv(&mut vb) => if let Ok(n) = r { vf.write_all(&(n as u32).to_le_bytes())?; vf.write_all(&vb[..n])?; nv += 1; },
            r = a.recv(&mut ab) => if let Ok(n) = r { af.write_all(&(n as u32).to_le_bytes())?; af.write_all(&ab[..n])?; na += 1; },
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    println!("captured {nv} video and {na} audio packets");
    while let Ok(e) = ctrl.events.try_recv() {
        println!("control event before cancel: {e:?}");
    }
    let _ = c.cancel().await;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(2) {
        match ctrl.events.try_recv() {
            Ok(e) => println!("control event after cancel: {e:?}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    ctrl.sender.disconnect();
    Ok(())
}
