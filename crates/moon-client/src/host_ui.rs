//! Host mode of the app: the same pages and settings as the web panel,
//! through the admin API (see `host_api`).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use super::*;
use crate::host_api::{self, HostApi, ServiceState};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum HostTab {
    Session,
    Appareils,
    Applications,
    Hote,
    Journal,
    Reglages,
}

const TABS: [(HostTab, &str); 6] = [
    (HostTab::Session, "Session"),
    (HostTab::Appareils, "Appareils"),
    (HostTab::Applications, "Applications"),
    (HostTab::Hote, "Hôte"),
    (HostTab::Journal, "Journal"),
    (HostTab::Reglages, "Réglages"),
];

/// What the host last told us (written by background requests).
#[derive(Default)]
struct HostData {
    status: Option<Value>,
    clients: Option<Vec<Value>>,
    apps: Option<Vec<Value>>,
    settings: Option<Value>,
    /// `/api/state`: whether the web panel has an account yet.
    state: Option<Value>,
    logs: Vec<String>,
    /// Box art fetched, by app id (None: the app has none).
    covers: HashMap<u64, Option<egui::ColorImage>>,
    /// The host could not be reached (or refused us): why.
    error: Option<String>,
    /// The token was refused: read `local-admin.json` again.
    stale: bool,
    /// How the service runs (checked while the host does not answer).
    service: Option<ServiceState>,
    /// Messages for the toast.
    notices: Vec<String>,
}

/// An application being edited (id 0: a new one).
#[derive(Clone, Default)]
struct AppEdit {
    id: u64,
    title: String,
    cmd: String,
    hdr: bool,
}

pub(super) struct HostPanel {
    pub(super) tab: HostTab,
    api: Option<HostApi>,
    data: Arc<Mutex<HostData>>,
    busy: Arc<AtomicBool>,
    service_busy: Arc<AtomicBool>,
    last_poll: Option<Instant>,
    last_service_check: Option<Instant>,
    last_discover: Option<Instant>,
    /// Data to fetch again at the next poll (after an action).
    dirty: bool,
    pin: String,
    follow: bool,
    name_edit: Option<String>,
    pw_current: String,
    pw_new: String,
    setup_user: String,
    setup_pass: String,
    login_user: String,
    login_pass: String,
    login_port: String,
    app_edit: Option<AppEdit>,
    confirm_unpair: Option<(String, String)>,
    /// "Change the remote access code?" confirmation open.
    confirm_regen: bool,
    covers: HashMap<u64, Option<egui::TextureHandle>>,
    /// Mbit/s sent over the last two minutes (session chart).
    history: VecDeque<f32>,
}

