//! Graphical client: host list (mDNS + saved hosts), pairing, app list,
//! settings, and the stream itself rendered as a GPU texture.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, pos2, vec2, Align, Color32, Key, Layout, Rect, RichText, Sense, Stroke, Vec2};
use moon_media::encoder::Codec;
use moon_proto::control::input::{self as inp, Event as In};
use moon_proto::xml::App as HostApp;
use serde::{Deserialize, Serialize};

use crate::control::{ControlEvent, ControlSender};
use crate::http::{GsClient, ServerInfo};
use crate::rtsp::StreamConfig;
use crate::session::{Session, SessionOptions, Stats};
use crate::store::{KnownHost, Store};
use crate::theme::{self as th, Look, Scene};
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
    /// Which microphone (input device id); None = the system default.
    pub mic_device: Option<String>,
    /// 4:4:4 chroma when available (sharper text, more bandwidth).
    pub yuv444: bool,
    /// Accept HDR (10-bit) streams, tone-mapped to this SDR window.
    pub hdr: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { width: 1920, height: 1080, fps: 60, bitrate_kbps: 20_000, codec: "auto".into(), audio_channels: 2, hw_decode: true, fullscreen: false, quit_app_on_exit: false, show_stats: false, vsync: false, mic: false, mic_device: None, yuv444: false, hdr: false }
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
    /// Always Some until drop (see `Drop for GuiApp`).
    rt: Option<tokio::runtime::Runtime>,
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
    look: Look,
    /// Home: the "add a host" panel is open.
    show_add: bool,
    /// Settings: section to scroll to, and where each section was last frame.
    settings_jump: Option<usize>,
    section_tops: [f32; 4],
    /// Height of the pairing / connecting dialog last frame (to place it).
    dialog_h: f32,
    /// Microphones of this machine, listed when the settings open.
    mics: Option<Vec<moon_media::audio_io::InputDevice>>,
}

pub fn run(store: Store) -> Result<()> {
    let vsync = Settings::load(&store).vsync;
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("2moon").with_inner_size([1180.0, 760.0]).with_min_inner_size([640.0, 400.0]).with_app_id("2moon"),
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

impl Drop for GuiApp {
    fn drop(&mut self) {
        // The GUI runs inside `#[tokio::main]`: dropping a runtime there panics.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

impl GuiApp {
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.rt.as_ref().expect("tokio runtime")
    }

    fn new(cc: &eframe::CreationContext<'_>, store: Store) -> Self {
        let look = Look::new(&cc.egui_ctx);
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        let (tx, rx) = mpsc::channel();
        let settings = Settings::load(&store);
        let app = Self {
            rt: Some(rt),
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
            look,
            show_add: false,
            settings_jump: None,
            section_tops: [f32::MAX; 4],
            dialog_h: 520.0,
            mics: None,
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
        self.rt().spawn(async move {
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
        let device = crate::device_name();
        self.rt().spawn(async move {
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
        self.rt().spawn(async move {
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
        let opts = SessionOptions { stream, app_id: Some(app_id), audio_output: true, decode: true, codec: s.codec(), hw_decode: s.hw_decode, mic: s.mic, mic_device: s.mic_device.clone(), yuv444: s.yuv444, hdr: s.hdr, zero_copy: Some(self.video.zero_copy_flag()) };
        // Decoded YUV frames go straight to the GPU renderer: no CPU colour conversion.
        let generation = self.video.reset();
        let (view, sink_ctx) = (self.video.clone(), ctx.clone());
        let sink = Box::new(move |f: crate::session::Decoded<'_>, _index: u32| {
            view.put(generation, f);
            sink_ctx.request_repaint();
        });
        let c = client_for(&self.store, &h);
        let (tx, hr) = (self.tx.clone(), h.clone());
        self.rt().spawn(async move {
            let r = Session::start(&c, opts, sink).await;
            let _ = tx.send(Msg::Started(hr, Box::new(r)));
        });
        self.screen = Screen::Connecting { host: h, since: Instant::now() };
    }

    fn quit_app(&mut self, h: &HostRef) {
        let c = client_for(&self.store, h);
        let tx = self.tx.clone();
        self.rt().spawn(async move {
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
                self.rt().spawn(async move {
                    let _ = c.cancel().await;
                });
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(egui::CursorGrab::None));
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("2moon".into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        // The decoder was what asked for repaints: without this the window keeps
        // showing the last video frame until the next input event.
        ctx.request_repaint();
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

        egui::CentralPanel::no_frame().show(ui, |ui| {
            let full = ui.max_rect();
            let scene = match self.screen {
                Screen::Home => Scene::Home,
                Screen::Pairing { .. } | Screen::Connecting { .. } => Scene::Centered,
                _ => Scene::Side,
            };
            self.look.paint_backdrop(ui.painter(), full, scene);
            if scene == Scene::Centered {
                self.dialog_ui(ui, full);
                return;
            }
            let header = Rect::from_min_size(full.min, vec2(full.width(), 76.0));
            self.header_ui(ui, header);
            let body = Rect::from_min_max(pos2(full.left(), header.bottom()), full.max);
            let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(body).layout(Layout::top_down(Align::Min)));
            match &self.screen {
                Screen::Home => self.home_ui(&mut bui),
                Screen::Apps { host, apps, error } => {
                    let (host, apps, error) = (host.clone(), apps.clone(), error.clone());
                    self.apps_ui(&mut bui, &ctx, host, apps, error);
                }
                Screen::Settings => self.settings_ui(&mut bui, body),
                _ => {}
            }
        });

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(4) {
                egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::CENTER_BOTTOM, [0.0, -24.0]).show(&ctx, |ui| {
                    egui::Frame::new().fill(th::SURFACE).stroke(Stroke::new(1.0, th::BORDER)).corner_radius(255).inner_margin(egui::Margin::symmetric(18, 12)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            th::phase(ui, 14.0, 2, th::ACCENT);
                            ui.label(RichText::new(msg).font(th::body(14.0)).color(th::TEXT2));
                        });
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(200));
            } else {
                self.toast = None;
            }
        }
    }
}

/// A centred column of at most `max_w` (side padding 40, 20 when narrow); `add` gets its width.
fn column<R>(ui: &mut egui::Ui, max_w: f32, add: impl FnOnce(&mut egui::Ui, f32) -> R) -> R {
    let avail = ui.available_width();
    let pad = if avail < 800.0 { 20.0 } else { 40.0 };
    let w = (avail - 2.0 * pad).min(max_w).max(200.0);
    let side = ((avail - w) / 2.0).max(0.0);
    let spacing = ui.spacing().item_spacing;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = spacing;
            ui.set_width(w);
            add(ui, w)
        })
        .inner
    })
    .inner
}

/// Lay `items` out in a grid of equal columns (CSS `repeat(auto-fill, minmax(min_w, 1fr))`).
fn grid<T>(ui: &mut egui::Ui, items: &[T], min_w: f32, gap: f32, mut cell: impl FnMut(&mut egui::Ui, &T)) {
    let w = ui.available_width();
    let cols = (((w + gap) / (min_w + gap)).floor() as usize).max(1);
    let cw = (w - gap * (cols - 1) as f32) / cols as f32;
    for row in items.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for it in row {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.set_width(cw);
                    cell(ui, it);
                });
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
}

/// Bold label with an optional muted description under it.
fn label_desc(ui: &mut egui::Ui, label: &str, desc: Option<&str>) {
    ui.spacing_mut().item_spacing.y = 2.0;
    th::text(ui, label, th::semibold(15.0), th::TEXT);
    if let Some(d) = desc {
        th::text(ui, d, th::body(13.0), th::MUTED);
    }
}

/// One settings row (padding 22×28, divider under it unless last).
fn row(ui: &mut egui::Ui, last: bool, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 22)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = vec2(10.0, 8.0);
        add(ui);
    });
    if !last {
        th::rule(ui, th::DIVIDER);
    }
}

