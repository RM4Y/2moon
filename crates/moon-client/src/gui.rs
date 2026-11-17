//! Graphical client: host list (mDNS + saved hosts), pairing, app list,
//! settings, and the stream itself rendered as a GPU texture.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, Key, RichText, Vec2};
use moon_media::encoder::Codec;
use moon_proto::control::input::{self as inp, Event as In};
use moon_proto::xml::App as HostApp;
use serde::{Deserialize, Serialize};

use crate::control::{ControlEvent, ControlSender};
use crate::http::{GsClient, ServerInfo};
use crate::rtsp::StreamConfig;
use crate::session::{Session, SessionOptions, Stats};
use crate::store::{KnownHost, Store};
use crate::video_gl::VideoView;

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// "auto", "h264", "hevc", "av1"
    pub codec: String,
    pub audio_channels: u32,
    pub hw_decode: bool,
    pub fullscreen: bool,
    pub quit_app_on_exit: bool,
    pub show_stats: bool,
    /// Wait for the display refresh before presenting (no tearing, +1 frame of latency on X11).
    pub vsync: bool,
    /// Send the microphone to the host (2moon hosts only).
    pub mic: bool,
    /// 4:4:4 chroma when available (sharper text, more bandwidth).
    pub yuv444: bool,
    /// Accept HDR (10-bit) streams, tone-mapped to this SDR window.
    pub hdr: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 60, bitrate_kbps: 20_000, codec: "auto".into(), audio_channels: 2, hw_decode: true, fullscreen: false, quit_app_on_exit: false, show_stats: false, vsync: false, mic: false, yuv444: false, hdr: false }
    }
}