impl Default for HostPanel {
    fn default() -> Self {
        Self {
            tab: HostTab::Session,
            api: None,
            data: Default::default(),
            busy: Default::default(),
            service_busy: Default::default(),
            last_poll: None,
            last_service_check: None,
            last_discover: None,
            dirty: true,
            pin: String::new(),
            follow: true,
            name_edit: None,
            pw_current: String::new(),
            pw_new: String::new(),
            setup_user: String::new(),
            setup_pass: String::new(),
            login_user: String::new(),
            login_pass: String::new(),
            login_port: host_api::DEFAULT_ADMIN_PORT.to_string(),
            app_edit: None,
            confirm_unpair: None,
            confirm_regen: false,
            covers: HashMap::new(),
            history: VecDeque::new(),
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}
fn n(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
/// Remote access is starting: keep reloading the settings.
fn settings_pending(set: &Value) -> bool {
    let waiting = |k: &str, busy: &[&str]| b(set, k) && b(set, &format!("{k}_available")) && set.pointer(&format!("/{k}_status/state")).and_then(|s| s.as_str()).is_none_or(|s| busy.contains(&s));
    waiting("remote", &["starting", "off"])
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// "6 h 56 min", "2 j 3 h 0 min" (as the web panel).
fn fmt_uptime(secs: f64) -> String {
    let s = secs as u64;
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("{d} j {h} h {m} min")
    } else {
        format!("{h} h {m} min")
    }
}

impl HostPanel {
    /// Fetch everything again (mode just switched to Host).
    pub(super) fn dirty_all(&mut self) {
        self.dirty = true;
        self.last_discover = None;
        self.last_service_check = None;
    }
}

impl GuiApp {
    // ------------------------------------------------------------ data

    /// Keep the host's data fresh while Host mode is shown.
    fn host_poll(&mut self, ctx: &egui::Context) {
        let hp = &mut self.host;
        // Notices from background requests.
        let notices = std::mem::take(&mut hp.data.lock().unwrap().notices);
        if hp.data.lock().unwrap().stale {
            hp.data.lock().unwrap().stale = false;
            hp.api = None;
        }
        // Find the local host (its token file appears when it starts).
        if hp.api.is_none() && hp.last_discover.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            hp.last_discover = Some(Instant::now());
            if let Some(api) = HostApi::local() {
                hp.api = Some(api);
                hp.dirty = true;
            }
        }
        // While the host does not answer, see how its service is doing.
        let unreachable = hp.api.is_none() || hp.data.lock().unwrap().error.is_some();
        if unreachable && !hp.service_busy.load(Ordering::Relaxed) && hp.last_service_check.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
            hp.last_service_check = Some(Instant::now());
            hp.service_busy.store(true, Ordering::Relaxed);
            let (data, busy, ctx2) = (hp.data.clone(), hp.service_busy.clone(), ctx.clone());
            self.rt.as_ref().expect("tokio runtime").spawn_blocking(move || {
                data.lock().unwrap().service = Some(host_api::service_state());
                busy.store(false, Ordering::Relaxed);
                ctx2.request_repaint();
            });
        }
        let hp = &mut self.host;
        if let Some(api) = hp.api.clone() {
            let due = hp.dirty || hp.last_poll.is_none_or(|t| t.elapsed() > Duration::from_millis(1500));
            if due && !hp.busy.load(Ordering::Relaxed) && !api.needs_login() {
                let full = hp.dirty;
                hp.dirty = false;
                hp.last_poll = Some(Instant::now());
                hp.busy.store(true, Ordering::Relaxed);
                let tab = hp.tab;
                let (data, busy, ctx2) = (hp.data.clone(), hp.busy.clone(), ctx.clone());
                let need_apps = tab == HostTab::Applications && (full || data.lock().unwrap().apps.is_none());
                let need_settings = tab == HostTab::Reglages && (full || data.lock().unwrap().settings.as_ref().is_none_or(settings_pending));
                self.rt().spawn(async move {
                    let r: anyhow::Result<()> = async {
                        let st = api.get("/api/status").await?;
                        let clients = api.get("/api/clients").await?;
                        {
                            let mut d = data.lock().unwrap();
                            d.status = Some(st);
                            d.clients = clients.as_array().cloned();
                            d.error = None;
                        }
                        if need_apps {
                            let apps = api.get("/api/apps").await?;
                            let list = apps.as_array().cloned().unwrap_or_default();
                            // Box art of new apps.
                            for a in &list {
                                let id = a.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                                let known = data.lock().unwrap().covers.contains_key(&id);
                                if !known || full {
                                    let img = if b(a, "cover") {
                                        api.get_bytes(&format!("/api/apps/cover?id={id}")).await.ok().flatten().and_then(|png| image::load_from_memory(&png).ok()).map(|i| {
                                            let i = i.to_rgba8();
                                            egui::ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                                        })
                                    } else {
                                        None
                                    };
                                    data.lock().unwrap().covers.insert(id, img);
                                }
                            }
                            data.lock().unwrap().apps = Some(list);
                        }
                        if need_settings {
                            let set = api.get("/api/settings").await?;
                            let state = api.get("/api/state").await?;
                            let mut d = data.lock().unwrap();
                            d.settings = Some(set);
                            d.state = Some(state);
                        }
                        if tab == HostTab::Journal {
                            let logs = api.get("/api/logs?n=300").await?;
                            data.lock().unwrap().logs = logs.as_array().map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect()).unwrap_or_default();
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(e) = r {
                        let mut d = data.lock().unwrap();
                        if e.downcast_ref::<host_api::ApiError>().is_some_and(|a| a.status == 401) {
                            d.stale = true;
                        }
                        d.error = Some(format!("{e:#}"));
                    }
                    busy.store(false, Ordering::Relaxed);
                    ctx2.request_repaint();
                });
            }
        }
        // Live bitrate history.
        let hp = &mut self.host;
        if let Some(live) = hp.data.lock().unwrap().status.as_ref().and_then(|s| s.get("live")).filter(|l| b(l, "active")) {
            if hp.last_poll.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
                hp.history.push_back(n(live, "mbps") as f32);
                while hp.history.len() > 120 {
                    hp.history.pop_front();
                }
            }
        } else {
            hp.history.clear();
        }
        for m in notices {
            self.toast(m);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// POST to the host in the background, then refresh; `ok` is shown on success.
    fn host_post(&mut self, path: &'static str, body: Value, ok: Option<&'static str>) {
        let Some(api) = self.host.api.clone() else { return };
        let data = self.host.data.clone();
        self.host.dirty = true;
        self.rt().spawn(async move {
            let msg = match api.post(path, body).await {
                Ok(_) => ok.map(String::from),
                Err(e) => Some(format!("{e:#}")),
            };
            if let Some(m) = msg {
                data.lock().unwrap().notices.push(m);
            }
        });
    }

    fn service_action(&mut self, what: &'static str) {
        let data = self.host.data.clone();
        self.host.last_service_check = None;
        self.host.api = None;
        self.rt().spawn_blocking(move || {
            let r = match what {
                "install" => host_api::install_service(),
                "start" => host_api::start_service(),
                _ => host_api::stop_service(),
            };
            let mut d = data.lock().unwrap();
            d.error = None;
            d.status = None;
            d.notices.push(match r {
                Ok(()) => match what {
                    "stop" => "Hôte arrêté".into(),
                    _ => "Hôte démarré".into(),
                },
                Err(e) => format!("{e:#}"),
            });
        });
    }

    // ------------------------------------------------------------ layout

    pub(super) fn host_ui(&mut self, ui: &mut egui::Ui, body: Rect, ctx: &egui::Context) {
        self.host_poll(ctx);
        let avail = body.width();
        let pad = if avail < 800.0 { 20.0 } else { 40.0 };
        let w = (avail - 2.0 * pad).min(1200.0);
        let x0 = body.center().x - w / 2.0;
        let side = avail >= 900.0;
        let main_left = if side { x0 + 220.0 + 48.0 } else { x0 };
        let main_w = x0 + w - main_left;
        let connected = self.host.api.as_ref().is_some_and(|a| !a.needs_login()) && self.host.data.lock().unwrap().status.is_some() && self.host.data.lock().unwrap().error.is_none();

        // Side navigation: one moon phase per tab, new moon to full moon.
        let device_count = self.host.data.lock().unwrap().clients.as_ref().map(|c| c.len());
        let tabs_rect = if side { Rect::from_min_max(pos2(x0, body.top() + 48.0), pos2(x0 + 220.0, body.bottom())) } else { Rect::from_min_size(pos2(x0, body.top() + 12.0), vec2(w, 44.0)) };
        let mut nui = ui.new_child(egui::UiBuilder::new().max_rect(tabs_rect).layout(if side { Layout::top_down(Align::Min) } else { Layout::left_to_right(Align::Center) }));
        nui.spacing_mut().item_spacing = vec2(2.0, 2.0);
        let phases = [0u8, 1, 2, 4, 3, 3];
        for (i, (tab, label)) in TABS.iter().enumerate() {
            let on = self.host.tab == *tab;
            let count = (*tab == HostTab::Appareils).then_some(device_count).flatten();
            let label_w = nui.painter().layout_no_wrap(label.to_string(), th::body(15.0), th::TEXT).size().x;
            let width = if side { 220.0 } else { label_w + 48.0 + if count.is_some() { 28.0 } else { 0.0 } };
            let (r, resp) = nui.allocate_exact_size(vec2(width, 44.0), Sense::click());
            let p = nui.painter();
            if on {
                p.rect_filled(r, 10.0, th::white(0.09));
            } else if resp.hovered() {
                p.rect_filled(r, 10.0, th::white(0.04));
            }
            th::paint_phase(p, pos2(r.left() + 20.0, r.center().y), 6.5, phases[i], if on { th::ACCENT } else { th::FAINT });
            let col = if on || resp.hovered() { th::TEXT } else { th::MUTED };
            let font = if on { th::semibold(15.0) } else { th::body(15.0) };
            let tr = p.text(pos2(r.left() + 40.0, r.center().y), egui::Align2::LEFT_CENTER, *label, font, col);
            if let Some(c) = count {
                let g = p.layout_no_wrap(c.to_string(), th::mono_bold(11.0), th::TEXT);
                let br = Rect::from_min_size(pos2(tr.right() + 8.0, r.center().y - 10.0), vec2(g.size().x + 14.0, 20.0));
                p.rect_filled(br, 10.0, th::white(0.1));
                p.galley(br.center() - g.size() / 2.0, g, th::TEXT);
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                self.host.tab = *tab;
                self.host.dirty = true;
                self.host.app_edit = None;
            }
        }
        if side {
            nui.add_space(16.0);
            nui.allocate_ui_with_layout(vec2(200.0, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(200.0);
                th::text(ui, "Une phase de lune par onglet, de la nouvelle lune à la pleine lune.", th::body(13.0), th::FAINT);
            });
        }

        let top = if side { body.top() } else { body.top() + 60.0 };
        let scroll = Rect::from_min_max(pos2(main_left, top), body.max);
        let mut sui = ui.new_child(egui::UiBuilder::new().max_rect(scroll).layout(Layout::top_down(Align::Min)));
        let hostname = self.host.data.lock().unwrap().status.as_ref().map(|v| s(v, "hostname").to_string()).unwrap_or_default();
        let tab = self.host.tab;
        let title = match tab {
            HostTab::Session => "Session",
            HostTab::Appareils => "Appareils appairés",
            HostTab::Applications => "Applications",
            HostTab::Hote => "Hôte",
            HostTab::Journal => "Journal",
            HostTab::Reglages => "Réglages de l'hôte",
        };
        egui::ScrollArea::vertical().id_salt(("host", tab)).auto_shrink([false; 2]).stick_to_bottom(tab == HostTab::Journal && self.host.follow).show(&mut sui, |ui| {
            ui.set_width(main_w);
            ui.add_space(if side { 48.0 } else { 16.0 });
            th::eyebrow(ui, &if hostname.is_empty() { "Hôte".to_string() } else { format!("Hôte · {hostname}") }, th::ACCENT);
            ui.add_space(2.0);
            th::text(ui, title, th::display((main_w / 12.0).clamp(36.0, 64.0)), th::TEXT);
            ui.add_space(36.0);
            if !connected {
                self.host_offline_ui(ui);
            } else {
                match tab {
                    HostTab::Session => self.host_session_ui(ui),
                    HostTab::Appareils => self.host_devices_ui(ui),
                    HostTab::Applications => self.host_apps_ui(ui, ctx),
                    HostTab::Hote => self.host_info_ui(ui),
                    HostTab::Journal => self.host_logs_ui(ui),
                    HostTab::Reglages => self.host_settings_ui(ui),
                }
            }
            ui.add_space(48.0);
        });
        self.unpair_dialog(ctx);
        self.regen_dialog(ctx);
    }

    // ------------------------------------------------------------ offline

    fn host_offline_ui(&mut self, ui: &mut egui::Ui) {
        let (service, error) = {
            let d = self.host.data.lock().unwrap();
            (d.service, d.error.clone())
        };
        let needs_login = self.host.api.as_ref().is_some_and(|a| a.needs_login());
        let mut action = None;
        th::card(th::white(0.12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 10.0;
            if needs_login || (service == Some(ServiceState::Running) && self.host.api.is_none() && cfg!(windows)) {
                // Windows: the service's folder is for administrators; log in instead.
                th::text(ui, "Connexion à l'hôte", th::display(26.0), th::TEXT);
                th::text(ui, "Identifiant et mot de passe de l'interface web d'administration de l'hôte.", th::body(14.0), th::MUTED);
                let w = ui.available_width().min(420.0);
                th::text_input(ui, &mut self.host.login_user, "Identifiant", w);
                password_input(ui, &mut self.host.login_pass, "Mot de passe", w);
                th::text_input(ui, &mut self.host.login_port, "Port", 160.0);
                if th::button(ui, th::Btn::Accent, None, "Se connecter", Some(w), true).clicked() {
                    action = Some("login");
                }
                return;
            }
            let (title, desc, button) = match service {
                Some(ServiceState::Missing) => (
                    "L'hôte n'est pas installé",
                    if cfg!(target_os = "macos") {
                        "Installez l'hôte 2moon pour partager cet écran. macOS demandera d'autoriser moon-server dans Réglages Système › Confidentialité et sécurité › Enregistrement de l'écran, puis Accessibilité pour le clavier et la souris."
                    } else if cfg!(windows) {
                        "Installez l'hôte avec l'installateur 2moon pour Windows."
                    } else {
                        "Installez l'hôte 2moon comme service de votre session (démarre avec le bureau)."
                    },
                    (!cfg!(windows)).then_some(("Installer et démarrer l'hôte", "install")),
                ),
                Some(ServiceState::Stopped) => ("L'hôte est arrêté", "Démarrez l'hôte pour que les clients puissent se connecter à cet ordinateur.", Some(("Démarrer l'hôte", "start"))),
                Some(ServiceState::Running) => ("L'hôte démarre…", "Il répondra dans quelques secondes.", None),
                Some(ServiceState::Unknown) | None => ("Recherche de l'hôte…", "Hôte 2moon de cet ordinateur.", None),
            };
            ui.horizontal(|ui| {
                th::phases_spinner(ui, self.look.t());
                ui.add_space(6.0);
                th::text(ui, title, th::display(26.0), th::TEXT);
            });
            th::text(ui, desc, th::body(14.0), th::MUTED);
            if let Some(e) = error.filter(|_| service == Some(ServiceState::Running)) {
                th::text(ui, e, th::mono(12.0), th::FAINT);
            }
            if let Some((label, what)) = button {
                ui.add_space(6.0);
                if th::button(ui, th::Btn::Accent, None, label, None, !self.host.service_busy.load(Ordering::Relaxed)).clicked() {
                    action = Some(what);
                }
            }
        });
        match action {
            Some("login") => {
                let port = self.host.login_port.trim().parse().unwrap_or(host_api::DEFAULT_ADMIN_PORT);
                let api = HostApi::with_login(port);
                let (user, pass) = (self.host.login_user.clone(), std::mem::take(&mut self.host.login_pass));
                let data = self.host.data.clone();
                self.host.api = Some(api.clone());
                self.host.dirty = true;
                self.rt().spawn(async move {
                    if let Err(e) = api.login(&user, &pass).await {
                        data.lock().unwrap().notices.push(format!("{e:#}"));
                    }
                });
            }
            Some(w @ ("install" | "start")) => self.service_action(w),
            _ => {}
        }
    }

    // ------------------------------------------------------------ session

    fn host_session_ui(&mut self, ui: &mut egui::Ui) {
        let st = self.host.data.lock().unwrap().status.clone().unwrap_or_default();
        let live = st.get("live").filter(|l| b(l, "active")).cloned();
        let session = st.get("session").filter(|s| !s.is_null()).cloned();
        let pending: Vec<Value> = st.get("pending").and_then(|p| p.as_array()).cloned().unwrap_or_default();
        let client = live.as_ref().map(|l| s(l, "peer").to_string()).filter(|c| !c.is_empty()).or_else(|| session.as_ref().map(|v| s(v, "client").to_string())).unwrap_or_default();
        let mut act: Option<(&'static str, &'static str)> = None;
        let wide = ui.available_width() >= 760.0;
        let half = if wide { (ui.available_width() - 20.0) / 2.0 } else { ui.available_width() };
        let session_panel = |ui: &mut egui::Ui, act: &mut Option<(&'static str, &'static str)>, history: &VecDeque<f32>| {
            th::card(if live.is_some() { th::ACCENT } else { th::white(0.12) }).show(ui, |ui| {
                ui.set_width(half - 56.0);
                ui.spacing_mut().item_spacing.y = 12.0;
                th::eyebrow(ui, "Stream", th::MUTED);
                ui.horizontal(|ui| {
                    let ph = if live.is_some() { 3 } else if session.is_some() { 2 } else { 0 };
                    th::phase(ui, 40.0, ph, if live.is_some() { th::ACCENT } else { th::FAINT });
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        let title = if live.is_some() { "Session en cours" } else if session.is_some() { "Session ouverte" } else { "Aucun stream" };
                        th::text(ui, title, th::display(22.0), th::TEXT);
                        let sub = match (&live, &session) {
                            (Some(l), _) => format!("{}×{} · {} ips · {} · client {}", n(l, "width"), n(l, "height"), n(l, "target_fps"), s(l, "codec"), client),
                            (None, Some(_)) => format!("client {client}"),
                            _ => "Aucun client ne streame en ce moment.".into(),
                        };
                        th::text(ui, sub, th::body(13.0), th::MUTED);
                    });
                });
                if let Some(l) = &live {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 24.0;
                        for (v, label) in [(format!("{:.1}", n(l, "fps")), "images/s"), (format!("{:.1}", n(l, "mbps")), "Mbit/s envoyés"), (format!("{:.2}", n(l, "encode_ms")), "ms d'encodage"), (format!("{}", n(l, "idr_frames")), "images clés")] {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                th::text(ui, v, th::display(26.0), th::TEXT);
                                th::text(ui, label, th::mono(11.0), th::MUTED);
                            });
                        }
                    });
                    th::text(ui, format!("{}×{} @ {} ips · {} via {} · débit cible {:.0} Mbit/s · audio {} canaux · client {}", n(l, "width"), n(l, "height"), n(l, "target_fps"), s(l, "codec"), s(l, "encoder"), n(l, "bitrate_kbps") / 1000.0, n(l, "audio_channels"), if client.is_empty() { "?" } else { &client }), th::body(13.0), th::TEXT2);
                    bitrate_chart(ui, history, (n(l, "bitrate_kbps") / 1000.0) as f32);
                }
                ui.add_space(4.0);
                th::text(ui, "Arrête le stream du client, ou demande une image clé si l'image est abîmée.", th::body(13.0), th::MUTED);
                ui.horizontal_wrapped(|ui| {
                    if th::button(ui, th::Btn::Outline, None, "Forcer une image clé", None, live.is_some()).clicked() {
                        *act = Some(("/api/session/idr", "Image clé demandée"));
                    }
                    if danger_button(ui, "Arrêter la session", session.is_some()).clicked() {
                        *act = Some(("/api/session/stop", "Session arrêtée"));
                    }
                });
            });
        };
        let mut pin_submit = false;
        let pin = &mut self.host.pin;
        let mut pairing_panel = |ui: &mut egui::Ui, pin_submit: &mut bool| {
            th::card(if pending.is_empty() { th::white(0.12) } else { th::ACCENT }).show(ui, |ui| {
                ui.set_width(half - 56.0);
                ui.spacing_mut().item_spacing.y = 12.0;
                let eyebrow = pending.first().map(|p| format!("Appairage · {}", s(p, "name"))).unwrap_or_else(|| "Appairage".into());
                th::eyebrow(ui, &eyebrow, if pending.is_empty() { th::MUTED } else { th::ACCENT });
                th::text(ui, "Saisissez le code affiché sur le client", th::display(22.0), th::TEXT);
                th::text(ui, "Lancez l'appairage depuis le client 2moon : il affiche un code à 4 chiffres, entrez-le ici.", th::body(13.0), th::TEXT2);
                for p in &pending {
                    ui.horizontal(|ui| {
                        th::phase(ui, 14.0, 2, th::ACCENT);
                        th::text(ui, format!("{} · {}", s(p, "name"), s(p, "address")), th::mono(13.0), th::TEXT);
                    });
                }
                pin.retain(|c| c.is_ascii_digit());
                pin.truncate(4);
                let r = th::text_input(ui, pin, "PIN", ui.available_width());
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (th::button(ui, th::Btn::Accent, None, "Valider le code", Some(ui.available_width()), pin.len() == 4).clicked() || enter) && pin.len() == 4 {
                    *pin_submit = true;
                }
            });
        };
        let history = self.host.history.clone();
        if wide {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 20.0;
                ui.vertical(|ui| session_panel(ui, &mut act, &history));
                ui.vertical(|ui| pairing_panel(ui, &mut pin_submit));
            });
        } else {
            session_panel(ui, &mut act, &history);
            ui.add_space(20.0);
            pairing_panel(ui, &mut pin_submit);
        }
        if pin_submit {
            let pin = std::mem::take(&mut self.host.pin);
            let Some(api) = self.host.api.clone() else { return };
            let data = self.host.data.clone();
            self.host.dirty = true;
            self.rt().spawn(async move {
                let m = match api.post("/api/pin", json!({"pin": pin})).await {
                    Ok(v) if v.get("delivered").and_then(|d| d.as_u64()).unwrap_or(0) > 0 => "Code transmis".to_string(),
                    Ok(_) => "Aucune demande d'appairage en attente".to_string(),
                    Err(e) => format!("{e:#}"),
                };
                data.lock().unwrap().notices.push(m);
            });
        }
        if let Some((path, ok)) = act {
            self.host_post(path, json!({}), Some(ok));
        }
    }

    // ------------------------------------------------------------ devices

    fn host_devices_ui(&mut self, ui: &mut egui::Ui) {
        let clients = self.host.data.lock().unwrap().clients.clone().unwrap_or_default();
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if clients.is_empty() {
                row(ui, true, |ui| {
                    th::text(ui, "Aucun appareil appairé.", th::body(15.0), th::MUTED);
                });
            }
            let last = clients.len().saturating_sub(1);
            for (i, c) in clients.iter().enumerate() {
                row(ui, i == last, |ui| {
                    split_row(ui, vec2(120.0, 48.0), s(c, "name"), Some(s(c, "fingerprint")), |ui| {
                        if danger_button(ui, "Révoquer", true).clicked() {
                            self.host.confirm_unpair = Some((s(c, "name").to_string(), s(c, "fingerprint").to_string()));
                        }
                    });
                });
            }
        });
    }

    fn unpair_dialog(&mut self, ctx: &egui::Context) {
        let Some((name, fp)) = self.host.confirm_unpair.clone() else { return };
        let mut close = false;
        let mut confirm = false;
        egui::Modal::new(egui::Id::unique("host-unpair")).frame(th::card(th::white(0.12))).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            th::text(ui, format!("Révoquer « {name} » ?"), th::display(22.0), th::TEXT);
            th::text(ui, "Il devra se réappairer.", th::body(14.0), th::MUTED);
            ui.horizontal(|ui| {
                if th::button(ui, th::Btn::Outline, None, "Annuler", None, true).clicked() {
                    close = true;
                }
                if danger_button(ui, "Révoquer", true).clicked() {
                    confirm = true;
                }
            });
        });
        if confirm {
            self.host_post("/api/clients/unpair", json!({"fingerprint": fp}), Some("Appareil révoqué"));
        }
        if close || confirm {
            self.host.confirm_unpair = None;
        }
    }

    fn regen_dialog(&mut self, ctx: &egui::Context) {
        if !self.host.confirm_regen {
            return;
        }
        let mut close = false;
        let mut confirm = false;
        egui::Modal::new(egui::Id::new("host-regen")).frame(th::card(th::white(0.12))).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            th::text(ui, "Changer le code de l'hôte ?", th::display(22.0), th::TEXT);
            th::text(
                ui,
                "L'ancien code ne fonctionnera plus, tout de suite : les appareils qui se connectent avec lui (même ceux déjà appairés) n'auront plus accès à cet hôte à distance, et un stream en cours par l'accès à distance sera coupé. Il faudra leur donner le nouveau code.",
                th::body(14.0),
                th::MUTED,
            );
            ui.horizontal(|ui| {
                if th::button(ui, th::Btn::Outline, None, "Annuler", None, true).clicked() {
                    close = true;
                }
                if danger_button(ui, "Changer le code", true).clicked() {
                    confirm = true;
                }
            });
        });
        if confirm {
            self.host_post("/api/remote/regenerate", json!({}), Some("Nouveau code créé"));
        }
        if close || confirm {
            self.host.confirm_regen = false;
        }
    }

    // ------------------------------------------------------------ apps

    fn host_apps_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let apps = self.host.data.lock().unwrap().apps.clone();
        // Upload fetched box art as textures.
        {
            let mut d = self.host.data.lock().unwrap();
            for (id, img) in d.covers.drain() {
                let tex = img.map(|i| ctx.load_texture(format!("host-cover-{id}"), i, egui::TextureOptions::LINEAR));
                self.host.covers.insert(id, tex);
            }
        }
        let Some(apps) = apps else {
            th::phases_spinner(ui, self.look.t());
            return;
        };
        let mut action: Option<(&'static str, Value)> = None;
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 48.0), Layout::right_to_left(Align::Center), |ui| {
            if th::button(ui, th::Btn::Outline, Some(th::Icon::Plus(0.0)), "Ajouter", None, self.host.app_edit.is_none()).clicked() {
                self.host.app_edit = Some(AppEdit::default());
            }
        });
        ui.add_space(16.0);
        if let Some(edit) = self.host.app_edit.as_mut() {
            let mut close = false;
            th::panel().inner_margin(28).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 12.0;
                th::text(ui, if edit.id == 0 { "Nouvelle application" } else { "Modifier l'application" }, th::display(22.0), th::TEXT);
                label_desc(ui, "Nom", None);
                th::text_input(ui, &mut edit.title, "Bureau", ui.available_width());
                label_desc(ui, "Commande au lancement", Some(if cfg!(windows) { "Exécutée quand un client lance l'application, arrêtée quand il la quitte. Vide : le bureau seul." } else { "Exécutée par sh -c quand un client lance l'application, arrêtée quand il la quitte. Vide : le bureau seul." }));
                th::text_input(ui, &mut edit.cmd, "steam -bigpicture", ui.available_width());
                ui.horizontal(|ui| {
                    th::switch(ui, &mut edit.hdr);
                    th::text(ui, "Annoncer le HDR pour cette application", th::body(15.0), th::TEXT);
                });
                th::text(ui, "Vignette : se change depuis l'interface web de l'hôte (glisser une image).", th::body(13.0), th::FAINT);
                ui.horizontal(|ui| {
                    if edit.id != 0 && danger_button(ui, "Supprimer", true).clicked() {
                        action = Some(("/api/apps/delete", json!({"id": edit.id})));
                        close = true;
                    }
                    if th::button(ui, th::Btn::Outline, None, "Annuler", None, true).clicked() {
                        close = true;
                    }
                    if th::button(ui, th::Btn::Accent, None, "Enregistrer", None, !edit.title.trim().is_empty()).clicked() {
                        action = Some(("/api/apps/save", json!({"id": edit.id, "title": edit.title.trim(), "cmd": edit.cmd.trim(), "hdr": edit.hdr})));
                        close = true;
                    }
                });
            });
            if close {
                self.host.app_edit = None;
            }
            ui.add_space(24.0);
        }
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            if apps.is_empty() {
                row(ui, true, |ui| {
                    th::text(ui, "Aucune application.", th::body(15.0), th::MUTED);
                });
            }
            let last = apps.len().saturating_sub(1);
            for (i, a) in apps.iter().enumerate() {
                let id = a.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                row(ui, i == last, |ui| {
                    ui.horizontal(|ui| {
                        // Box art 2:3 (or a phase glyph).
                        let (cr, _) = ui.allocate_exact_size(vec2(40.0, 60.0), Sense::hover());
                        match self.host.covers.get(&id).cloned().flatten() {
                            Some(tex) => {
                                ui.painter().image(tex.id(), cr, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                            }
                            None => {
                                ui.painter().rect_filled(cr, 8.0, th::white(0.06));
                                th::paint_gamepad(ui.painter(), Rect::from_center_size(cr.center(), Vec2::splat(22.0)), th::MUTED);
                            }
                        }
                        ui.add_space(8.0);
                        let bw = 3.0 * 52.0 + 110.0;
                        ui.allocate_ui_with_layout(vec2((ui.available_width() - bw).max(120.0), 60.0), Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 4.0;
                            ui.horizontal(|ui| {
                                th::text(ui, s(a, "title"), th::semibold(15.0), th::TEXT);
                                if b(a, "hdr") {
                                    th::pill(ui, th::PillKind::Outline(th::TEXT2), 3, "HDR");
                                }
                                if b(a, "running") {
                                    th::pill(ui, th::PillKind::Filled(th::ACCENT), 3, "EN COURS");
                                }
                            });
                            let cmd = s(a, "cmd");
                            th::text(ui, if cmd.is_empty() { "Bureau (aucune commande)" } else { cmd }, th::mono(12.0), th::MUTED);
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if th::button(ui, th::Btn::Outline, None, "Modifier", None, self.host.app_edit.is_none()).clicked() {
                                self.host.app_edit = Some(AppEdit { id, title: s(a, "title").into(), cmd: s(a, "cmd").into(), hdr: b(a, "hdr") });
                            }
                            if th::button(ui, th::Btn::Ghost, None, "↓", Some(44.0), i < last).on_hover_text("Descendre").clicked() {
                                action = Some(("/api/apps/move", json!({"id": id, "dir": 1})));
                            }
                            if th::button(ui, th::Btn::Ghost, None, "↑", Some(44.0), i > 0).on_hover_text("Monter").clicked() {
                                action = Some(("/api/apps/move", json!({"id": id, "dir": -1})));
                            }
                        });
                    });
                });
            }
        });
        if let Some((path, body)) = action {
            let ok = match path {
                "/api/apps/save" => Some("Application enregistrée"),
                "/api/apps/delete" => Some("Application supprimée"),
                _ => None,
            };
            self.host_post(path, body, ok);
        }
    }

    // ------------------------------------------------------------ info

    fn host_info_ui(&mut self, ui: &mut egui::Ui) {
        let st = self.host.data.lock().unwrap().status.clone().unwrap_or_default();
        let ports = st.get("ports").cloned().unwrap_or_default();
        let codecs = st.get("codecs").cloned().unwrap_or_default();
        let codec_list: Vec<String> = ["av1", "h264", "hevc"].iter().filter(|c| b(&codecs, c)).map(|c| c.to_uppercase()).collect();
        let rows: Vec<(&str, String)> = vec![
            ("Version", s(&st, "version").into()),
            ("En service depuis", fmt_uptime(n(&st, "uptime_s"))),
            ("Capture", format!("{} · audio {}", s(&st, "video_source"), s(&st, "audio_source"))),
            ("Encodeur", format!("{} (preset {})", s(&st, "encoder"), s(&st, "nvenc_preset"))),
            ("Codecs", codec_list.join(" · ")),
            ("Ports", format!("HTTP {} · HTTPS {} · RTSP {}", n(&ports, "http"), n(&ports, "https"), n(&ports, "rtsp"))),
            ("Entrées reçues", format!("{}", n(&st, "input_events"))),
        ];
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let last = rows.len() - 1;
            for (i, (k, v)) in rows.iter().enumerate() {
                row(ui, i == last, |ui| {
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(vec2(200.0, 20.0), Layout::left_to_right(Align::Center), |ui| {
                            th::text(ui, *k, th::body(14.0), th::MUTED);
                        });
                        th::text(ui, v, th::mono(14.0), th::TEXT);
                    });
                });
            }
        });
    }

    // ------------------------------------------------------------ log

    fn host_logs_ui(&mut self, ui: &mut egui::Ui) {
        let logs = self.host.data.lock().unwrap().logs.clone();
        ui.horizontal(|ui| {
            th::switch(ui, &mut self.host.follow);
            th::text(ui, "suivre", th::mono(13.0), th::TEXT2);
        });
        ui.add_space(12.0);
        th::panel().inner_margin(20).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            if logs.is_empty() {
                th::text(ui, "Journal vide.", th::mono(12.0), th::MUTED);
            }
            for l in &logs {
                let col = if l.contains(" ERROR ") {
                    th::WARN
                } else if l.contains(" WARN ") {
                    Color32::from_rgb(0xFF, 0xC8, 0x57)
                } else {
                    th::TEXT2
                };
                th::text(ui, l, th::mono(12.0), col);
            }
        });
    }

    // ------------------------------------------------------------ settings

    fn host_settings_ui(&mut self, ui: &mut egui::Ui) {
        let (set, state) = {
            let d = self.host.data.lock().unwrap();
            (d.settings.clone(), d.state.clone())
        };
        let Some(set) = set else {
            th::phases_spinner(ui, self.look.t());
            return;
        };
        let mut patch: Option<Value> = None;
        let mut copied = false;
        let mut regen = false;
        let section_head = |ui: &mut egui::Ui, idx: u8, title: &str| {
            egui::Frame::new().inner_margin(egui::Margin::symmetric(28, 20)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    th::phase(ui, 18.0, idx, th::ACCENT);
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).font(th::display(20.0)).color(th::TEXT));
                });
            });
            th::rule(ui, th::DIVIDER);
        };

        // Paramètres (the web panel's settings, same order and wording).
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            section_head(ui, 0, "Paramètres");
            // Keyboard layout for typed text.
            row(ui, false, |ui| {
                let current = s(&set, "keyboard").to_string();
                let detected = s(&set, "keyboard_detected").to_string();
                let mut options: Vec<(String, String)> = vec![("auto".into(), format!("Automatique (détecté : {detected})"))];
                if let Some(list) = set.get("keyboards").and_then(|k| k.as_array()) {
                    options.extend(list.iter().map(|k| (s(k, "id").to_string(), s(k, "label").to_string())));
                }
                split_row(ui, vec2(320.0, 40.0), "Type de clavier", Some("Disposition utilisée pour taper le texte envoyé par les clients (saisie de texte, coller, clavier du téléphone). Les touches, elles, suivent toujours la disposition active de l'hôte."), |ui| {
                    if let Some(v) = combo(ui, "host-kb", &current, &options, 320.0) {
                        patch = Some(json!({"keyboard": v}));
                    }
                });
            });
            // Host name.
            row(ui, false, |ui| {
                let saved = s(&set, "host_name").to_string();
                let default = s(&set, "host_name_default").to_string();
                let edit = self.host.name_edit.get_or_insert_with(|| saved.clone());
                split_row(ui, vec2(320.0, 48.0), "Nom de l'hôte", Some("Nom affiché dans les clients 2moon. Vide : le nom du système."), |ui| {
                    let r = th::text_input(ui, edit, &default, 320.0);
                    if r.lost_focus() && edit.trim() != saved {
                        patch = Some(json!({"host_name": edit.trim()}));
                    }
                });
            });
            // Encoder preset (NVENC only).
            if b(&set, "nvenc_preset_available") { row(ui, false, |ui| {
                let current = s(&set, "nvenc_preset").to_string();
                let mut options: Vec<(String, String)> = vec![(String::new(), format!("Par défaut ({})", s(&set, "nvenc_preset_default")))];
                for i in 1..=7 {
                    let p = format!("p{i}");
                    let label = match i {
                        1 => format!("{p} · la plus rapide"),
                        4 => format!("{p} · équilibrée"),
                        7 => format!("{p} · la meilleure"),
                        _ => p.clone(),
                    };
                    options.push((p, label));
                }
                split_row(ui, vec2(320.0, 40.0), "Qualité d'encodage", Some("Preset de l'encodeur NVENC : plus la qualité est haute, plus le GPU travaille. Appliqué au prochain lancement de stream."), |ui| {
                    if let Some(v) = combo(ui, "host-preset", &current, &options, 320.0) {
                        patch = Some(json!({"nvenc_preset": v}));
                    }
                });
            }); }
            // Automatic stop.
            row(ui, false, |ui| {
                let current = (n(&set, "auto_quit_min") as u64).to_string();
                let options: Vec<(String, String)> = set
                    .get("auto_quit_choices")
                    .and_then(|c| c.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m.as_u64())
                            .map(|m| {
                                let label = if m == 0 {
                                    "Jamais".to_string()
                                } else if m < 60 {
                                    format!("Après {m} min")
                                } else {
                                    format!("Après {} h", m / 60)
                                };
                                (m.to_string(), label)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                split_row(ui, vec2(320.0, 40.0), "Arrêt automatique", Some("Quitte l'application lancée (et arrête sa commande) quand aucun client ne streame depuis ce temps."), |ui| {
                    if let Some(v) = combo(ui, "host-autoquit", &current, &options, 320.0) {
                        patch = Some(json!({"auto_quit_min": v.parse::<u64>().unwrap_or(0)}));
                    }
                });
            });
            // Remote access (tunnel).
            let code = set.get("remote_id").and_then(|s| s.as_str()).map(String::from);
            row(ui, code.is_none(), |ui| {
                let available = b(&set, "remote_available");
                let wanted = b(&set, "remote");
                let mut on = wanted && available;
                let state = set.pointer("/remote_status/state").and_then(|s| s.as_str()).unwrap_or("");
                let desc = if !available {
                    "Désactivé au démarrage du service (--no-remote).".to_string()
                } else if !wanted || state == "off" {
                    "Les clients 2moon joignent l'hôte depuis Internet sans rien ouvrir sur le routeur : connexion directe à travers le NAT, ou par un serveur relais en dernier recours. Passe par les serveurs publics d'iroh (n0).".to_string()
                } else if state == "online" {
                    "Actif : joignable depuis Internet".to_string()
                } else if state == "failed" {
                    format!("Échec : {}", set.pointer("/remote_status/error").and_then(|s| s.as_str()).unwrap_or(""))
                } else {
                    "Démarrage…".to_string()
                };
                split_row(ui, vec2(52.0, 30.0), "Accès à distance", Some(&desc), |ui| {
                    ui.add_enabled_ui(available, |ui| {
                        if th::switch(ui, &mut on).on_hover_text("Joindre l'hôte depuis n'importe où, sans ouvrir de port").changed() {
                            patch = Some(json!({"remote": on}));
                        }
                    });
                });
            });
            if let Some(code) = code {
                row(ui, true, |ui| {
                    let desc = format!("{code}\nÀ saisir dans 2moon (Ajouter un ordinateur) pour s'y connecter depuis l'extérieur. Les appareils déjà appairés le reçoivent tout seuls. Il faut toujours le code PIN pour appairer un nouvel appareil.");
                    split_row(ui, vec2(330.0, 48.0), "Code de l'hôte", Some(&desc), |ui| {
                        if danger_button(ui, "Changer le code", true).clicked() {
                            regen = true;
                        }
                        if th::button(ui, th::Btn::Outline, None, "Copier", None, true).clicked() {
                            ui.ctx().copy_text(code.clone());
                            copied = true;
                        }
                    });
                });
            }
        });
        ui.add_space(32.0);

        // Web panel account: create it (first use) or change its password.
        let setup_required = state.as_ref().is_some_and(|s| b(s, "setup_required"));
        let mut pw_action: Option<(&'static str, Value)> = None;
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            section_head(ui, 2, "Mot de passe");
            row(ui, false, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                let w = ui.available_width().min(420.0);
                if setup_required {
                    label_desc(ui, "Compte de l'interface web", Some("Première utilisation : créez le compte administrateur du panneau web (8 caractères minimum)."));
                    th::text_input(ui, &mut self.host.setup_user, "Identifiant", w);
                    password_input(ui, &mut self.host.setup_pass, "Mot de passe", w);
                    let ok = !self.host.setup_user.trim().is_empty() && self.host.setup_pass.chars().count() >= 8;
                    if th::button(ui, th::Btn::OutlineStrong, None, "Créer le compte", None, ok).clicked() {
                        pw_action = Some(("/api/setup", json!({"user": self.host.setup_user.trim(), "password": std::mem::take(&mut self.host.setup_pass)})));
                    }
                } else {
                    label_desc(ui, "Mot de passe", Some("Mot de passe de ce panneau d'administration."));
                    password_input(ui, &mut self.host.pw_current, "Mot de passe actuel", w);
                    password_input(ui, &mut self.host.pw_new, "Nouveau mot de passe", w);
                    let ok = !self.host.pw_current.is_empty() && self.host.pw_new.chars().count() >= 8;
                    if th::button(ui, th::Btn::OutlineStrong, None, "Changer", None, ok).clicked() {
                        pw_action = Some(("/api/password", json!({"current": std::mem::take(&mut self.host.pw_current), "new": std::mem::take(&mut self.host.pw_new)})));
                    }
                }
            });
            // Address of the web panel.
            row(ui, true, |ui| {
                let port = self.host.api.as_ref().map(|a| a.port()).unwrap_or(host_api::DEFAULT_ADMIN_PORT);
                let url = format!("http://{}:{port}", local_ip().unwrap_or_else(|| "127.0.0.1".into()));
                split_row(ui, vec2(260.0, 48.0), "Interface web", Some(&url), |ui| {
                    if th::button(ui, th::Btn::Outline, None, "Ouvrir l'interface web", None, true).clicked() {
                        open_url(&format!("http://127.0.0.1:{port}"));
                    }
                });
            });
        });
        ui.add_space(32.0);

        // The service itself.
        let mut svc: Option<&'static str> = None;
        th::panel().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            section_head(ui, 3, "Service");
            row(ui, true, |ui| {
                split_row(ui, vec2(300.0, 48.0), "Hôte 2moon", Some("L'hôte tourne en service, même quand cette application est fermée."), |ui| {
                    if danger_button(ui, "Arrêter l'hôte", !cfg!(windows)).clicked() {
                        svc = Some("stop");
                    }
                    if th::button(ui, th::Btn::Outline, None, "Redémarrer", None, !cfg!(windows)).clicked() {
                        svc = Some("start");
                    }
                });
            });
        });

        if copied {
            self.toast("Code copié");
        }
        if regen {
            self.host.confirm_regen = true;
        }
        if let Some(p) = patch {
            if p.get("host_name").is_some() {
                self.host.name_edit = None;
            }
            self.host_post("/api/settings", p, Some("Paramètre enregistré"));
        }
        if let Some((path, body)) = pw_action {
            let ok = if path == "/api/setup" { "Compte créé" } else { "Mot de passe changé" };
            self.host_post(path, body, Some(ok));
        }
        match svc {
            Some("stop") => self.service_action("stop"),
            Some("start") => {
                // Restart: stop then start again (launchd / systemd).
                let data = self.host.data.clone();
                self.host.api = None;
                self.rt().spawn_blocking(move || {
                    let _ = host_api::stop_service();
                    std::thread::sleep(Duration::from_millis(500));
                    let r = host_api::start_service();
                    data.lock().unwrap().notices.push(match r {
                        Ok(()) => "Hôte redémarré".into(),
                        Err(e) => format!("{e:#}"),
                    });
                });
            }
            _ => {}
        }
    }
}