/// Height of `label_desc` at width `w`.
fn label_desc_height(ui: &egui::Ui, label: &str, desc: Option<&str>, w: f32) -> f32 {
    let p = ui.painter();
    let mut h = p.layout(label.into(), th::semibold(15.0), th::TEXT, w).size().y;
    if let Some(d) = desc {
        h += 2.0 + p.layout(d.into(), th::body(13.0), th::MUTED, w).size().y;
    }
    h
}

/// Label (+ description) on the left, `right` (of natural size `right_size`) on the right,
/// both vertically centred; stacked when too narrow.
fn split_row(ui: &mut egui::Ui, right_size: Vec2, label: &str, desc: Option<&str>, right: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    if right_size.x + 24.0 + 180.0 > w {
        label_desc(ui, label, desc);
        ui.add_space(6.0);
        right(ui);
        return;
    }
    let lw = w - right_size.x - 24.0;
    let lh = label_desc_height(ui, label, desc, lw);
    let h = lh.max(right_size.y);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(lw, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(lw);
            ui.add_space((h - lh) / 2.0);
            label_desc(ui, label, desc);
        });
        ui.allocate_ui_with_layout(vec2(ui.available_width(), h), Layout::right_to_left(Align::Center), |ui| {
            ui.set_height(h);
            right(ui);
        });
    });
}

fn seg_row<T: PartialEq + Clone>(ui: &mut egui::Ui, last: bool, label: &str, value: &mut T, opts: &[(T, &str)], mono: bool, min_w: f32) {
    row(ui, last, |ui| {
        let labels: Vec<&str> = opts.iter().map(|o| o.1).collect();
        let sw = th::segmented_width(ui, &labels, mono, min_w);
        split_row(ui, vec2(sw, 50.0), label, None, |ui| {
            th::segmented(ui, value, opts, mono, min_w);
        });
    });
}

fn toggle_row(ui: &mut egui::Ui, last: bool, label: &str, desc: &str, value: &mut bool) {
    row(ui, last, |ui| {
        split_row(ui, vec2(52.0, 30.0), label, Some(desc), |ui| {
            th::switch(ui, value).on_hover_text(label);
        });
    });
}

/// A settings section: panel with a phase glyph + title header. Records where it is for the side nav.
fn section(ui: &mut egui::Ui, idx: usize, title: &str, tops: &mut [f32; 4], jump: &mut Option<usize>, add: impl FnOnce(&mut egui::Ui)) {
    let r = th::panel()
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 20)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    th::phase(ui, 18.0, idx as u8, th::ACCENT);
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).font(th::display(20.0)).color(th::TEXT));
                });
            });
            th::rule(ui, th::DIVIDER);
            add(ui);
        })
        .response
        .rect;
    tops[idx] = r.top();
    if *jump == Some(idx) {
        ui.scroll_to_rect(r, Some(Align::TOP));
        *jump = None;
    }
    ui.add_space(32.0);
}