impl Settings {
    fn load(store: &Store) -> Self {
        std::fs::read_to_string(store.dir.join("settings.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    fn save(&self, store: &Store) {
        let _ = std::fs::write(store.dir.join("settings.json"), serde_json::to_string_pretty(self).unwrap_or_default());
    }
    fn codec(&self) -> Option<Codec> {
        match self.codec.as_str() {
            "h264" => Some(Codec::H264),
            "hevc" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
    /// Reasonable default bitrate for a resolution / frame rate.
    fn suggested_bitrate(&self) -> u32 {
        let px = (self.width * self.height) as f64 / (1920.0 * 1080.0);
        ((20_000.0 * px * (self.fps as f64 / 60.0).sqrt()) / 1000.0).round() as u32 * 1000
    }
}

// ---------------------------------------------------------------- hosts

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostRef {
    name: String,
    address: String,
    port: u16,
}

impl HostRef {
    fn key(&self) -> String {
        format!("{}:{}", self.address, self.port)
    }
}

#[derive(Debug, Clone, Default)]
struct HostStatus {
    online: bool,
    info: Option<ServerInfo>,
    checked: Option<Instant>,
}

enum Msg {
    Paired(HostRef, Result<KnownHost>),
    Apps(HostRef, Result<Vec<HostApp>>),
    Started(HostRef, Box<Result<Session>>),
    Quit(Result<()>),
}

enum Screen {
    Home,
    Pairing { host: HostRef, pin: String },
    Apps { host: HostRef, apps: Option<Vec<HostApp>>, error: Option<String> },
    Settings,
    Connecting { host: HostRef, since: Instant },
    Streaming,
}

struct Stream {
    session: Session,
    host: HostRef,
    captured: bool,
    fullscreen: bool,
    pressed: HashSet<u16>,
    mods: egui::Modifiers,
    /// When the microphone was last (un)muted, to show a short notice.
    mic_toast: Option<Instant>,
    /// (window start, frames presented at start, fps over the last window)
    fps_window: (Instant, u64, f64),
}

pub struct GuiApp {
    rt: tokio::runtime::Runtime,
    store: Store,
    settings: Settings,
    screen: Screen,
    discovered: Arc<Mutex<Vec<HostRef>>>,
    status: Arc<Mutex<HashMap<String, HostStatus>>>,
    targets: Arc<Mutex<Vec<(HostRef, Option<Vec<u8>>)>>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    toast: Option<(String, Instant)>,
    manual: String,
    stream: Option<Stream>,
    /// GPU video output (shared with the decode thread).
    video: VideoView,
}

pub fn run(store: Store) -> Result<()> {
    let vsync = Settings::load(&store).vsync;
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("2moon").with_inner_size([1100.0, 700.0]).with_min_inner_size([640.0, 400.0]).with_app_id("2moon"),
        glow_options: eframe::egui_glow::GlowConfiguration { vsync, ..Default::default() },
        ..Default::default()
    };
    eframe::run_native("2moon", opts, Box::new(|cc| Ok(Box::new(GuiApp::new(cc, store)))))
        .map_err(|e| anyhow::anyhow!("cannot open the window: {e}"))
}

fn client_for(store: &Store, h: &HostRef) -> GsClient {
    let mut c = GsClient::new(&h.address, h.port, store.identity.clone());
    if let Some(k) = store.find(&h.address, h.port) {
        c.server_cert = moon_proto::crypto::pem_decode_cert(&k.server_cert_pem).ok();
    }
    c
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>, store: Store) -> Self {
        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        cc.egui_ctx.set_global_style(style);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        let (tx, rx) = mpsc::channel();
        let settings = Settings::load(&store);
        let app = Self {
            rt,
            store,
            settings,
            screen: Screen::Home,
            discovered: Default::default(),
            status: Default::default(),
            targets: Default::default(),
            tx,
            rx,
            toast: None,
            manual: String::new(),
            stream: None,
            video: VideoView::default(),
        };
        app.start_discovery(cc.egui_ctx.clone());
        let mut app = app;
        if let Ok(target) = std::env::var("MOON_GUI_AUTOLAUNCH") {
            let (address, port) = crate::split_host_port(&target);
            let h = HostRef { name: address.clone(), address, port };
            app.launch(&cc.egui_ctx, h, std::env::var("MOON_GUI_APP").ok().and_then(|a| a.parse().ok()).unwrap_or(1));
        }
        app.start_polling(cc.egui_ctx.clone());
        app
    }

    /// Browse `_nvstream._tcp` continuously.
    fn start_discovery(&self, ctx: egui::Context) {
        let found = self.discovered.clone();
        std::thread::spawn(move || {
            let Ok(mdns) = mdns_sd::ServiceDaemon::new() else { return };
            let Ok(rx) = mdns.browse("_nvstream._tcp.local.") else { return };
            while let Ok(ev) = rx.recv() {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = ev {
                    let addrs = info.get_addresses();
                    // Prefer IPv4, then global IPv6.
                    let addr = addrs.iter().find(|a| a.is_ipv4()).or_else(|| addrs.iter().next());
                    let Some(addr) = addr else { continue };
                    let name = info.get_fullname().trim_end_matches("._nvstream._tcp.local.").to_string();
                    let h = HostRef { name, address: addr.to_string(), port: info.get_port() };
                    let mut f = found.lock().unwrap();
                    match f.iter_mut().find(|x| x.name == h.name && x.port == h.port) {
                        // Same host seen again: switch to IPv4 when it shows up.
                        Some(x) if !x.address.contains('.') && h.address.contains('.') => {
                            x.address = h.address;
                            ctx.request_repaint();
                        }
                        Some(_) => {}
                        None => {
                            f.push(h);
                            ctx.request_repaint();
                        }
                    }
                }
            }
        });
    }

    /// Refresh every host's serverinfo every few seconds.
    fn start_polling(&self, ctx: egui::Context) {
        let (targets, status, identity) = (self.targets.clone(), self.status.clone(), self.store.identity.clone());
        self.rt.spawn(async move {
            loop {
                let list = targets.lock().unwrap().clone();
                let mut tasks = Vec::new();
                for (h, cert) in list {
                    let mut c = GsClient::new(&h.address, h.port, identity.clone());
                    c.server_cert = cert;
                    tasks.push(tokio::spawn(async move { (h.key(), tokio::time::timeout(Duration::from_secs(3), c.server_info()).await.ok().and_then(|r| r.ok())) }));
                }
                for t in tasks {
                    if let Ok((k, info)) = t.await {
                        let mut s = status.lock().unwrap();
                        let e = s.entry(k).or_default();
                        e.online = info.is_some();
                        if info.is_some() {
                            e.info = info;
                        }
                        e.checked = Some(Instant::now());
                    }
                }
                ctx.request_repaint();
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    fn all_hosts(&self) -> Vec<HostRef> {
        let mut v: Vec<HostRef> = self
            .store
            .hosts
            .iter()
            .map(|k| HostRef { name: if k.name.trim().is_empty() { k.address.clone() } else { k.name.clone() }, address: k.address.clone(), port: k.http_port })
            .collect();
        for d in self.discovered.lock().unwrap().iter() {
            // A discovered host may already be saved under another address (same name).
            if !v.iter().any(|h| h.key() == d.key() || (h.name == d.name && h.port == d.port)) {
                v.push(d.clone());
            }
        }
        v
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn is_paired(&self, h: &HostRef) -> bool {
        self.store.find(&h.address, h.port).is_some()
    }

    // ------------------------------------------------------------ actions

    fn start_pairing(&mut self, h: HostRef) {
        let pin = format!("{:04}", rand::random::<u16>() % 10000);
        let mut c = client_for(&self.store, &h);
        c.server_cert = None;
        let (tx, p, hr) = (self.tx.clone(), pin.clone(), h.clone());
        let device = std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "2moon".into());
        self.rt.spawn(async move {
            let r = async {
                c.pair(&p, &device).await?;
                let info = c.server_info().await?;
                Ok(KnownHost {
                    name: info.hostname.clone(),
                    uuid: info.uuid.clone(),
                    address: c.host.clone(),
                    http_port: c.http_port,
                    server_cert_pem: moon_proto::crypto::pem_encode_cert(c.server_cert.as_ref().unwrap()),
                })
            }
            .await;
            let _ = tx.send(Msg::Paired(hr, r));
        });
        self.screen = Screen::Pairing { host: h, pin };
    }

    fn open_apps(&mut self, h: HostRef) {
        let c = client_for(&self.store, &h);
        let (tx, hr) = (self.tx.clone(), h.clone());
        self.rt.spawn(async move {
            let r = c.app_list().await;
            let _ = tx.send(Msg::Apps(hr, r));
        });
        self.screen = Screen::Apps { host: h, apps: None, error: None };
    }

    fn launch(&mut self, ctx: &egui::Context, h: HostRef, app_id: u32) {
        let s = &self.settings;
        let stream = StreamConfig {
            width: s.width,
            height: s.height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            audio_channels: s.audio_channels,
            audio_mask: match s.audio_channels {
                6 => 0x3f,
                8 => 0x63f,
                _ => 0x3,
            },
            ..StreamConfig::default()
        };
        let opts = SessionOptions { stream, app_id: Some(app_id), audio_output: true, decode: true, codec: s.codec(), hw_decode: s.hw_decode, mic: s.mic, yuv444: s.yuv444, hdr: s.hdr, zero_copy: Some(self.video.zero_copy_flag()) };
        // Decoded YUV frames go straight to the GPU renderer: no CPU colour conversion.
        let generation = self.video.reset();
        let (view, sink_ctx) = (self.video.clone(), ctx.clone());
        let sink = Box::new(move |f: crate::session::Decoded<'_>, _index: u32| {
            view.put(generation, f);
            sink_ctx.request_repaint();
        });
        let c = client_for(&self.store, &h);
        let (tx, hr) = (self.tx.clone(), h.clone());
        self.rt.spawn(async move {
            let r = Session::start(&c, opts, sink).await;
            let _ = tx.send(Msg::Started(hr, Box::new(r)));
        });
        self.screen = Screen::Connecting { host: h, since: Instant::now() };
    }

    fn quit_app(&mut self, h: &HostRef) {
        let c = client_for(&self.store, h);
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let _ = tx.send(Msg::Quit(c.cancel().await));
        });
    }

    fn end_stream(&mut self, ctx: &egui::Context, why: Option<String>) {
        if let Some(st) = self.stream.take() {
            for vk in st.pressed.iter() {
                st.session.control.input(&In::Key { down: false, vk: *vk, modifiers: 0 });
            }
            let quit = self.settings.quit_app_on_exit;
            let c = client_for(&self.store, &st.host);
            let summary = st.session.stats.summary();
            tracing::info!("stream ended: {summary}");
            std::thread::spawn(move || st.session.stop());
            self.video.reset();
            if quit {
                self.rt.spawn(async move {
                    let _ = c.cancel().await;
                });
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::None));
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("2moon".into()));
        if let Some(w) = why {
            self.toast(w);
        }
        self.screen = Screen::Home;
    }

    fn handle_messages(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Paired(h, Ok(k)) => {
                    let name = k.name.clone();
                    let _ = self.store.upsert(k);
                    self.toast(format!("Appairé avec {name}"));
                    self.open_apps(h);
                }
                Msg::Paired(_, Err(e)) => {
                    self.toast(format!("Échec de l'appairage : {e:#}"));
                    self.screen = Screen::Home;
                }
                Msg::Apps(h, r) => {
                    if let Screen::Apps { host, apps, error } = &mut self.screen {
                        if *host == h {
                            match r {
                                Ok(a) => *apps = Some(a),
                                Err(e) => *error = Some(format!("{e:#}")),
                            }
                        }
                    }
                }
                Msg::Started(h, r) => match *r {
                    Ok(session) => {
                        let fullscreen = self.settings.fullscreen;
                        if fullscreen {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("2moon — {}", h.name)));
                        self.stream = Some(Stream {
                            session,
                            host: h,
                            captured: false,
                            fullscreen,
                            pressed: HashSet::new(),
                            mods: egui::Modifiers::NONE,
                            mic_toast: None,
                            fps_window: (Instant::now(), 0, 0.0),
                        });
                        self.screen = Screen::Streaming;
                    }
                    Err(e) => {
                        self.toast(format!("Impossible de démarrer le stream : {e:#}"));
                        self.screen = Screen::Home;
                    }
                },
                Msg::Quit(r) => match r {
                    Ok(()) => self.toast("Application fermée sur l'hôte"),
                    Err(e) => self.toast(format!("Échec : {e:#}")),
                },
            }
        }
    }
}