/// Red outline button (destructive actions).
fn danger_button(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    let red = Color32::from_rgb(0xFF, 0x80, 0x80);
    let g = ui.painter().layout_no_wrap(label.to_string(), th::body(15.0), red);
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 40.0, 48.0), if enabled { Sense::click() } else { Sense::hover() });
    let fade = if enabled { 1.0 } else { 0.4 };
    let fill = if enabled && resp.hovered() { th::alpha(red, 0.14) } else { th::alpha(red, 0.08 * fade) };
    ui.painter().rect(r, 12.0, fill, Stroke::new(1.0, th::alpha(red, 0.55 * fade)), egui::StrokeKind::Inside);
    ui.painter().galley(r.center() - g.size() / 2.0, g, th::alpha(red, fade));
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A password field in the design's style.
fn password_input(ui: &mut egui::Ui, value: &mut String, hint: &str, width: f32) -> egui::Response {
    let (r, _) = ui.allocate_exact_size(vec2(width, 48.0), Sense::hover());
    let id = ui.scope_id().with(("password", hint));
    let focused = ui.memory(|m| m.has_focus(id));
    ui.painter().rect(r, 12.0, th::BG, Stroke::new(1.0, if focused { th::ACCENT } else { th::BORDER_STRONG }), egui::StrokeKind::Inside);
    let edit = egui::TextEdit::singleline(value)
        .id(id)
        .password(true)
        .frame(egui::Frame::NONE)
        .font(th::body(15.0))
        .text_color(th::TEXT)
        .hint_text(RichText::new(hint).font(th::body(15.0)).color(th::FAINT))
        .align(egui::Align2::LEFT_CENTER)
        .desired_width(r.width() - 32.0);
    ui.place(r.shrink2(vec2(16.0, 0.0)), edit)
}