const SECTIONS: [&str; 4] = ["Vidéo", "Audio et micro", "Affichage", "Session"];

impl GuiApp {
    fn header_ui(&mut self, ui: &mut egui::Ui, r: Rect) {
        let p = ui.painter();
        p.rect_filled(r, 0.0, th::alpha(th::BG, 0.45));
        p.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, th::white(0.08)));
        let narrow = r.width() < 800.0;
        let pad = if narrow { 20.0 } else { 40.0 };
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink2(vec2(pad, 0.0))).layout(Layout::left_to_right(Align::Center)));
        ui.spacing_mut().item_spacing.x = 8.0;

        // Logo.
        let word = ui.painter().layout_no_wrap("2moon".into(), th::display(24.0), th::TEXT);
        let (lr, logo) = ui.allocate_exact_size(vec2(54.0 + 12.0 + word.size().x, 44.0), Sense::click());
        th::paint_logo(ui.painter(), Rect::from_min_size(pos2(lr.left(), lr.center().y - 18.0), vec2(54.0, 36.0)));
        ui.painter().galley(pos2(lr.left() + 66.0, lr.center().y - word.size().y / 2.0), word, th::TEXT);
        if logo.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            self.screen = Screen::Home;
        }
        ui.add_space(if narrow { 16.0 } else { 36.0 });

        // Tabs.
        let on_settings = matches!(self.screen, Screen::Settings);
        for (label, active, to_settings) in [("Ordinateurs", !on_settings, false), ("Réglages", on_settings, true)] {
            let font = if active { th::semibold(15.0) } else { th::body(15.0) };
            let g = ui.painter().layout_no_wrap(label.into(), font, Color32::PLACEHOLDER);
            let (tr, resp) = ui.allocate_exact_size(vec2(g.size().x + 28.0, 44.0), Sense::click());
            let col = if active || resp.hovered() { th::TEXT } else { th::MUTED };
            ui.painter().galley(tr.center() - g.size() / 2.0, g, col);
            if active {
                ui.painter().rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - 2.0), tr.max), 0.0, th::ACCENT);
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !active {
                self.screen = if to_settings { Screen::Settings } else { Screen::Home };
                self.mics = None;
            }
        }

        // Right side: what is going on.
        let s = &self.settings;
        let summary = format!("{}×{} · {} ips · {} Mbit/s", s.width, s.height, s.fps, s.bitrate_kbps / 1000);
        let searching = matches!(self.screen, Screen::Home);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if searching {
                if r.width() >= 960.0 {
                    let g = th::spaced(ui, "RECHERCHE SUR LE RÉSEAU LOCAL", th::mono(12.0), th::TEXT2, 0.5);
                    let (tr, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                    ui.painter().galley(tr.min, g, th::TEXT2);
                    th::phase(ui, 14.0, 1, th::ACCENT);
                }
            } else if r.width() >= 760.0 {
                let g = ui.painter().layout_no_wrap(summary, th::mono(13.0), th::TEXT);
                let (cr, _) = ui.allocate_exact_size(g.size() + vec2(24.0, 16.0), Sense::hover());
                ui.painter().rect(cr, 10.0, th::SURFACE, Stroke::new(1.0, th::BORDER), egui::StrokeKind::Inside);
                ui.painter().galley(cr.center() - g.size() / 2.0, g, th::TEXT);
            }
        });
    }

    fn home_ui(&mut self, ui: &mut egui::Ui) {
        let hosts = self.all_hosts();
        let status = self.status.lock().unwrap().clone();
        let mut action: Option<(u8, HostRef)> = None;
        let t = self.look.t();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            column(ui, 1120.0, |ui, w| {
                ui.add_space(40.0);
                // "Add a host" toggle, right-aligned.
                ui.allocate_ui_with_layout(vec2(ui.available_width(), 48.0), Layout::right_to_left(Align::Center), |ui| {
                    let open = self.show_add;
                    let turn = ui.ctx().animate_bool_with_time(egui::Id::unique("add-turn"), open, 0.2) * std::f32::consts::FRAC_PI_4;
                    let b = th::button(ui, th::Btn::Outline, Some(th::Icon::Plus(turn)), "Ajouter un hôte", None, true);
                    if open {
                        ui.painter().rect_stroke(b.rect, 12.0, Stroke::new(1.0, th::ACCENT), egui::StrokeKind::Inside);
                    }
                    if b.clicked() {
                        self.show_add = !open;
                    }
                });
                ui.add_space(32.0);

                if self.show_add {
                    let fr = egui::Frame::new().fill(Color32::from_rgba_unmultiplied(38, 38, 44, 190)).corner_radius(18).inner_margin(28).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let iw = ui.available_width();
                        let intro = |ui: &mut egui::Ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            th::text(ui, "Ajouter un hôte manuellement", th::display(22.0), th::TEXT);
                            th::text(ui, "Adresse IP ou nom, port optionnel.", th::body(14.0), th::MUTED);
                        };
                        let mut form = |ui: &mut egui::Ui| {
                            ui.horizontal(|ui| {
                                let bw = 196.0;
                                let r = th::text_input(ui, &mut self.manual, "192.168.1.85:47989", (ui.available_width() - bw - 10.0).max(160.0));
                                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                                let add = th::button(ui, th::Btn::OutlineStrong, None, "Ajouter et appairer", Some(bw), true).clicked();
                                if (add || enter) && !self.manual.trim().is_empty() {
                                    let (addr, port) = crate::split_host_port(self.manual.trim());
                                    action = Some((1, HostRef { name: addr.clone(), address: addr, port }));
                                    self.manual.clear();
                                }
                            });
                        };
                        if iw < 760.0 {
                            intro(ui);
                            ui.add_space(12.0);
                            form(ui);
                        } else {
                            ui.horizontal(|ui| {
                                ui.allocate_ui_with_layout(vec2(iw * 0.36, 0.0), Layout::top_down(Align::Min), intro);
                                ui.add_space(14.0);
                                form(ui);
                            });
                        }
                    });
                    let path = th::rounded_path(fr.response.rect.shrink(0.5), 18.0);
                    ui.painter().extend(egui::Shape::dashed_line(&path, Stroke::new(1.0, th::BORDER_STRONG), 6.0, 4.0));
                    ui.add_space(32.0);
                }

                if hosts.is_empty() {
                    th::card(th::white(0.12)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            th::phases_spinner(ui, t);
                            ui.add_space(6.0);
                            th::text(ui, "Recherche d'hôtes 2moon, Sunshine ou GeForce Experience sur le réseau local…", th::body(15.0), th::TEXT2);
                        });
                    });
                    ui.add_space(20.0);
                }
                grid(ui, &hosts, 340.0, 20.0, |ui, h| {
                    let st = status.get(&h.key()).cloned().unwrap_or_default();
                    if let Some(a) = self.host_card(ui, h, &st) {
                        action = Some((a, h.clone()));
                    }
                });

                // Shortcuts reminder.
                ui.add_space(28.0);
                th::rule(ui, th::DIVIDER);
                ui.add_space(16.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(18.0, 10.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        th::eyebrow(ui, "Pendant le stream", th::TEXT2);
                        ui.add_space(12.0);
                        for k in ["Ctrl", "Alt", "Maj"] {
                            th::kbd(ui, k, false);
                        }
                        ui.label(RichText::new("+").font(th::body(13.0)).color(th::MUTED));
                    });
                    for (k, what) in [("Q", "accueil"), ("Z", "capturer la souris"), ("F", "plein écran"), ("S", "statistiques"), ("M", "couper le micro")] {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            th::kbd(ui, k, false);
                            ui.label(RichText::new(what).font(th::body(13.0)).color(th::MUTED));
                        });
                    }
                });
                ui.add_space(40.0);
                let _ = w;
            });
        });
        match action {
            Some((0, h)) => self.open_apps(h),
            Some((1, h)) => {
                self.show_add = false;
                self.start_pairing(h)
            }
            Some((2, h)) => {
                self.store.hosts.retain(|k| !(k.address == h.address && k.http_port == h.port));
                let _ = self.store.save();
                self.toast(format!("{} oublié", h.name));
            }
            _ => {}
        }
    }

    /// One host card; returns 0 = open, 1 = pair, 2 = forget.
    fn host_card(&self, ui: &mut egui::Ui, h: &HostRef, st: &HostStatus) -> Option<u8> {
        let paired = self.is_paired(h);
        let in_game = st.online && st.info.as_ref().is_some_and(|i| i.busy && i.current_game != 0);
        let mut action = None;
        th::card(if in_game { th::ACCENT } else { th::white(0.12) }).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                th::icon_tile(ui, |p, r| th::paint_monitor(p, r, th::TEXT));
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| match (st.checked, st.online) {
                    (None, _) => th::pill(ui, th::PillKind::Outline(th::FAINT), 0, "…"),
                    (_, false) => th::pill(ui, th::PillKind::Outline(th::MUTED), 0, "HORS LIGNE"),
                    _ if in_game => th::pill(ui, th::PillKind::Filled(th::ACCENT), 3, "EN JEU"),
                    _ => th::pill(ui, th::PillKind::Outline(th::TEXT), 2, "EN LIGNE"),
                });
            });
            ui.add_space(14.0);
            ui.add(egui::Label::new(RichText::new(&h.name).font(th::display(36.0)).color(th::TEXT)).truncate());
            ui.label(RichText::new(format!("{}:{}", h.address, h.port)).font(th::mono(14.0)).color(th::MUTED));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (icon, txt, col) = match (paired, in_game) {
                    (true, true) => (th::Icon::Check, "Appairé · session en cours", th::TEXT2),
                    (true, false) => (th::Icon::Check, "Appairé", th::TEXT2),
                    (false, _) => (th::Icon::Lock, "Pas encore appairé", th::MUTED),
                };
                let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                th::paint_icon(ui.painter(), icon, r, col);
                ui.label(RichText::new(txt).font(th::body(14.0)).color(col));
            });
            ui.add_space(14.0);
            th::rule(ui, th::white(0.08));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let full = ui.available_width();
                if paired {
                    let forget_w = 108.0;
                    if th::button(ui, th::Btn::Accent, Some(th::Icon::Play), "Ouvrir", Some(full - forget_w - 10.0), st.online).clicked() {
                        action = Some(0);
                    }
                    if th::button(ui, th::Btn::Outline, None, "Oublier", Some(forget_w), true).on_hover_text("Supprimer cet hôte de la liste").clicked() {
                        action = Some(2);
                    }
                } else if th::button(ui, th::Btn::Light, None, "Appairer", Some(full), st.online).clicked() {
                    action = Some(1);
                }
            });
        });
        action
    }

    fn apps_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, host: HostRef, apps: Option<Vec<HostApp>>, error: Option<String>) {
        let current = self.status.lock().unwrap().get(&host.key()).and_then(|s| s.info.clone()).map(|i| i.current_game).unwrap_or(0);
        let s = &self.settings;
        let audio = match s.audio_channels {
            6 => "5.1",
            8 => "7.1",
            _ => "stéréo",
        };
        let summary = format!("{}×{} · {} ips · {} Mbit/s · codec {} · audio {audio}", s.width, s.height, s.fps, s.bitrate_kbps / 1000, s.codec);
        let t = self.look.t();
        let mut launch = None;
        let mut quit = false;
        let mut back = false;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            column(ui, 1120.0, |ui, w| {
                ui.add_space(28.0);
                back = th::button(ui, th::Btn::Ghost, Some(th::Icon::Back), "Ordinateurs", None, true).clicked();
                ui.add_space(28.0);
                th::eyebrow(ui, "Applications", th::ACCENT);
                th::text(ui, &host.name, th::display((w / 12.0).clamp(36.0, 64.0)), th::TEXT);
                th::text(ui, summary, th::mono(13.0), th::MUTED);
                ui.add_space(32.0);
                if let Some(e) = &error {
                    th::card(th::WARN).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        th::text(ui, "Impossible de lire la liste des applications", th::semibold(15.0), th::WARN);
                        th::text(ui, e, th::mono(13.0), th::TEXT2);
                    });
                    return;
                }
                let Some(apps) = &apps else {
                    ui.horizontal(|ui| {
                        th::phases_spinner(ui, t);
                        ui.label(RichText::new("Chargement des applications…").font(th::body(15.0)).color(th::TEXT2));
                    });
                    return;
                };
                grid(ui, apps, 300.0, 20.0, |ui, a| {
                    let running = current == a.id;
                    th::card(if running { th::ACCENT } else { th::white(0.12) }).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            th::icon_tile(ui, |p, r| th::paint_gamepad(p, r, th::TEXT));
                            if running {
                                ui.with_layout(Layout::right_to_left(Align::Min), |ui| th::pill(ui, th::PillKind::Filled(th::ACCENT), 3, "EN COURS"));
                            }
                        });
                        ui.add_space(14.0);
                        ui.add(egui::Label::new(RichText::new(&a.title).font(th::display(24.0)).color(th::TEXT)).truncate());
                        ui.add_space(10.0);
                        th::rule(ui, th::white(0.08));
                        ui.add_space(12.0);
                        // Full-width buttons, one under the other.
                        let full = ui.available_width();
                        ui.spacing_mut().item_spacing.y = 10.0;
                        if th::button(ui, th::Btn::Accent, Some(th::Icon::Play), if running { "Reprendre" } else { "Lancer" }, Some(full), true).clicked() {
                            launch = Some(a.id);
                        }
                        if running && th::button(ui, th::Btn::Outline, None, "Quitter l'application", Some(full), true).on_hover_text("Fermer l'application sur l'hôte").clicked() {
                            quit = true;
                        }
                    });
                });
                ui.add_space(40.0);
            });
        });
        if back {
            self.screen = Screen::Home;
        }
        if let Some(id) = launch {
            self.launch(ctx, host.clone(), id);
        }
        if quit {
            self.quit_app(&host);
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, body: Rect) {
        let before = self.settings.clone();
        let avail = body.width();
        let pad = if avail < 800.0 { 20.0 } else { 40.0 };
        let w = (avail - 2.0 * pad).min(1120.0);
        let x0 = body.center().x - w / 2.0;
        let side = avail >= 900.0;
        let main_left = if side { x0 + 220.0 + 48.0 } else { x0 };
        let main_w = x0 + w - main_left;

        // Side navigation: one moon phase per section, new moon to full moon.
        if side {
            let mut active = 0;
            for (i, top) in self.section_tops.iter().enumerate() {
                if *top <= body.top() + 160.0 {
                    active = i;
                }
            }
            let nav = Rect::from_min_max(pos2(x0, body.top() + 64.0 + 150.0), pos2(x0 + 220.0, body.bottom()));
            let mut nui = ui.new_child(egui::UiBuilder::new().max_rect(nav).layout(Layout::top_down(Align::Min)));
            nui.spacing_mut().item_spacing.y = 2.0;
            for (i, label) in SECTIONS.iter().enumerate() {
                let on = i == active;
                let (r, resp) = nui.allocate_exact_size(vec2(220.0, 44.0), Sense::click());
                let p = nui.painter();
                if on {
                    p.rect_filled(r, 10.0, th::SURFACE);
                } else if resp.hovered() {
                    p.rect_filled(r, 10.0, th::white(0.04));
                }
                th::paint_phase(p, pos2(r.left() + 20.0, r.center().y), 6.5, i as u8, if on { th::ACCENT } else { th::FAINT });
                let col = if on || resp.hovered() { th::TEXT } else { th::MUTED };
                p.text(pos2(r.left() + 40.0, r.center().y), egui::Align2::LEFT_CENTER, *label, th::body(15.0), col);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.settings_jump = Some(i);
                }
            }
            nui.add_space(16.0);
            nui.allocate_ui_with_layout(vec2(200.0, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(200.0);
                th::text(ui, "Une phase de lune par section, de la nouvelle lune à la pleine lune.", th::body(13.0), th::FAINT);
            });
        }

        let scroll = Rect::from_min_max(pos2(main_left, body.top()), body.max);
        let mut sui = ui.new_child(egui::UiBuilder::new().max_rect(scroll).layout(Layout::top_down(Align::Min)));
        let mics = self.mics.get_or_insert_with(moon_media::audio_io::input_devices).clone();
        let (s, tops, jump) = (&mut self.settings, &mut self.section_tops, &mut self.settings_jump);
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(&mut sui, |ui| {
            ui.set_width(main_w);
            ui.add_space(110.0);
            th::eyebrow(ui, "Configuration", th::ACCENT);
            ui.add_space(2.0);
            th::text(ui, "Réglages du stream", th::display((main_w / 10.5).clamp(40.0, 72.0)), th::TEXT);
            ui.add_space(48.0);

            section(ui, 0, SECTIONS[0], tops, jump, |ui| {
                let mut res = (s.width, s.height);
                let opts = [((1280, 720), "1280×720"), ((1600, 900), "1600×900"), ((1920, 1080), "1920×1080"), ((2560, 1440), "2560×1440"), ((3840, 2160), "3840×2160")];
                seg_row(ui, false, "Résolution", &mut res, &opts, true, 0.0);
                (s.width, s.height) = res;
                seg_row(ui, false, "Images par seconde", &mut s.fps, &[(30, "30"), (60, "60"), (90, "90"), (120, "120"), (144, "144")], true, 56.0);
                row(ui, false, |ui| bitrate_row(ui, s));
                let codecs = [("auto".to_string(), "Automatique"), ("h264".to_string(), "H.264"), ("hevc".to_string(), "HEVC"), ("av1".to_string(), "AV1")];
                seg_row(ui, false, "Codec", &mut s.codec, &codecs, false, 0.0);
                toggle_row(ui, false, "Couleurs 4:4:4", "Texte plus net, plus de débit. H.264 / HEVC si l'hôte le propose.", &mut s.yuv444);
                toggle_row(ui, false, "HDR / 10 bits", "HEVC ou AV1 · moins d'effets d'escalier · affichée en SDR.", &mut s.hdr);
                toggle_row(ui, true, "Décodage GPU", "Si possible. NVIDIA : NVDEC · Intel / AMD : VAAPI.", &mut s.hw_decode);
            });
            section(ui, 1, SECTIONS[1], tops, jump, |ui| {
                seg_row(ui, false, "Canaux audio", &mut s.audio_channels, &[(2, "Stéréo"), (6, "5.1"), (8, "7.1")], false, 64.0);
                row(ui, true, |ui| mic_choice(ui, s, &mics));
            });
            section(ui, 2, SECTIONS[2], tops, jump, |ui| {
                toggle_row(ui, false, "Plein écran au démarrage", "Le stream s'ouvre directement en plein écran.", &mut s.fullscreen);
                toggle_row(ui, false, "Statistiques pendant le stream", "Affichées en surimpression · Ctrl+Alt+Maj+S pour basculer.", &mut s.show_stats);
                toggle_row(ui, true, "Synchro verticale (V-Sync)", "Pas de déchirement, mais plus de latence · appliqué au prochain démarrage.", &mut s.vsync);
            });
            section(ui, 3, SECTIONS[3], tops, jump, |ui| {
                toggle_row(ui, true, "Quitter l'application sur l'hôte", "En fermant le stream, l'application lancée sur l'hôte est fermée aussi.", &mut s.quit_app_on_exit);
            });

            // Keyboard shortcuts.
            let keys: [(&[&str], &str); 5] = [
                (&["Ctrl", "Alt", "Maj", "Q"], "Revenir à l'accueil"),
                (&["…", "Z"], "Capturer la souris"),
                (&["…", "F"], "Plein écran"),
                (&["…", "S"], "Statistiques"),
                (&["…", "M"], "Couper le micro"),
            ];
            grid(ui, &keys, 200.0, 12.0, |ui, (ks, what)| {
                egui::Frame::new().stroke(Stroke::new(1.0, th::BORDER)).corner_radius(14).inner_margin(18).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for k in ks.iter() {
                            th::kbd(ui, k, ks.len() == 2 && *k != "…");
                        }
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new(*what).font(th::body(14.0)).color(th::TEXT2));
                });
            });
            ui.add_space(48.0);
        });
        if format!("{:?}", before) != format!("{:?}", self.settings) {
            self.settings.save(&self.store);
        }
    }

    /// Pairing and connecting: a dialog under the moon, no header.
    fn dialog_ui(&mut self, ui: &mut egui::Ui, full: Rect) {
        let w = (full.width() - 32.0).min(560.0);
        let top = (full.height() - self.dialog_h - 32.0).min(full.height() * 0.3).max(24.0);
        let r = Rect::from_min_max(pos2(full.center().x - w / 2.0, full.top() + top), pos2(full.center().x + w / 2.0, full.bottom()));
        let mut dui = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
        let t = self.look.t();
        let title_size = if w < 480.0 { 32.0 } else { 40.0 };
        let mut cancel = false;
        let mut dialog_h = self.dialog_h;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(&mut dui, |ui| {
            let frame = egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(18, 18, 21, 199))
                .stroke(Stroke::new(1.0, th::white(0.12)))
                .corner_radius(22)
                .inner_margin(36)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 10.0;
                    let waiting = |ui: &mut egui::Ui, msg: &str| {
                        egui::Frame::new().fill(th::SURFACE2).corner_radius(12).inner_margin(egui::Margin::symmetric(16, 14)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                th::phases_spinner(ui, t);
                                ui.add_space(6.0);
                                ui.label(RichText::new(msg).font(th::body(14.0)).color(th::TEXT2));
                            });
                        });
                    };
                    match &self.screen {
                        Screen::Pairing { host, pin } => {
                            th::eyebrow(ui, &format!("Appairage · {}", host.name), th::ACCENT);
                            th::text(ui, "Saisissez ce code sur l'hôte", th::display(title_size), th::TEXT);
                            th::text(
                                ui,
                                format!("Ouvrez l'interface de l'hôte ({}) — panneau d'administration 2moon ou interface Sunshine — et entrez le code PIN ci-dessous pour autoriser cet appareil.", host.address),
                                th::body(15.0),
                                th::MUTED,
                            );
                            ui.add_space(18.0);
                            let (pr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 108.0), Sense::hover());
                            let bw = (pr.width() - 3.0 * 12.0) / 4.0;
                            for (i, d) in pin.chars().take(4).enumerate() {
                                let b = Rect::from_min_size(pr.min + vec2(i as f32 * (bw + 12.0), 0.0), vec2(bw, 108.0));
                                ui.painter().rect(b, 16.0, th::BG, Stroke::new(1.0, th::BORDER_STRONG), egui::StrokeKind::Inside);
                                th::centered_text(ui.painter(), b, &d.to_string(), th::display(64.0), th::TEXT);
                            }
                            ui.add_space(18.0);
                            waiting(ui, "En attente de la confirmation de l'hôte…");
                            ui.add_space(18.0);
                            ui.allocate_ui_with_layout(vec2(ui.available_width(), 48.0), Layout::right_to_left(Align::Center), |ui| {
                                cancel = th::button(ui, th::Btn::Outline, None, "Annuler", None, true).clicked();
                            });
                        }
                        Screen::Connecting { host, since } => {
                            let s = &self.settings;
                            th::eyebrow(ui, &format!("Connexion · {} s", since.elapsed().as_secs()), th::ACCENT);
                            th::text(ui, &host.name, th::display(title_size), th::TEXT);
                            th::text(ui, format!("{}×{} · {} ips · {} Mbit/s", s.width, s.height, s.fps, s.bitrate_kbps / 1000), th::mono(14.0), th::MUTED);
                            ui.add_space(18.0);
                            waiting(ui, "Démarrage de la session sur l'hôte…");
                        }
                        _ => {}
                    }
                });
            dialog_h = frame.response.rect.height();
        });
        self.dialog_h = dialog_h;
        if cancel {
            self.screen = Screen::Home;
        }
    }
}