// ---------------------------------------------------------------- UI

impl eframe::App for GuiApp {
    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let Some(gl) = gl {
            self.video.destroy(gl);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_messages(&ctx);
        // Keep the host poller's target list current (not while streaming: this runs every frame).
        if !matches!(self.screen, Screen::Streaming) {
            let list: Vec<(HostRef, Option<Vec<u8>>)> = self
                .all_hosts()
                .into_iter()
                .map(|h| {
                    let cert = self.store.find(&h.address, h.port).and_then(|k| moon_proto::crypto::pem_decode_cert(&k.server_cert_pem).ok());
                    (h, cert)
                })
                .collect();
            *self.targets.lock().unwrap() = list;
        }

        if matches!(self.screen, Screen::Streaming) {
            self.stream_ui(ui, &ctx);
            return;
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("2moon").size(20.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !matches!(self.screen, Screen::Settings) && ui.button("Réglages").clicked() {
                        self.screen = Screen::Settings;
                    }
                    if !matches!(self.screen, Screen::Home) && ui.button("Accueil").clicked() {
                        self.screen = Screen::Home;
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| match &self.screen {
            Screen::Home => self.home_ui(ui),
            Screen::Pairing { host, pin } => {
                let (host, pin) = (host.clone(), pin.clone());
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.heading(format!("Appairage avec {}", host.name));
                    ui.add_space(20.0);
                    ui.label("Saisissez ce code sur l'hôte (panneau d'administration 2moon ou interface Sunshine) :");
                    ui.add_space(10.0);
                    ui.label(RichText::new(&pin).size(64.0).monospace().strong());
                    ui.add_space(20.0);
                    ui.spinner();
                    ui.label(RichText::new("En attente de l'hôte…").weak());
                    ui.add_space(20.0);
                    if ui.button("Annuler").clicked() {
                        self.screen = Screen::Home;
                    }
                });
            }
            Screen::Apps { host, apps, error } => {
                let (host, apps, error) = (host.clone(), apps.clone(), error.clone());
                self.apps_ui(ui, &ctx, host, apps, error);
            }
            Screen::Settings => self.settings_ui(ui),
            Screen::Connecting { host, since } => {
                let (name, secs) = (host.name.clone(), since.elapsed().as_secs());
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.spinner();
                    ui.heading(format!("Connexion à {name}…"));
                    ui.label(RichText::new(format!("{secs} s")).weak());
                });
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Screen::Streaming => {}
        });

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(4) {
                egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -20.0]).show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(msg));
                });
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.toast = None;
            }
        }
    }
}

