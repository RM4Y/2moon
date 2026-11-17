use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use moon_client::http::GsClient;
use moon_client::rtsp::StreamConfig;
use moon_client::session::{Session, SessionOptions};
use moon_client::store::{KnownHost, Store};

#[derive(Parser)]
#[command(name = "moon-client", about = "2moon: GameStream/Moonlight-compatible streaming client written in Rust")]
struct Args {
    /// Directory holding the client identity and known hosts.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Without a command, the graphical interface opens.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a host using a PIN (type the same PIN on the host).
    Pair {
        host: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// List the applications of a paired host.
    List { host: String },
    /// Show host information.
    Info { host: String },
    /// Find GameStream hosts on the local network (mDNS).
    Discover {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Quit the running application on the host.
    Quit { host: String },
    /// Debug: launch, negotiate RTSP and dump raw UDP packets for a few seconds.
    Probe {
        host: String,
        #[arg(long, default_value = "probe-dump")]
        out: PathBuf,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
    /// Stream an application from a paired host.
    Stream {
        host: String,
        /// Application id (default: first app).
        #[arg(long)]
        app: Option<u32>,
        #[arg(long, default_value_t = 1920)]
        width: u32,
        #[arg(long, default_value_t = 1080)]
        height: u32,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Video bitrate in kbit/s.
        #[arg(long, default_value_t = 20000)]
        bitrate: u32,
        /// Audio channels (2, 6 or 8).
        #[arg(long, default_value_t = 2)]
        audio_channels: u32,
        /// No window and no audio output: receive and decode for N seconds, print stats.
        #[arg(long)]
        headless: bool,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Quit the app on the host when the stream window closes.
        #[arg(long)]
        quit_on_exit: bool,
        /// Video codec: auto, h264, hevc or av1.
        #[arg(long, default_value = "auto")]
        codec: String,
        /// Decode on the CPU even if a GPU decoder is available.
        #[arg(long)]
        software_decode: bool,
        /// Headless only: send a scripted sequence of input events (for tests).
        #[arg(long)]
        test_input: bool,
        /// Send this computer's default microphone to the host (2moon hosts only).
        #[arg(long)]
        mic: bool,
    },
}

/// Split `host[:port]` (IPv6 in brackets) into address + HTTP port.
pub fn split_host(s: &str) -> (String, u16) {
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            return (h.to_string(), p.trim_start_matches(':').parse().unwrap_or(47989));
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().unwrap_or(47989)),
        _ => (s.to_string(), 47989),
    }
}

fn client_for(store: &Store, host: &str) -> GsClient {
    let (addr, port) = split_host(host);
    let mut c = GsClient::new(&addr, port, store.identity.clone());
    if let Some(k) = store.find(&addr, port) {
        c.host = k.address.clone();
        c.http_port = k.http_port;
        c.server_cert = moon_proto::crypto::pem_decode_cert(&k.server_cert_pem).ok();
    }
    c
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let mut store = Store::load(args.state_dir.unwrap_or_else(Store::default_dir))?;

    let Some(cmd) = args.cmd else {
        return moon_client::gui::run(store);
    };
    match cmd {
        Cmd::Pair { host, pin } => {
            let mut c = client_for(&store, &host);
            c.server_cert = None;
            let pin = pin.unwrap_or_else(|| format!("{:04}", rand::random::<u16>() % 10000));
            println!("Enter PIN {pin} on the host to pair...");
            let device = std::fs::read_to_string("/proc/sys/kernel/hostname").or_else(|_| std::fs::read_to_string("/etc/hostname")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "2moon".into());
            c.pair(&pin, &device).await?;
            let info = c.server_info().await?;
            store.upsert(KnownHost {
                name: info.hostname.clone(),
                uuid: info.uuid.clone(),
                address: c.host.clone(),
                http_port: c.http_port,
                server_cert_pem: moon_proto::crypto::pem_encode_cert(c.server_cert.as_ref().unwrap()),
            })?;
            println!("Paired with {} (paired status reported by host: {})", info.hostname, info.paired);
        }
        Cmd::List { host } => {
            let c = client_for(&store, &host);
            c.server_cert.as_ref().context("not paired with this host")?;
            for a in c.app_list().await? {
                println!("{}\t{}", a.id, a.title);
            }
        }
        Cmd::Info { host } => {
            let c = client_for(&store, &host);
            println!("{:#?}", c.server_info().await?);
        }
        Cmd::Probe { host, out, seconds } => {
            let c = client_for(&store, &host);
            moon_client::probe::run(&c, &out, seconds).await?;
        }
        Cmd::Discover { seconds } => {
            let mdns = mdns_sd::ServiceDaemon::new()?;
            let rx = mdns.browse("_nvstream._tcp.local.")?;
            let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
            let mut seen = std::collections::HashSet::new();
            while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
                if let Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(left) {
                    let addrs: Vec<String> = info.get_addresses().iter().map(|a| a.to_string()).collect();
                    if seen.insert(info.get_fullname().to_string()) {
                        println!("{}\t{}\tport {}", info.get_fullname().trim_end_matches("._nvstream._tcp.local."), addrs.join(","), info.get_port());
                    }
                }
            }
            let _ = mdns.shutdown();
        }
        Cmd::Stream { host, app, width, height, fps, bitrate, audio_channels, headless, seconds, quit_on_exit, test_input, codec, software_decode, mic } => {
            use moon_media::encoder::Codec;
            let codec = match codec.to_lowercase().as_str() {
                "auto" => None,
                "h264" | "avc" => Some(Codec::H264),
                "hevc" | "h265" => Some(Codec::Hevc),
                "av1" => Some(Codec::Av1),
                other => anyhow::bail!("unknown codec {other}"),
            };
            let c = client_for(&store, &host);
            c.server_cert.as_ref().context("not paired with this host (run `pair` first)")?;
            let stream = StreamConfig {
                width,
                height,
                fps,
                bitrate_kbps: bitrate,
                audio_channels,
                audio_mask: match audio_channels {
                    6 => 0x3f,
                    8 => 0x63f,
                    _ => 0x3,
                },
                ..StreamConfig::default()
            };
            let opts = SessionOptions { stream, app_id: app, audio_output: !headless, decode: true, codec, hw_decode: !software_decode, mic, zero_copy: None };
            if headless {
                let first = std::sync::Arc::new(std::sync::Mutex::new(None::<(usize, usize, u32)>));
                let f2 = first.clone();
                let session = Session::start(&c, opts, Box::new(move |d: moon_client::session::Decoded<'_>, _| {
                    if let moon_client::session::Decoded::Cpu(p) = d {
                        f2.lock().unwrap().get_or_insert((p.width, p.height, p.y[p.y.len() / 2] as u32));
                    }
                }))
                .await?;
                if test_input {
                    use moon_proto::control::input::{self as inp, Event as In};
                    let ctl = session.control.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let evs = [
                            In::Key { down: true, vk: 0x41, modifiers: inp::MOD_SHIFT },
                            In::Key { down: false, vk: 0x41, modifiers: inp::MOD_SHIFT },
                            In::MouseRel { dx: 12, dy: -7 },
                            In::MouseAbs { x: 640, y: 360, width: 1280, height: 720 },
                            In::MouseButton { down: true, button: inp::BUTTON_LEFT },
                            In::MouseButton { down: false, button: inp::BUTTON_LEFT },
                            In::Scroll { amount: -120 },
                            In::HScroll { amount: 120 },
                            In::Text("2moon".into()),
                            In::Gamepad(inp::Gamepad { index: 0, active_mask: 1, buttons: inp::pad::A, left_trigger: 0, right_trigger: 200, ls_x: 1000, ls_y: -1000, rs_x: 0, rs_y: 0 }),
                        ];
                        for e in &evs {
                            ctl.input(e);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        println!("sent {} input events", evs.len());
                    });
                }
                let t = std::time::Instant::now();
                while t.elapsed() < std::time::Duration::from_secs(seconds) {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if let Ok(moon_client::control::ControlEvent::Terminated(code)) = session.control_events.try_recv() {
                        println!("host terminated the session (0x{code:08x})");
                        break;
                    }
                }
                let fps = session.stats.frames_decoded.load(std::sync::atomic::Ordering::Relaxed) as f64 / t.elapsed().as_secs_f64();
                println!("codec {:?}; {}", session.codec, session.stats.summary());
                println!("decode rate {fps:.1} fps; first picture {:?}", first.lock().unwrap());
                session.stop();
            } else {
                let (el, slot) = moon_client::window::make_event_loop()?;
                let s2 = slot.clone();
                let mut session = Session::start(&c, opts, Box::new(move |d: moon_client::session::Decoded<'_>, _| s2.put(d))).await?;
                let proxy = moon_client::window::proxy(&slot);
                let events = std::mem::replace(&mut session.control_events, std::sync::mpsc::channel().1);
                std::thread::spawn(move || {
                    while let Ok(ev) = events.recv() {
                        match ev {
                            moon_client::control::ControlEvent::Terminated(code) => {
                                let _ = proxy.send_event(moon_client::window::UserEvent::SessionEnded(format!("host ended the session (0x{code:08x})")));
                            }
                            moon_client::control::ControlEvent::Disconnected => {
                                let _ = proxy.send_event(moon_client::window::UserEvent::SessionEnded("control stream disconnected".into()));
                                break;
                            }
                            _ => {}
                        }
                    }
                });
                let title = format!("2moon — {}", c.host);
                let mut app = moon_client::window::App::new(title, width, height, slot, session.control.clone(), session.stats.clone());
                el.run_app(&mut app)?;
                if let Some(why) = &app.exit_reason {
                    println!("{why}");
                }
                println!("{}", session.stats.summary());
                session.stop();
                if quit_on_exit {
                    c.cancel().await?;
                }
            }
        }
        Cmd::Quit { host } => {
            client_for(&store, &host).cancel().await?;
            println!("Session ended");
        }
    }
    Ok(())
}
