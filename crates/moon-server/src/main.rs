mod admin;
mod apps;
mod capture;
mod control;
mod display;
mod gpu;
mod http;
mod input;
#[cfg(target_os = "linux")]
mod kwin;
#[cfg(target_os = "macos")]
mod mac;
mod logbuf;
mod mdns;
#[cfg(target_os = "linux")]
mod mic;
#[cfg(target_os = "linux")]
mod pipewire;
#[cfg(target_os = "linux")]
mod portal;
mod remote;
mod rtsp;
mod settings;
mod state;
mod stream;
// Typing with an XKB layout: Linux (Windows types Unicode directly).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod text;
#[cfg(target_os = "linux")]
mod uinput;
#[cfg(windows)]
mod win;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "moon-server", about = "2moon: game streaming host")]
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
    /// NVENC preset default (p1..p7); the admin panel setting replaces it. Hidden: kept
    /// so existing service arguments still work.
    #[arg(long, default_value = "p4", hide = true)]
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
    /// Video encoder: auto (NVIDIA NVENC; else AMD / Intel: VAAPI on Linux, AMF / Quick
    /// Sync on Windows), software, nvenc, vaapi, amf, qsv, or an FFmpeg encoder name.
    #[arg(long, default_value = "auto")]
    encoder: String,
    /// Advertise via mDNS on the loopback interface only (tests).
    #[arg(long)]
    mdns_local_only: bool,
    /// Do not advertise the host on the local network.
    #[arg(long)]
    no_mdns: bool,
    /// Ignored (UPnP was removed; kept so existing service arguments still parse).
    #[arg(long, hide = true)]
    #[allow(dead_code)]
    no_upnp: bool,
    /// No remote access (reaching this host from outside without opening ports).
    #[arg(long)]
    no_remote: bool,
    /// Do not accept the client's microphone (no virtual microphone on this host).
    #[arg(long)]
    no_mic: bool,
    /// Measure the KWin capture frame rate for this many seconds, then exit (diagnostics).
    #[arg(long, hide = true)]
    probe_capture: Option<u64>,
    /// Windows: write the log to this file too (the service's host process has no console).
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Windows: no icon in the notification area.
    #[arg(long)]
    no_tray: bool,
}

fn main() -> Result<()> {
    // Windows service entry points (run / install / uninstall), before anything else.
    #[cfg(windows)]
    if let Some(r) = win::service::from_args() {
        return r;
    }
    run()
}

#[tokio::main]
async fn run() -> Result<()> {
    let args = Args::parse();
    let log_file = args.log_file.as_ref().map(|p| logbuf::open_log_file(p)).transpose()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(log_file.is_none())
        .with_writer(move || logbuf::TeeWriter(log_file.clone()))
        .init();
    // Without a console (Windows service) errors and panics must reach the log too.
    std::panic::set_hook(Box::new(|p| tracing::error!("panic: {p}")));
    let r = serve(args).await;
    if let Err(e) = &r {
        tracing::error!("{e:#}");
    }
    r
}