impl GuiApp {
    fn home_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Ordinateurs");
        ui.label(RichText::new("Hôtes 2moon / Sunshine / GeForce Experience trouvés sur le réseau et hôtes enregistrés.").weak());
        ui.add_space(10.0);
        let hosts = self.all_hosts();
        let status = self.status.lock().unwrap().clone();
        let mut action: Option<(u8, HostRef)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for h in &hosts {
                    let st = status.get(&h.key()).cloned().unwrap_or_default();
                    let paired = self.is_paired(h);
                    egui::Frame::group(ui.style()).inner_margin(14.0).corner_radius(10.0).show(ui, |ui| {
                        ui.set_width(230.0);
                        ui.vertical(|ui| {
                            monitor_icon(ui, 34.0);
                            ui.label(RichText::new(&h.name).size(17.0).strong());
                            ui.label(RichText::new(format!("{}:{}", h.address, h.port)).weak().small());
                            let (txt, col) = match (&st.checked, st.online, &st.info) {
                                (None, _, _) => ("…", Color32::GRAY),
                                (_, false, _) => ("Hors ligne", Color32::from_rgb(220, 90, 90)),
                                (_, true, Some(i)) if i.busy && i.current_game != 0 => ("En jeu", Color32::from_rgb(240, 180, 40)),
                                _ => ("En ligne", Color32::from_rgb(60, 200, 130)),
                            };
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                                ui.painter().circle_filled(r.center(), 4.5, col);
                                ui.label(txt);
                                if paired {
                                    ui.label(RichText::new("· appairé").weak());
                                }
                            });
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if paired {
                                    if ui.add_enabled(st.online, egui::Button::new("Ouvrir")).clicked() {
                                        action = Some((0, h.clone()));
                                    }
                                } else if ui.add_enabled(st.online, egui::Button::new("Appairer")).clicked() {
                                    action = Some((1, h.clone()));
                                }
                                if paired && ui.small_button("Oublier").on_hover_text("Supprimer cet hôte de la liste").clicked() {
                                    action = Some((2, h.clone()));
                                }
                            });
                        });
                    });
                }
            });
            if hosts.is_empty() {
                ui.label(RichText::new("Recherche d'hôtes sur le réseau…").weak());
            }
            ui.add_space(20.0);
            ui.separator();
            ui.label("Ajouter un hôte manuellement (adresse IP ou nom, port optionnel) :");
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut self.manual).hint_text("192.168.1.85:47989").desired_width(240.0));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui.button("Ajouter et appairer").clicked() || enter) && !self.manual.trim().is_empty() {
                    let (addr, port) = crate::split_host_port(self.manual.trim());
                    action = Some((1, HostRef { name: addr.clone(), address: addr, port }));
                    self.manual.clear();
                }
            });
        });
        match action {
            Some((0, h)) => self.open_apps(h),
            Some((1, h)) => self.start_pairing(h),
            Some((2, h)) => {
                self.store.hosts.retain(|k| !(k.address == h.address && k.http_port == h.port));
                let _ = self.store.save();
                self.toast(format!("{} oublié", h.name));
            }
            _ => {}
        }
    }

    fn apps_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, host: HostRef, apps: Option<Vec<HostApp>>, error: Option<String>) {
        ui.add_space(8.0);
        ui.heading(&host.name);
        let current = self.status.lock().unwrap().get(&host.key()).and_then(|s| s.info.clone()).map(|i| i.current_game).unwrap_or(0);
        let s = &self.settings;
        ui.label(RichText::new(format!("{}×{} · {} fps · {} Mbit/s · codec {} · audio {} canaux", s.width, s.height, s.fps, s.bitrate_kbps / 1000, s.codec, s.audio_channels)).weak());
        ui.add_space(10.0);
        if let Some(e) = error {
            ui.colored_label(Color32::from_rgb(220, 90, 90), e);
            return;
        }
        let Some(apps) = apps else {
            ui.spinner();
            return;
        };
        let mut launch = None;
        let mut quit = false;
        ui.horizontal_wrapped(|ui| {
            for a in &apps {
                egui::Frame::group(ui.style()).inner_margin(14.0).corner_radius(10.0).show(ui, |ui| {
                    ui.set_width(200.0);
                    ui.vertical(|ui| {
                        gamepad_icon(ui, 30.0);
                        ui.label(RichText::new(&a.title).size(16.0).strong());
                        if current == a.id {
                            ui.label(RichText::new("en cours").color(Color32::from_rgb(240, 180, 40)));
                        }
                        ui.horizontal(|ui| {
                            let label = if current == a.id { "Reprendre" } else { "Lancer" };
                            if ui.button(label).clicked() {
                                launch = Some(a.id);
                            }
                            if current == a.id && ui.button("Quitter l'app").clicked() {
                                quit = true;
                            }
                        });
                    });
                });
            }
        });
        if let Some(id) = launch {
            self.launch(ctx, host.clone(), id);
        }
        if quit {
            self.quit_app(&host);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Réglages du stream");
        ui.add_space(10.0);
        let before = self.settings.clone();
        let s = &mut self.settings;
        egui::Grid::new("settings").num_columns(2).spacing([24.0, 12.0]).show(ui, |ui| {
            ui.label("Résolution");
            egui::ComboBox::from_id_salt("res").selected_text(format!("{}×{}", s.width, s.height)).show_ui(ui, |ui| {
                for (w, h, n) in [(1280, 720, "720p"), (1600, 900, "900p"), (1920, 1080, "1080p"), (2560, 1440, "1440p"), (3840, 2160, "4K")] {
                    if ui.selectable_label(s.width == w && s.height == h, format!("{n} ({w}×{h})")).clicked() {
                        s.width = w;
                        s.height = h;
                    }
                }
            });
            ui.end_row();
            ui.label("Images par seconde");
            ui.horizontal(|ui| {
                for f in [30, 60, 90, 120, 144] {
                    ui.selectable_value(&mut s.fps, f, f.to_string());
                }
            });
            ui.end_row();
            ui.label("Débit vidéo");
            ui.horizontal(|ui| {
                let mut mbps = s.bitrate_kbps as f32 / 1000.0;
                if ui.add(egui::Slider::new(&mut mbps, 2.0..=150.0).suffix(" Mbit/s").logarithmic(true)).changed() {
                    s.bitrate_kbps = (mbps * 1000.0) as u32;
                }
                let sugg = s.suggested_bitrate();
                if ui.small_button(format!("conseillé : {}", sugg / 1000)).clicked() {
                    s.bitrate_kbps = sugg;
                }
            });
            ui.end_row();
            ui.label("Codec");
            ui.horizontal(|ui| {
                for (v, n) in [("auto", "Automatique"), ("h264", "H.264"), ("hevc", "HEVC"), ("av1", "AV1")] {
                    ui.selectable_value(&mut s.codec, v.to_string(), n);
                }
            });
            ui.end_row();
            ui.label("Couleurs");
            ui.checkbox(&mut s.yuv444, "4:4:4 (texte plus net ; plus de débit ; H.264 / HEVC si l'hôte le propose)");
            ui.end_row();
            ui.label("HDR");
            ui.checkbox(&mut s.hdr, "Accepter la HDR / 10 bits (HEVC ou AV1 ; moins d'effets d'escalier ; affichée en SDR)");
            ui.end_row();
            ui.label("Audio");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut s.audio_channels, 2, "Stéréo");
                ui.selectable_value(&mut s.audio_channels, 6, "5.1");
                ui.selectable_value(&mut s.audio_channels, 8, "7.1");
            });
            ui.end_row();
            ui.label("Décodage");
            ui.checkbox(&mut s.hw_decode, "Décoder sur le GPU si possible (NVIDIA : NVDEC, Intel/AMD : VAAPI)");
            ui.end_row();
            ui.label("Affichage");
            ui.checkbox(&mut s.fullscreen, "Plein écran au démarrage du stream");
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut s.show_stats, "Afficher les statistiques pendant le stream");
            ui.end_row();
            ui.label("Micro");
            ui.checkbox(&mut s.mic, "Envoyer mon micro à l'hôte (hôtes 2moon ; il devient le micro par défaut de l'hôte pendant le stream)");
            ui.end_row();
            ui.label("Synchro verticale");
            ui.checkbox(&mut s.vsync, "V-Sync (pas de déchirement, mais plus de latence ; appliqué au prochain démarrage)");
            ui.end_row();
            ui.label("Fin de session");
            ui.checkbox(&mut s.quit_app_on_exit, "Quitter l'application sur l'hôte en fermant le stream");
            ui.end_row();
        });
        ui.add_space(20.0);
        ui.label(RichText::new("Pendant le stream : Ctrl+Alt+Maj+Q revenir à l'accueil · +Z capturer la souris · +F plein écran · +S statistiques · +M couper le micro").weak());
        if format!("{:?}", before) != format!("{:?}", self.settings) {
            self.settings.save(&self.store);
        }
    }

    // ------------------------------------------------------------ streaming

    fn stream_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(st) = self.stream.as_mut() else {
            self.screen = Screen::Home;
            return;
        };
        // Session events.
        let mut ended: Option<String> = None;
        while let Ok(ev) = st.session.control_events.try_recv() {
            match ev {
                ControlEvent::Terminated(code) => ended = Some(format!("L'hôte a terminé la session (0x{code:08x})")),
                ControlEvent::Message(0x010e, payload) => {
                    if let Some((enabled, m)) = moon_media::hdr::HdrMetadata::parse(&payload) {
                        tracing::info!("host HDR mode: {}", if enabled { "on" } else { "off" });
                        if enabled {
                            self.video.set_hdr(m);
                        }
                    }
                }
                ControlEvent::Disconnected => ended = Some("Connexion à l'hôte perdue".into()),
                _ => {}
            }
        }
        let size = self.video.size();
        if st.fps_window.0.elapsed() >= Duration::from_secs(1) {
            let n = self.video.presented();
            st.fps_window = (Instant::now(), n, n.saturating_sub(st.fps_window.1) as f64 / st.fps_window.0.elapsed().as_secs_f64());
        }

        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let video = if size[0] > 0 {
            let (sw, sh) = (size[0] as f32, size[1] as f32);
            let scale = (rect.width() / sw).min(rect.height() / sh);
            egui::Rect::from_center_size(rect.center(), Vec2::new(sw * scale, sh * scale))
        } else {
            rect
        };
        if size[0] > 0 {
            // Snap to whole pixels so the video is not resampled more than needed.
            let ppp = ctx.pixels_per_point();
            let snap = |p: egui::Pos2| egui::pos2((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp);
            self.video.paint(ui.painter(), egui::Rect::from_min_max(snap(video.min), snap(video.max)));
        } else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "En attente de la première image…", egui::FontId::proportional(18.0), Color32::GRAY);
        }
        if let Some(t) = st.mic_toast.filter(|t| t.elapsed() < Duration::from_secs(2)) {
            let muted = st.session.mic_muted.load(Ordering::Relaxed);
            let (txt, col) = if muted { ("Micro coupé", Color32::from_rgb(240, 120, 90)) } else { ("Micro actif", Color32::from_rgb(120, 255, 160)) };
            ui.painter().text(rect.center_top() + Vec2::new(0.0, 30.0), egui::Align2::CENTER_TOP, txt, egui::FontId::proportional(22.0), col);
            ctx.request_repaint_after(Duration::from_secs(2).saturating_sub(t.elapsed()));
        }
        if self.settings.show_stats {
            let mic = match (st.session.mic_active, st.session.mic_muted.load(Ordering::Relaxed)) {
                (false, _) => String::new(),
                (true, true) => " · micro coupé".into(),
                (true, false) => format!(" · micro {} paquets", st.session.stats.mic_packets.load(Ordering::Relaxed)),
            };
            let txt = format!("{:.0} fps affichées · {}{mic}", st.fps_window.2, short_stats(&st.session.stats));
            ui.painter().text(rect.left_top() + Vec2::new(10.0, 10.0), egui::Align2::LEFT_TOP, txt, egui::FontId::monospace(13.0), Color32::from_rgb(120, 255, 160));
        }
        // Make sure the whole area receives pointer input.
        let _ = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        // The host's cursor is drawn in the video: hide ours over it (and always
        // when captured). Set every frame, or egui would show it again.
        let over_video = ctx.input(|i| i.pointer.latest_pos()).is_some_and(|p| video.contains(p));
        if st.captured || (over_video && size[0] > 0) {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }

        // Input forwarding.
        let (events, focused) = ctx.input(|i| (i.events.clone(), i.viewport().focused.unwrap_or(true)));
        let ctl = st.session.control.clone();
        let mut exit = false;
        if !focused && (!st.pressed.is_empty() || st.captured) {
            release_all(st, &ctl);
            if st.captured {
                st.captured = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::None));
            }
        }
        for ev in events {
            match ev {
                egui::Event::Key { key, physical_key, pressed, repeat, modifiers } => {
                    let k = physical_key.unwrap_or(key);
                    let combo = modifiers.ctrl && modifiers.alt && modifiers.shift;
                    if combo && pressed && !repeat {
                        match k {
                            Key::Q => {
                                exit = true;
                                continue;
                            }
                            Key::Z => {
                                release_all(st, &ctl);
                                st.captured = !st.captured;
                                ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(if st.captured { egui::CursorGrab::Locked } else { egui::CursorGrab::None }));
                                continue;
                            }
                            Key::F => {
                                st.fullscreen = !st.fullscreen;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(st.fullscreen));
                                continue;
                            }
                            Key::S => {
                                self.settings.show_stats = !self.settings.show_stats;
                                continue;
                            }
                            Key::M if st.session.mic_active => {
                                let m = !st.session.mic_muted.load(Ordering::Relaxed);
                                st.session.mic_muted.store(m, Ordering::Relaxed);
                                st.mic_toast = Some(Instant::now());
                                continue;
                            }
                            _ => {}
                        }
                    }
                    sync_modifiers(st, &ctl, modifiers);
                    if repeat {
                        continue;
                    }
                    if let Some(vk) = egui_key_to_vk(k) {
                        if pressed {
                            st.pressed.insert(vk);
                        } else if !st.pressed.remove(&vk) {
                            continue;
                        }
                        ctl.input(&In::Key { down: pressed, vk, modifiers: mod_bits(modifiers) });
                    }
                }
                egui::Event::ModifiersChanged(m) => sync_modifiers(st, &ctl, m),
                egui::Event::PointerMoved(pos) if !st.captured && size[0] > 0 => {
                    if video.contains(pos) {
                        let x = ((pos.x - video.min.x) / video.width() * size[0] as f32) as i16;
                        let y = ((pos.y - video.min.y) / video.height() * size[1] as f32) as i16;
                        ctl.input(&In::MouseAbs { x, y, width: size[0] as i16, height: size[1] as i16 });
                    }
                }
                egui::Event::MouseMoved(d) if st.captured => {
                    let (dx, dy) = (d.x.round() as i32, d.y.round() as i32);
                    if dx != 0 || dy != 0 {
                        ctl.input(&In::MouseRel { dx: dx.clamp(-32768, 32767) as i16, dy: dy.clamp(-32768, 32767) as i16 });
                    }
                }
                egui::Event::PointerButton { button, pressed, .. } => {
                    let b = match button {
                        egui::PointerButton::Primary => inp::BUTTON_LEFT,
                        egui::PointerButton::Secondary => inp::BUTTON_RIGHT,
                        egui::PointerButton::Middle => inp::BUTTON_MIDDLE,
                        egui::PointerButton::Extra1 => inp::BUTTON_X1,
                        egui::PointerButton::Extra2 => inp::BUTTON_X2,
                    };
                    ctl.input(&In::MouseButton { down: pressed, button: b });
                }
                egui::Event::MouseWheel { unit, delta, .. } => {
                    let scale = match unit {
                        egui::MouseWheelUnit::Line => 120.0,
                        egui::MouseWheelUnit::Page => 360.0,
                        egui::MouseWheelUnit::Point => 2.0,
                    };
                    let (x, y) = ((delta.x * scale) as i32, (delta.y * scale) as i32);
                    if y != 0 {
                        ctl.input(&In::Scroll { amount: y.clamp(-32768, 32767) as i16 });
                    }
                    if x != 0 {
                        ctl.input(&In::HScroll { amount: x.clamp(-32768, 32767) as i16 });
                    }
                }
                _ => {}
            }
        }
        if exit || ended.is_some() {
            self.end_stream(ctx, ended);
        }
    }
}