/// Microphone: off, the system default, or one of this machine's microphones.
fn mic_choice(ui: &mut egui::Ui, s: &mut Settings, mics: &[moon_media::audio_io::InputDevice]) {
    label_desc(ui, "Micro envoyé à l'hôte", Some("Hôtes 2moon · devient le micro par défaut de l'hôte pendant le stream · Ctrl+Alt+Maj+M pour le couper."));
    ui.add_space(6.0);
    ui.spacing_mut().item_spacing.y = 6.0;
    let w = ui.available_width();
    let mut pick: Option<(bool, Option<String>)> = None;
    if th::choice(ui, w, !s.mic, "Désactivé", None).clicked() {
        pick = Some((false, None));
    }
    if th::choice(ui, w, s.mic && s.mic_device.is_none(), "Micro par défaut du système", None).clicked() {
        pick = Some((true, None));
    }
    for m in mics {
        let on = s.mic && s.mic_device.as_deref() == Some(m.id.as_str());
        if th::choice(ui, w, on, &m.label, None).on_hover_text(&m.id).clicked() {
            pick = Some((true, Some(m.id.clone())));
        }
    }
    // A chosen microphone that is not plugged in right now stays visible.
    if let Some(id) = s.mic_device.clone().filter(|id| !mics.iter().any(|m| &m.id == id)) {
        th::choice(ui, w, s.mic, &id, Some("non branché · le micro par défaut sera utilisé"));
    }
    if let Some((on, dev)) = pick {
        s.mic = on;
        if on {
            s.mic_device = dev;
        }
    }
}

