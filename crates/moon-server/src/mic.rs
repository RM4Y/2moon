//! Client microphone (2moon extension): received over UDP, decoded, played into a
//! virtual microphone made the default input during the stream.
//! Created with `pactl` (PulseAudio and PipeWire) on the first packet only.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use moon_media::opus::{OpusDecoder, StreamLayout};
use moon_media::pulse::PulseStream;
use moon_proto::audio::mic;
use tracing::{debug, info, warn};

const SINK: &str = "2moon_mic_sink";
const SOURCE: &str = "2moon_mic";

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl").args(args).env("LC_ALL", "C").output().context("cannot run pactl (install pulseaudio-utils / pipewire-pulseaudio)")?;
    if !out.status.success() {
        bail!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The virtual microphone; removed (and the previous default input restored) on drop.
struct VirtualMic {
    modules: Vec<String>,
    previous_source: Option<String>,
}

impl VirtualMic {
    fn create() -> Result<Self> {
        remove_stale();
        let previous_sink = pactl(&["get-default-sink"]).ok();
        let previous_source = pactl(&["get-default-source"]).ok().filter(|s| s != SOURCE);
        let mut m = Self { modules: Vec::new(), previous_source };
        m.modules.push(pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
            "rate=48000",
            "channels=1",
            r#"sink_properties="device.description=\"2moon - micro du client\"""#,
        ])?);
        m.modules.push(pactl(&[
            "load-module",
            "module-remap-source",
            &format!("master={SINK}.monitor"),
            &format!("source_name={SOURCE}"),
            "channels=1",
            "channel_map=mono",
            r#"source_properties="device.description=\"2moon Microphone\"""#,
        ])?);
        // A new sink may become the default output: keep the user's.
        if let Some(s) = previous_sink.filter(|s| s != SINK) {
            let _ = pactl(&["set-default-sink", &s]);
        }
        pactl(&["set-default-source", SOURCE])?;
        info!("virtual microphone \"2moon Microphone\" created and set as default input (was {:?})", m.previous_source);
        Ok(m)
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        // Restore the previous default input, unless the user picked another one meanwhile.
        if let (Some(prev), Ok(cur)) = (&self.previous_source, pactl(&["get-default-source"])) {
            if cur == SOURCE {
                let _ = pactl(&["set-default-source", prev]);
            }
        }
        for id in self.modules.iter().rev() {
            if let Err(e) = pactl(&["unload-module", id]) {
                warn!("cannot remove the virtual microphone: {e:#}");
            }
        }
        info!("virtual microphone removed, default input restored");
    }
}

/// Remove modules left behind by a previous run that did not exit cleanly.
fn remove_stale() {
    let Ok(list) = pactl(&["list", "short", "modules"]) else { return };
    for line in list.lines() {
        let mut f = line.split('\t');
        let (Some(id), Some(_name), Some(args)) = (f.next(), f.next(), f.next()) else { continue };
        if args.contains(&format!("sink_name={SINK}")) || args.contains(&format!("source_name={SOURCE}")) {
            debug!("removing stale virtual microphone module {id}");
            let _ = pactl(&["unload-module", id]);
        }
    }
}

pub struct MicReceiver {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicReceiver {
    /// Listen for the client's microphone on `socket` (packets from `client_ip` only).
    pub fn start(socket: Arc<UdpSocket>, client_ip: IpAddr, key: [u8; 16], key_id: u32) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new().name("mic-rx".into()).spawn(move || {
            if let Err(e) = run(&socket, client_ip, mic::cipher(key, key_id), &s2) {
                warn!("client microphone: {e:#}");
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for MicReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn same_ip(a: IpAddr, b: IpAddr) -> bool {
    let canon = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    canon(a) == canon(b)
}

fn run(sock: &UdpSocket, client_ip: IpAddr, cipher: moon_proto::audio::AudioCipher, stop: &AtomicBool) -> Result<()> {
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    // Drop anything queued from a previous session.
    let mut buf = [0u8; 2048];
    sock.set_nonblocking(true)?;
    while sock.recv_from(&mut buf).is_ok() {}
    sock.set_nonblocking(false)?;

    let mut dec = OpusDecoder::new(StreamLayout::mono(), mic::SAMPLE_RATE)?;
    let mut pcm = Vec::new();
    let mut device: Option<(VirtualMic, PulseStream)> = None;
    let mut next_seq: Option<u16> = None;
    let mut packets = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let (n, from): (usize, SocketAddr) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e.into()),
        };
        if !same_ip(from.ip(), client_ip) {
            continue;
        }
        let Some((seq, _ts, opus)) = mic::open(&cipher, &buf[..n]) else { continue };
        if device.is_none() {
            let vm = VirtualMic::create()?;
            let out = PulseStream::playback_to(SINK, "2moon client microphone", mic::SAMPLE_RATE, 1, 30)?;
            device = Some((vm, out));
        }
        let (_, out) = device.as_mut().unwrap();
        // Conceal a few lost packets; ignore late / duplicated ones.
        if let Some(exp) = next_seq {
            let gap = seq.wrapping_sub(exp);
            if gap >= 0x8000 {
                continue;
            }
            for _ in 0..gap.min(5) {
                dec.decode(None, mic::FRAME_SAMPLES, &mut pcm)?;
                out.write(&pcm)?;
            }
        }
        next_seq = Some(seq.wrapping_add(1));
        if dec.decode(Some(&opus), 0, &mut pcm).is_ok() {
            // Never let the microphone lag: drop the buffer if the device stalls.
            if out.latency_us() > 120_000 {
                out.flush();
            }
            out.write(&pcm)?;
        }
        packets += 1;
        if packets == 1 {
            info!("receiving the client's microphone");
        }
    }
    if packets > 0 {
        info!("client microphone: {packets} packets");
    }
    Ok(())
}

/// Bind the microphone UDP port on all IPv4 and IPv6 addresses.
pub fn bind(port: u16) -> Result<UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_only_v6(false)?;
    s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)).into()).with_context(|| format!("cannot bind microphone port {port}"))?;
    Ok(s.into())
}