/// Small vector icons (the bundled font has no emoji).
fn monitor_icon(ui: &mut egui::Ui, h: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(h * 1.4, h), egui::Sense::hover());
    let p = ui.painter();
    let col = ui.visuals().widgets.active.fg_stroke.color;
    let screen = egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, r.min.y + h * 0.72));
    p.rect_stroke(screen, 3.0, egui::Stroke::new(2.0, col), egui::StrokeKind::Inside);
    p.line_segment([egui::pos2(r.center().x, screen.max.y), egui::pos2(r.center().x, r.max.y - 2.0)], egui::Stroke::new(2.0, col));
    p.line_segment([egui::pos2(r.center().x - h * 0.3, r.max.y - 2.0), egui::pos2(r.center().x + h * 0.3, r.max.y - 2.0)], egui::Stroke::new(2.0, col));
}

fn gamepad_icon(ui: &mut egui::Ui, h: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(h * 1.6, h), egui::Sense::hover());
    let p = ui.painter();
    let col = ui.visuals().widgets.active.fg_stroke.color;
    let body = r.shrink2(Vec2::new(0.0, h * 0.15));
    p.rect_stroke(body, h * 0.35, egui::Stroke::new(2.0, col), egui::StrokeKind::Inside);
    let c = body.left_center() + Vec2::new(h * 0.45, 0.0);
    p.line_segment([c - Vec2::new(h * 0.15, 0.0), c + Vec2::new(h * 0.15, 0.0)], egui::Stroke::new(2.0, col));
    p.line_segment([c - Vec2::new(0.0, h * 0.15), c + Vec2::new(0.0, h * 0.15)], egui::Stroke::new(2.0, col));
    let b = body.right_center() - Vec2::new(h * 0.45, 0.0);
    p.circle_filled(b + Vec2::new(h * 0.1, -h * 0.08), h * 0.06, col);
    p.circle_filled(b - Vec2::new(h * 0.1, -h * 0.08), h * 0.06, col);
}

