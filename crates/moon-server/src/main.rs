mod admin;
mod apps;
mod capture;
mod control;
mod http;
mod input;
mod kwin;
mod logbuf;
mod mdns;
mod mic;
mod pipewire;
mod portal;
mod rtsp;
mod settings;
mod state;
mod stream;
mod text;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "moon-server", about = "2moon: GameStream/Moonlight-compatible streaming host written in Rust")]
struct Args {
    /// Base HTTP port (GameStream default 47989); other ports are derived from it.
    #[arg(long, default_value_t = 47989)]
    port: u16,
    /// Directory holding the host identity and paired clients.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Host name advertised to clients.
    #[arg(long)]
    name: Option<String>,
    /// Accept this PIN for every pairing request (for automation/tests).
    #[arg(long)]
    pin: Option<String>,
    /// Video source.
    #[arg(long, value_enum, default_value_t = state::VideoSourceKind::Test)]
    video: state::VideoSourceKind,
    /// Audio source.
    #[arg(long, value_enum, default_value_t = state::AudioSourceKind::Desktop)]
    audio: state::AudioSourceKind,
    /// Do not inject input into this machine; just log received events.
    #[arg(long)]
    no_input: bool,
    /// PipeWire node for --video pipewire: numeric id or name substring.
    #[arg(long, default_value = "kwin-screencast")]
    pw_node: String,
    /// NVENC quality preset: p1 (fastest) to p7 (best quality).
    #[arg(long, default_value = "p4")]
    nvenc_preset: String,
    /// Output to capture for --video kwin (substring of the output name; empty = first).
    #[arg(long, default_value = "")]
    output: String,
    /// Admin web panel port (default: base port + 1).
    #[arg(long)]
    admin_port: Option<u16>,
    /// Disable the admin web panel.
    #[arg(long)]
    no_admin: bool,
    /// List paired clients and exit.
    #[arg(long)]
    list_clients: bool,
    /// Authorise this executable to use KWin's screencast protocol, then exit.
    #[arg(long)]
    install_kwin_permission: bool,
    /// Video encoder: auto (NVENC if available), software, nvenc, or an FFmpeg encoder name.
    #[arg(long, default_value = "auto")]
    encoder: String,
    /// Advertise via mDNS on the loopback interface only (tests).
    #[arg(long)]
    mdns_local_only: bool,
    /// Do not advertise the host on the local network.
    #[arg(long)]
    no_mdns: bool,
    /// Do not accept the client's microphone (no virtual microphone on this host).
    #[arg(long)]
    no_mic: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(|| logbuf::TeeWriter)
        .init();
    let args = Args::parse();
    if args.install_kwin_permission {
        let p = kwin::install_desktop_file()?;
        println!("Wrote {} — KWin will now allow screen capture without a dialog (restart moon-server).", p.display());
        return Ok(());
    }
    let state_dir = args.state_dir.unwrap_or_else(|| {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
        base.join("2moon-server")
    });
    let hostname = args.name.unwrap_or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").or_else(|_| std::fs::read_to_string("/etc/hostname")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "2moon".into()));
    let mut cfg = state::Config::with_base_port(args.port, state_dir, hostname);
    cfg.preset_pin = args.pin;
    cfg.video_source = args.video;
    cfg.audio_source = args.audio;
    cfg.encoder = args.encoder.clone();
    cfg.nvenc_preset = args.nvenc_preset.clone();
    if args.no_mic {
        cfg.mic_port = None;
    }
    if cfg.encoder != "software" && cfg.encoder != "openh264" {
        cfg.hevc = moon_media::ffenc::probe("hevc_nvenc").is_ok();
        cfg.av1 = moon_media::ffenc::probe("av1_nvenc").is_ok();
        cfg.rfi = matches!(cfg.encoder.as_str(), "auto" | "nvenc") && moon_media::nvenc::available(moon_media::encoder::Codec::Hevc);
        if matches!(cfg.encoder.as_str(), "auto" | "nvenc") {
            cfg.yuv444_h264 = moon_media::nvenc::available_444(moon_media::encoder::Codec::H264);
            cfg.yuv444_hevc = cfg.hevc && moon_media::nvenc::available_444(moon_media::encoder::Codec::Hevc);
            cfg.hdr10_hevc = cfg.hevc && moon_media::nvenc::available_hdr10(moon_media::encoder::Codec::Hevc);
            cfg.hdr10_av1 = cfg.av1 && moon_media::nvenc::available_hdr10(moon_media::encoder::Codec::Av1);
        }
    }
    info!("codecs: H.264{}{}{}", if cfg.hevc { ", HEVC" } else { "" }, if cfg.av1 { ", AV1" } else { "" }, if cfg.rfi { " (direct NVENC, reference frame invalidation)" } else { "" });
    if cfg.hdr10_hevc || cfg.hdr10_av1 {
        info!("10-bit HDR10: {}{}", if cfg.hdr10_hevc { "HEVC " } else { "" }, if cfg.hdr10_av1 { "AV1" } else { "" });
    }
    if cfg.yuv444_h264 || cfg.yuv444_hevc {
        info!("4:4:4: {}{}", if cfg.yuv444_h264 { "H.264 " } else { "" }, if cfg.yuv444_hevc { "HEVC" } else { "" });
    }
    let host = state::Host::load(cfg)?;
    if args.list_clients {
        for c in host.paired_clients() {
            println!("{}", c.name);
        }
        return Ok(());
    }
    if host.cfg.video_source == state::VideoSourceKind::Portal {
        let cap = portal::PortalCapture::open(&host.cfg.state_dir, 60).await?;
        let _ = host.portal.set(cap);
    }
    if host.cfg.video_source == state::VideoSourceKind::Kwin {
        let cap = portal::PortalCapture::from_kwin(&args.output, 60)?;
        let _ = host.portal.set(cap);
    }
    if host.cfg.video_source == state::VideoSourceKind::Pipewire {
        let cap = portal::PortalCapture::from_node(&args.pw_node, 60)?;
        let _ = host.portal.set(cap);
    }
    let no_input = args.no_input;
    let input_host = host.clone();
    control::spawn(
        host.clone(),
        Box::new(move || -> Box<dyn input::Injector> {
            if no_input {
                return Box::new(input::LogInjector::default());
            }
            let h = input_host.clone();
            match input::UinputInjector::new(Box::new(move || h.settings.keyboard_layout())) {
                Ok(u) => Box::new(u),
                Err(e) => {
                    tracing::warn!("input injection disabled: {e:#}");
                    Box::new(input::LogInjector::default())
                }
            }
        }),
    )?;
    if !args.no_admin {
        let admin_host = host.clone();
        let port = args.admin_port.unwrap_or(args.port + 1);
        tokio::spawn(async move {
            if let Err(e) = admin::run(admin_host, port).await {
                tracing::error!("admin panel: {e:#}");
            }
        });
    }
    let rtsp_host = host.clone();
    tokio::spawn(async move {
        if let Err(e) = rtsp::run(rtsp_host).await {
            tracing::error!("RTSP server: {e:#}");
        }
    });
    info!("2moon host '{}' uuid {}", host.hostname(), host.uuid);
    // mDNS: follows the name and visibility set in the admin panel.
    host.mdns_available.store(!args.no_mdns, std::sync::atomic::Ordering::Relaxed);
    if !args.no_mdns {
        let (h, local_only) = (host.clone(), args.mdns_local_only);
        let mut changed = host.settings.changed.subscribe();
        tokio::spawn(async move {
            let mut current: Option<(String, mdns::Advert)> = None;
            loop {
                let want = h.settings.get().mdns.then(|| h.hostname());
                if current.as_ref().map(|c| &c.0) != want.as_ref() {
                    current = None; // goodbye for the old name first
                    if let Some(name) = want {
                        let port = h.cfg.http_port;
                        match tokio::task::spawn_blocking(move || mdns::advertise(&name, port, local_only).map(|a| (name, a))).await {
                            Ok(Ok(a)) => current = Some(a),
                            Ok(Err(e)) => tracing::warn!("mDNS advertisement failed: {e:#}"),
                            Err(_) => {}
                        }
                    } else {
                        info!("mDNS advertisement disabled in the settings");
                    }
                }
                if changed.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    // Automatic stop: quit the app when no client has been streaming for the chosen time.
    let h = host.clone();
    tokio::spawn(async move {
        let mut idle_since: Option<std::time::Instant> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let minutes = h.settings.get().auto_quit_min;
            let streaming = h.live.lock().unwrap().active;
            if minutes == 0 || h.current_session().is_none() || streaming {
                idle_since = None;
                continue;
            }
            let since = *idle_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= std::time::Duration::from_secs(minutes as u64 * 60) {
                info!("no client for {minutes} min: quitting the application");
                h.set_session(None);
                h.apps.stop();
                idle_since = None;
            }
        }
    });

    // PINs typed in the console are delivered to waiting pairing requests.
    let h = host.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let pin = line.trim();
            if !pin.is_empty() {
                let n = h.submit_pin(pin);
                info!("PIN delivered to {n} pending pairing request(s)");
            }
            line.clear();
        }
    });

    http::run(host).await
}