/// Drop-down list; returns the newly chosen value.
fn combo(ui: &mut egui::Ui, id: &str, current: &str, options: &[(String, String)], width: f32) -> Option<String> {
    let label = options.iter().find(|o| o.0 == current).map(|o| o.1.clone()).unwrap_or_else(|| current.to_string());
    let mut chosen = None;
    egui::ComboBox::from_id_salt(id).width(width).selected_text(RichText::new(label).font(th::body(14.0)).color(th::TEXT)).show_ui(ui, |ui| {
        for (v, l) in options {
            if ui.selectable_label(v == current, RichText::new(l).font(th::body(14.0))).clicked() && v != current {
                chosen = Some(v.clone());
            }
        }
    });
    chosen
}

/// Bitrate over the last two minutes, with the target as a dashed line.
fn bitrate_chart(ui: &mut egui::Ui, history: &VecDeque<f32>, target: f32) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 90.0), Sense::hover());
    let p = ui.painter();
    let max = history.iter().copied().fold(target * 1.1, f32::max).max(1.0);
    let y_of = |v: f32| r.bottom() - (v / max) * (r.height() - 6.0);
    let ty = y_of(target);
    p.extend(egui::Shape::dashed_line(&[pos2(r.left(), ty), pos2(r.right(), ty)], Stroke::new(1.0, th::BORDER_STRONG), 4.0, 4.0));
    p.text(pos2(r.left() + 4.0, (ty - 6.0).max(r.top() + 12.0)), egui::Align2::LEFT_BOTTOM, format!("cible {target:.0} Mbit/s"), th::mono(11.0), th::MUTED);
    if history.len() < 2 {
        return;
    }
    let step = r.width() / 119.0;
    let x0 = r.right() - (history.len() - 1) as f32 * step;
    let pts: Vec<egui::Pos2> = history.iter().enumerate().map(|(i, v)| pos2(x0 + i as f32 * step, y_of(*v))).collect();
    // Area under the curve, fading down.
    let mut m = egui::Mesh::default();
    for (i, pt) in pts.iter().enumerate() {
        m.colored_vertex(*pt, th::alpha(th::ACCENT, 0.22));
        m.colored_vertex(pos2(pt.x, r.bottom()), th::alpha(th::ACCENT, 0.0));
        if i > 0 {
            let k = (i * 2) as u32;
            m.add_triangle(k - 2, k - 1, k);
            m.add_triangle(k - 1, k, k + 1);
        }
    }
    p.add(egui::Shape::mesh(m));
    p.add(egui::Shape::line(pts, Stroke::new(2.0, th::ACCENT)));
}

/// This computer's address on the local network (for the web panel's link).
fn local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

fn open_url(url: &str) {
    let r = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = r {
        tracing::warn!("cannot open {url}: {e}");
    }
}