fn short_stats(s: &Stats) -> String {
    format!(
        "{} · décodage {:.1} ms · décodées {} · jetées {} · perdues {} · FEC {} · IDR {} · RFI {}",
        s.decoder.lock().unwrap(),
        {
            let n = s.frames_decoded.load(Ordering::Relaxed);
            if n > 0 { s.decode_us_total.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0 } else { 0.0 }
        },
        s.frames_decoded.load(Ordering::Relaxed),
        s.frames_dropped.load(Ordering::Relaxed),
        s.frames_lost.load(Ordering::Relaxed),
        s.shards_recovered.load(Ordering::Relaxed),
        s.idr_requests.load(Ordering::Relaxed),
        s.rfi_requests.load(Ordering::Relaxed)
    )
}

fn mod_bits(m: egui::Modifiers) -> u8 {
    (m.shift as u8) * inp::MOD_SHIFT | (m.ctrl as u8) * inp::MOD_CTRL | (m.alt as u8) * inp::MOD_ALT | (m.mac_cmd as u8) * inp::MOD_META
}

/// egui does not emit key events for modifier keys: synthesize them from modifier changes.
fn sync_modifiers(st: &mut Stream, ctl: &ControlSender, m: egui::Modifiers) {
    for (was, now, vk) in [(st.mods.shift, m.shift, 0xA0u16), (st.mods.ctrl, m.ctrl, 0xA2), (st.mods.alt, m.alt, 0xA4)] {
        if was != now {
            if now {
                st.pressed.insert(vk);
            } else {
                st.pressed.remove(&vk);
            }
            ctl.input(&In::Key { down: now, vk, modifiers: mod_bits(m) });
        }
    }
    st.mods = m;
}