/// Bitrate: big value, "advised" chip, log slider.
fn bitrate_row(ui: &mut egui::Ui, s: &mut Settings) {
    let w = ui.available_width();
    let left = |ui: &mut egui::Ui| label_desc(ui, "Débit vidéo", Some("Plus haut = plus net, plus de bande passante."));
    let mut right = |ui: &mut egui::Ui| {
        ui.spacing_mut().item_spacing.y = 10.0;
        let mbps = s.bitrate_kbps / 1000;
        let sugg = s.suggested_bitrate();
        ui.horizontal(|ui| {
            let g1 = ui.painter().layout_no_wrap(mbps.to_string(), th::display(40.0), th::TEXT);
            let g2 = ui.painter().layout_no_wrap("Mbit/s".into(), th::mono(14.0), th::MUTED);
            let (r, _) = ui.allocate_exact_size(vec2(g1.size().x + 8.0 + g2.size().x, g1.size().y), Sense::hover());
            let base = r.top() + g1.size().y;
            ui.painter().galley(r.min, g1.clone(), th::TEXT);
            ui.painter().galley(pos2(r.left() + g1.size().x + 8.0, base - g2.size().y - 6.0), g2, th::MUTED);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if sugg == s.bitrate_kbps { format!("conseillé : {}", sugg / 1000) } else { format!("conseillé : {} · appliquer", sugg / 1000) };
                let g = ui.painter().layout_no_wrap(label, th::mono(13.0), th::TEXT2);
                let (cr, resp) = ui.allocate_exact_size(g.size() + vec2(24.0, 18.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(cr, 18.0, th::white(0.05));
                }
                ui.painter().extend(egui::Shape::dashed_line(&th::rounded_path(cr.shrink(0.5), cr.height() / 2.0), Stroke::new(1.0, Color32::from_rgb(0x52, 0x52, 0x5B)), 4.0, 3.0));
                ui.painter().galley(cr.center() - g.size() / 2.0, g, th::TEXT2);
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    s.bitrate_kbps = sugg;
                }
            });
        });
        let mut v = s.bitrate_kbps as f32 / 1000.0;
        if th::slider_log(ui, &mut v, 2.0, 150.0).changed() {
            s.bitrate_kbps = (v * 1000.0) as u32;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("2").font(th::mono(12.0)).color(th::MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new("150 Mbit/s").font(th::mono(12.0)).color(th::MUTED)));
        });
    };
    if w < 620.0 {
        left(ui);
        ui.add_space(8.0);
        right(ui);
    } else {
        ui.horizontal_top(|ui| {
            let lw = (w * 0.33).max(180.0);
            ui.allocate_ui_with_layout(vec2(lw, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(lw);
                left(ui);
            });
            ui.add_space(14.0);
            ui.vertical(|ui| {
                ui.set_width(w - lw - 24.0);
                right(ui);
            });
        });
    }
}