async fn serve(args: Args) -> Result<()> {
    #[cfg(target_os = "linux")]
    if args.install_kwin_permission {
        let p = kwin::install_desktop_file()?;
        println!("Wrote {} — KWin will now allow screen capture without a dialog (restart moon-server).", p.display());
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if let Some(secs) = args.probe_capture {
        return portal::probe_capture(&args.output, secs);
    }
    let state_dir = args.state_dir.clone().unwrap_or_else(default_state_dir);
    let hostname = args.name.clone().unwrap_or_else(system_hostname);
    let mut cfg = state::Config::with_base_port(args.port, state_dir, hostname);
    cfg.preset_pin = args.pin;
    cfg.video_source = args.video;
    cfg.audio_source = args.audio;
    cfg.encoder = args.encoder.clone();
    cfg.nvenc_preset = args.nvenc_preset.clone();
    cfg.kwin_output = args.output.clone();
    if args.no_mic || cfg!(not(target_os = "linux")) {
        cfg.mic_port = None;
    }
    #[cfg(windows)]
    if cfg.encoder == "auto" {
        // NVIDIA, AMD or Intel: the first hardware encoder FFmpeg can open.
        cfg.encoder = win::detect_encoder();
    }
    #[cfg(windows)]
    if let Some(family) = ["nvenc", "amf", "qsv"].into_iter().find(|f| *f == cfg.encoder) {
        use moon_media::encoder::Codec;
        let can = |c: Codec| c.hardware(family).is_some_and(|n| moon_media::ffenc::probe(&n).is_ok());
        cfg.hevc = can(Codec::Hevc);
        cfg.av1 = can(Codec::Av1);
    }
    // Linux without NVIDIA: AMD and Intel GPUs encode through VAAPI.
    #[cfg(target_os = "linux")]
    if cfg.encoder == "auto" && !moon_media::nvenc::available(moon_media::encoder::Codec::H264) && moon_media::ffenc::probe("h264_nvenc").is_err() {
        match moon_media::ffenc::probe("h264_vaapi") {
            Ok(()) => {
                info!("video encoder: VAAPI (AMD / Intel)");
                cfg.encoder = "vaapi".into();
            }
            Err(e) => info!("no NVENC; VAAPI: {e:#}"),
        }
    }
    #[cfg(target_os = "linux")]
    if cfg.encoder == "vaapi" {
        cfg.hevc = moon_media::ffenc::probe("hevc_vaapi").is_ok();
        cfg.av1 = moon_media::ffenc::probe("av1_vaapi").is_ok();
    } else if cfg.encoder != "software" && cfg.encoder != "openh264" {
        // Direct NVENC first, then NVENC through FFmpeg.
        let direct = matches!(cfg.encoder.as_str(), "auto" | "nvenc");
        let can = |c: moon_media::encoder::Codec| (direct && moon_media::nvenc::available(c)) || moon_media::ffenc::probe(c.nvenc()).is_ok();
        cfg.hevc = can(moon_media::encoder::Codec::Hevc);
        cfg.av1 = can(moon_media::encoder::Codec::Av1);
        cfg.rfi = matches!(cfg.encoder.as_str(), "auto" | "nvenc") && moon_media::nvenc::available(moon_media::encoder::Codec::Hevc);
        if matches!(cfg.encoder.as_str(), "auto" | "nvenc") {
            cfg.yuv444_h264 = moon_media::nvenc::available_444(moon_media::encoder::Codec::H264);
            cfg.yuv444_hevc = cfg.hevc && moon_media::nvenc::available_444(moon_media::encoder::Codec::Hevc);
            cfg.hdr10_hevc = cfg.hevc && moon_media::nvenc::available_hdr10(moon_media::encoder::Codec::Hevc);
            cfg.hdr10_av1 = cfg.av1 && moon_media::nvenc::available_hdr10(moon_media::encoder::Codec::Av1);
        }
    }
    // macOS: Apple VideoToolbox (H.264, HEVC; Apple silicon has no AV1 encoder).
    #[cfg(target_os = "macos")]
    {
        if cfg.encoder == "auto" {
            cfg.encoder = mac::detect_encoder();
        }
        if cfg.encoder == "videotoolbox" {
            cfg.hevc = moon_media::ffenc::probe("hevc_videotoolbox").is_ok();
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
    #[cfg(target_os = "linux")]
    if host.cfg.video_source == state::VideoSourceKind::Portal {
        let cap = portal::PortalCapture::open(&host.cfg.state_dir, 60).await?;
        let _ = host.portal.set(cap);
    }
    #[cfg(target_os = "linux")]
    if host.cfg.video_source == state::VideoSourceKind::Kwin {
        // Checked now so a missing permission shows at startup; each stream then
        // opens its own screencast (see PipeWireSource::kwin).
        drop(kwin::KwinScreencast::start(&args.output)?);
    }
    #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "linux")]
            let injector = uinput::UinputInjector::new(Box::new(move || h.settings.keyboard_layout()));
            #[cfg(windows)]
            let injector = {
                drop(h);
                win::input::SendInputInjector::new()
            };
            #[cfg(target_os = "macos")]
            let injector = {
                drop(h);
                mac::input::CgInjector::new()
            };
            match injector {
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
    // The host has no window: an icon near the clock shows that it runs.
    #[cfg(windows)]
    if !args.no_tray {
        win::tray::spawn(host.clone(), (!args.no_admin).then(|| args.admin_port.unwrap_or(args.port + 1)));
    }
    let rtsp_host = host.clone();
    tokio::spawn(async move {
        if let Err(e) = rtsp::run(rtsp_host).await {
            tracing::error!("RTSP server: {e:#}");
        }
    });
    info!("2moon host '{}' uuid {}", host.hostname(), host.uuid);
    // mDNS: follows the host name set in the admin panel.
    if !args.no_mdns {
        let (h, local_only) = (host.clone(), args.mdns_local_only);
        let mut changed = host.settings.changed.subscribe();
        tokio::spawn(async move {
            let mut current: Option<(String, mdns::Advert)> = None;
            loop {
                let want = Some(h.hostname());
                if current.as_ref().map(|c| &c.0) != want.as_ref() {
                    current = None; // goodbye for the old name first
                    if let Some(name) = want {
                        let port = h.cfg.http_port;
                        match tokio::task::spawn_blocking(move || mdns::advertise(&name, port, local_only).map(|a| (name, a))).await {
                            Ok(Ok(a)) => current = Some(a),
                            Ok(Err(e)) => tracing::warn!("mDNS advertisement failed: {e:#}"),
                            Err(_) => {}
                        }
                    }
                }
                if changed.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    // Remote access through the tunnel (2moon clients), following the admin panel.
    host.remote_available.store(!args.no_remote, std::sync::atomic::Ordering::Relaxed);
    if !args.no_remote {
        remote::spawn(host.clone());
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

    // Stopping the service (SIGTERM, Ctrl+C): put the captured output back to its own mode.
    #[cfg(target_os = "linux")]
    tokio::spawn(async {
        use tokio::signal::unix::{signal, SignalKind};
        let Ok(mut term) = signal(SignalKind::terminate()) else { return };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        tokio::task::spawn_blocking(display::restore).await.ok();
        std::process::exit(0);
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

/// Where the host keeps its identity, paired clients and settings.
fn default_state_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/2moon-server");
    }
    if cfg!(windows) {
        // Shared by the service (SYSTEM) and the admin panel: C:\\ProgramData\\2moon.
        PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("2moon")
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
        base.join("2moon-server")
    }
}

fn system_hostname() -> String {
    #[cfg(target_os = "macos")]
    if let Some(n) = mac::computer_name() {
        return n;
    }
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else {
        std::fs::read_to_string("/proc/sys/kernel/hostname").or_else(|_| std::fs::read_to_string("/etc/hostname")).ok()
    };
    name.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "2moon".into())
}