fn release_all(st: &mut Stream, ctl: &ControlSender) {
    for vk in st.pressed.drain() {
        ctl.input(&In::Key { down: false, vk, modifiers: 0 });
    }
    st.mods = egui::Modifiers::NONE;
}

/// Map an egui key (physical position when available) to a Windows virtual-key code.
fn egui_key_to_vk(k: Key) -> Option<u16> {
    use Key::*;
    let letters = [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z];
    if let Some(i) = letters.iter().position(|&c| c == k) {
        return Some(0x41 + i as u16);
    }
    let digits = [Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9];
    if let Some(i) = digits.iter().position(|&c| c == k) {
        return Some(0x30 + i as u16);
    }
    let fkeys = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20];
    if let Some(i) = fkeys.iter().position(|&c| c == k) {
        return Some(0x70 + i as u16);
    }
    Some(match k {
        Backspace => 0x08,
        Tab => 0x09,
        Enter => 0x0D,
        Escape => 0x1B,
        Space => 0x20,
        PageUp => 0x21,
        PageDown => 0x22,
        End => 0x23,
        Home => 0x24,
        ArrowLeft => 0x25,
        ArrowUp => 0x26,
        ArrowRight => 0x27,
        ArrowDown => 0x28,
        Insert => 0x2D,
        Delete => 0x2E,
        Semicolon | Colon => 0xBA,
        Equals | Plus => 0xBB,
        Comma => 0xBC,
        Minus => 0xBD,
        Period => 0xBE,
        Slash | Questionmark => 0xBF,
        Backtick => 0xC0,
        OpenBracket | OpenCurlyBracket => 0xDB,
        Backslash | Pipe => 0xDC,
        CloseBracket | CloseCurlyBracket => 0xDD,
        Quote => 0xDE,
        _ => return None,
    })
}