impl GuiApp {

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
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "En attente de la première image…", th::body(18.0), th::MUTED);
        }
        if let Some(t) = st.mic_toast.filter(|t| t.elapsed() < Duration::from_secs(2)) {
            let muted = st.session.mic_muted.load(Ordering::Relaxed);
            let (txt, col, ph) = if muted { ("Micro coupé", th::WARN, 0) } else { ("Micro actif", th::ACCENT, 3) };
            let p = ui.painter();
            let g = p.layout_no_wrap(txt.into(), th::semibold(15.0), th::TEXT);
            let pill = Rect::from_center_size(rect.center_top() + vec2(0.0, 48.0), vec2(g.size().x + 64.0, 44.0));
            p.rect(pill, 255.0, th::alpha(th::BG, 0.85), Stroke::new(1.0, col), egui::StrokeKind::Inside);
            th::paint_phase(p, pos2(pill.left() + 26.0, pill.center().y), 6.5, ph, col);
            p.galley(pos2(pill.left() + 44.0, pill.center().y - g.size().y / 2.0), g, th::TEXT);
            ctx.request_repaint_after(Duration::from_secs(2).saturating_sub(t.elapsed()));
        }
        if self.settings.show_stats {
            let mic = match (st.session.mic_active, st.session.mic_muted.load(Ordering::Relaxed)) {
                (false, _) => String::new(),
                (true, true) => " · micro coupé".into(),
                (true, false) => format!(" · micro {} paquets", st.session.stats.mic_packets.load(Ordering::Relaxed)),
            };
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = (rect.width() - 56.0).max(200.0);
            let fmt = |font, color| egui::TextFormat { font_id: font, color, ..Default::default() };
            job.append(&format!("{:.0} fps", st.fps_window.2), 0.0, fmt(th::mono_bold(13.0), th::ACCENT));
            job.append(&format!(" affichées · {}{mic}", short_stats(&st.session.stats)), 0.0, fmt(th::mono(13.0), th::TEXT));
            let p = ui.painter();
            let g = p.layout_job(job);
            let bg = Rect::from_min_size(rect.left_top() + vec2(12.0, 12.0), g.size() + vec2(28.0, 20.0));
            p.rect(bg, 12.0, th::alpha(th::BG, 0.78), Stroke::new(1.0, th::white(0.12)), egui::StrokeKind::Inside);
            p.galley(bg.min + vec2(14.0, 10.0), g, th::TEXT);
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
                    // Forwarded to the host: the physical key (the host applies its own layout).
                    let k = physical_key.unwrap_or(key);
                    let combo = modifiers.ctrl && modifiers.alt && modifiers.shift;
                    if combo && pressed && !repeat {
                        // Our shortcuts: the letter printed on the key (logical key), so that
                        // Ctrl+Alt+Maj+Q is the Q key on AZERTY too (physically QWERTY's A).
                        match key {
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
