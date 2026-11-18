//! Controller profiles and the guided setup (like Steam's): any controller,
//! catalogued by SDL or not, in any mode (8BitDo X/D/Switch input…), can be
//! taught which of its inputs is each Xbox 360 control.
//!
//! A profile maps each control to a raw input of the controller: a button, or
//! an axis with its resting and fully-pressed values. That covers swapped
//! buttons, inverted axes, triggers on an axis or as buttons, and d-pads
//! reported as a hat. The setup asks for one action at a time (press, then
//! release) and learns rest and full values from what it sees.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gilrs::{Gamepad as Pad, Gilrs};
use moon_proto::control::input::{pad, Gamepad};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The Xbox 360 controls a profile maps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Control {
    A,
    B,
    X,
    Y,
    LB,
    RB,
    LT,
    RT,
    Back,
    Start,
    Home,
    LS,
    RS,
    Up,
    Down,
    Left,
    Right,
    LeftX,
    LeftY,
    RightX,
    RightY,
}

/// The setup's steps: control, what to do (French UI), can it be skipped.
pub const STEPS: [(Control, &str); 21] = [
    (Control::A, "Appuie sur le bouton du bas (A sur Xbox, croix sur PlayStation), puis relâche"),
    (Control::B, "Appuie sur le bouton de droite (B, rond), puis relâche"),
    (Control::X, "Appuie sur le bouton de gauche (X, carré), puis relâche"),
    (Control::Y, "Appuie sur le bouton du haut (Y, triangle), puis relâche"),
    (Control::Up, "Croix directionnelle : appuie vers le haut, puis relâche"),
    (Control::Down, "Croix directionnelle : appuie vers le bas, puis relâche"),
    (Control::Left, "Croix directionnelle : appuie vers la gauche, puis relâche"),
    (Control::Right, "Croix directionnelle : appuie vers la droite, puis relâche"),
    (Control::LB, "Appuie sur la gâchette haute gauche (LB, L1), puis relâche"),
    (Control::RB, "Appuie sur la gâchette haute droite (RB, R1), puis relâche"),
    (Control::LT, "Enfonce à fond la gâchette basse gauche (LT, L2), puis relâche"),
    (Control::RT, "Enfonce à fond la gâchette basse droite (RT, R2), puis relâche"),
    (Control::Back, "Appuie sur Select / Back / Share / −, puis relâche"),
    (Control::Start, "Appuie sur Start / Menu / Options / +, puis relâche"),
    (Control::Home, "Appuie sur le bouton central (Xbox, PS, Home) — « Passer » s'il n'y en a pas"),
    (Control::LS, "Clique le stick gauche (enfonce-le), puis relâche"),
    (Control::RS, "Clique le stick droit (enfonce-le), puis relâche"),
    (Control::LeftX, "Pousse le stick gauche à fond vers la DROITE, puis lâche-le"),
    (Control::LeftY, "Pousse le stick gauche à fond vers le HAUT, puis lâche-le"),
    (Control::RightX, "Pousse le stick droit à fond vers la DROITE, puis lâche-le"),
    (Control::RightY, "Pousse le stick droit à fond vers le HAUT, puis lâche-le"),
];

/// A raw input of the controller.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// A button (value 0..1, analog for pressure-sensitive ones).
    Button { code: u32 },
    /// An axis (-1..1 as gilrs reports it) with its resting and full values.
    Axis { code: u32, rest: f32, full: f32 },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub map: Vec<(Control, Source)>,
}

/// Profiles by controller identifier (SDL GUID, hex), saved next to the client identity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Profiles {
    pub by_uuid: HashMap<String, Profile>,
}

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where profiles live (the client's config directory); call once at startup.
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

fn path() -> Option<PathBuf> {
    DIR.get().map(|d| d.join("controllers.json"))
}

impl Profiles {
    pub fn load() -> Self {
        path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Err(e) = serde_json::to_string_pretty(self).map_err(std::io::Error::other).and_then(|s| std::fs::write(&p, s)) {
            warn!("cannot save controller profiles: {e}");
        }
    }

    pub fn get(&self, p: &Pad<'_>) -> Option<&Profile> {
        self.by_uuid.get(&uuid_of(p))
    }
}

pub fn uuid_of(p: &Pad<'_>) -> String {
    p.uuid().iter().map(|b| format!("{b:02x}")).collect()
}

/// Raw values of a controller: code -> (is axis, value).
fn raw(p: &Pad<'_>) -> HashMap<u32, (bool, f32)> {
    let st = p.state();
    let mut m: HashMap<u32, (bool, f32)> = st.buttons().map(|(c, d)| (c.into_u32(), (false, d.value()))).collect();
    m.extend(st.axes().map(|(c, d)| (c.into_u32(), (true, d.value()))));
    m
}

/// Value of a source: 0..1 for buttons and half axes, -1..1 relative to rest for axes.
fn value(raw: &HashMap<u32, (bool, f32)>, s: &Source) -> f32 {
    match *s {
        Source::Button { code } => raw.get(&code).map(|v| v.1).unwrap_or(0.0),
        Source::Axis { code, rest, full } => {
            // Never reported since it was plugged in: still at its resting position.
            let v = raw.get(&code).map(|v| v.1).unwrap_or(rest);
            let span = full - rest;
            if span.abs() < 1e-3 {
                0.0
            } else {
                ((v - rest) / span).clamp(-1.0, 1.0)
            }
        }
    }
}

/// GameStream state of a controller through its profile.
pub fn state_with(p: &Pad<'_>, prof: &Profile) -> Gamepad {
    let r = raw(p);
    let get = |c: Control| prof.map.iter().find(|(k, _)| *k == c).map(|(_, s)| value(&r, s)).unwrap_or(0.0);
    let mut buttons = 0;
    for (c, flag) in [
        (Control::A, pad::A),
        (Control::B, pad::B),
        (Control::X, pad::X),
        (Control::Y, pad::Y),
        (Control::LB, pad::LB),
        (Control::RB, pad::RB),
        (Control::Back, pad::BACK),
        (Control::Start, pad::START),
        (Control::Home, pad::HOME),
        (Control::LS, pad::LS),
        (Control::RS, pad::RS),
        (Control::Up, pad::UP),
        (Control::Down, pad::DOWN),
        (Control::Left, pad::LEFT),
        (Control::Right, pad::RIGHT),
    ] {
        if get(c) > 0.5 {
            buttons |= flag;
        }
    }
    let trig = |c| (get(c).clamp(0.0, 1.0) * 255.0).round() as u8;
    let stick = |c| {
        let v = get(c);
        // Small dead zone: worn sticks rarely rest exactly where they did during setup.
        let v = if v.abs() < 0.06 { 0.0 } else { v };
        (v * 32767.0).round() as i16
    };
    Gamepad {
        index: 0,
        active_mask: 0,
        buttons,
        left_trigger: trig(Control::LT),
        right_trigger: trig(Control::RT),
        ls_x: stick(Control::LeftX),
        ls_y: stick(Control::LeftY),
        rs_x: stick(Control::RightX),
        rs_y: stick(Control::RightY),
    }
}

/// GameStream state: through the profile if this controller has one, else the SDL mapping.
pub fn state(p: &Pad<'_>, profiles: &Profiles) -> Gamepad {
    match profiles.get(p) {
        Some(prof) => state_with(p, prof),
        None => crate::gamepad::state_of(p),
    }
}

// ---------------------------------------------------------------- setup

/// Learns one control: the input that moves most, its peak, and where it settles.
#[derive(Default)]
pub struct Learner {
    /// Per code: (is axis, first value, min, max, last value, last change).
    seen: HashMap<u32, (bool, f32, f32, f32, f32, Instant)>,
}

impl Learner {
    /// Feed the controller's raw values; returns the source once pressed and released.
    pub fn feed(&mut self, now: Instant, raw: &HashMap<u32, (bool, f32)>) -> Option<Source> {
        for (code, (axis, v)) in raw {
            let e = self.seen.entry(*code).or_insert((*axis, *v, *v, *v, *v, now));
            if (e.4 - v).abs() > 1e-4 {
                e.4 = *v;
                e.5 = now;
            }
            e.2 = e.2.min(*v);
            e.3 = e.3.max(*v);
        }
        // The input that moved the most (at least half its travel).
        let (code, &(axis, _, min, max, last, changed)) = self.seen.iter().filter(|(_, e)| e.3 - e.2 > 0.5).max_by(|a, b| (a.1 .3 - a.1 .2).total_cmp(&(b.1 .3 - b.1 .2)))?;
        // Released: back away from the peak and stable for a moment.
        let peak = if (max - last).abs() > (min - last).abs() { max } else { min };
        if (peak - last).abs() < 0.5 || now.duration_since(changed) < Duration::from_millis(250) {
            return None;
        }
        Some(if axis { Source::Axis { code: *code, rest: last, full: peak } } else { Source::Button { code: *code } })
    }
}

// ---------------------------------------------------------------- settings service

/// How a connected controller is understood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Known {
    /// A profile made with the setup.
    Profile,
    /// In SDL's controller database.
    Sdl,
    /// Unknown: standard Linux layout assumed (run the setup if it is wrong).
    Guessed,
}

#[derive(Clone, Debug)]
pub struct PadInfo {
    pub id: usize,
    pub name: String,
    pub known: Known,
    /// Live GameStream state, as the host would receive it.
    pub state: Gamepad,
}

#[derive(Clone, Debug)]
pub struct Wizard {
    pub pad: usize,
    pub step: usize,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    pub pads: Vec<PadInfo>,
    pub wizard: Option<Wizard>,
    /// Last result, to show under the list.
    pub message: Option<String>,
}

pub enum Cmd {
    Configure(usize),
    Skip,
    Cancel,
    Reset(usize),
}

/// Controllers for the settings screen (own thread, while the screen is open).
pub struct Service {
    pub view: Arc<Mutex<View>>,
    tx: Sender<Cmd>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Service {
    pub fn start(repaint: impl Fn() + Send + 'static) -> Self {
        let view = Arc::new(Mutex::new(View::default()));
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (view, stop) = (view.clone(), stop.clone());
            let _ = std::thread::Builder::new().name("pad-setup".into()).spawn(move || service(view, rx, stop, repaint));
        }
        Self { view, tx, stop }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn service(view: Arc<Mutex<View>>, rx: Receiver<Cmd>, stop: Arc<std::sync::atomic::AtomicBool>, repaint: impl Fn()) {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(gilrs::Error::NotImplemented(g)) => g,
        Err(e) => {
            warn!("game controllers unavailable: {e}");
            view.lock().unwrap().message = Some(format!("Manettes indisponibles : {e}"));
            repaint();
            return;
        }
    };
    let mut profiles = Profiles::load();
    // Setup in progress: pad, step, learner, profile being built.
    let mut setup: Option<(usize, usize, Learner, Profile)> = None;
    let mut last: Option<(Vec<(usize, Gamepad)>, Option<(usize, usize)>)> = None;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        while gilrs.next_event_blocking(Some(Duration::from_millis(10))).is_some() {}
        let mut message = None;
        while let Ok(c) = rx.try_recv() {
            match c {
                Cmd::Configure(id) => {
                    if let Some((_, p)) = gilrs.gamepads().find(|(g, _)| usize::from(*g) == id) {
                        setup = Some((id, 0, Learner::default(), Profile { name: p.name().to_string(), map: Vec::new() }));
                    }
                }
                Cmd::Skip => {
                    if let Some(s) = setup.as_mut() {
                        s.1 += 1;
                        s.2 = Learner::default();
                    }
                }
                Cmd::Cancel => {
                    setup = None;
                    message = Some("Configuration annulée.".to_string());
                }
                Cmd::Reset(id) => {
                    if let Some((_, p)) = gilrs.gamepads().find(|(g, _)| usize::from(*g) == id) {
                        profiles.by_uuid.remove(&uuid_of(&p));
                        profiles.save();
                        message = Some(format!("{} : réglage d'origine rétabli.", p.name()));
                    }
                }
            }
        }
        // Setup: learn the current step.
        if let Some((id, step, learner, prof)) = setup.as_mut() {
            match gilrs.gamepads().find(|(g, _)| usize::from(*g) == *id) {
                None => {
                    setup = None;
                    message = Some("Manette débranchée : configuration annulée.".into());
                }
                Some((_, p)) => {
                    if let Some(src) = learner.feed(Instant::now(), &raw(&p)) {
                        prof.map.push((STEPS[*step].0, src));
                        *step += 1;
                        *learner = Learner::default();
                    }
                    if *step >= STEPS.len() {
                        let uuid = uuid_of(&p);
                        info!("controller profile saved for {} ({uuid}): {:?}", prof.name, prof.map);
                        message = Some(format!("{} configurée : elle sera envoyée à l'hôte comme une manette Xbox 360.", prof.name));
                        profiles.by_uuid.insert(uuid, prof.clone());
                        profiles.save();
                        setup = None;
                    }
                }
            }
        }
        let pads: Vec<PadInfo> = gilrs
            .gamepads()
            .filter(|(_, p)| !p.name().starts_with("2moon virtual"))
            .map(|(id, p)| {
                let known = if profiles.get(&p).is_some() {
                    Known::Profile
                } else if p.mapping_source() == gilrs::MappingSource::SdlMappings {
                    Known::Sdl
                } else {
                    Known::Guessed
                };
                PadInfo { id: usize::from(id), name: p.name().to_string(), known, state: state(&p, &profiles) }
            })
            .collect();
        let snapshot = (pads.iter().map(|p| (p.id, p.state.clone())).collect::<Vec<_>>(), setup.as_ref().map(|s| (s.0, s.1)));
        let changed = last.as_ref() != Some(&snapshot) || message.is_some() || last.as_ref().map(|l| l.0.len()) != Some(pads.len());
        if changed {
            let mut v = view.lock().unwrap();
            v.pads = pads;
            v.wizard = setup.as_ref().map(|s| Wizard { pad: s.0, step: s.1 });
            if message.is_some() {
                v.message = message;
            }
            drop(v);
            last = Some(snapshot);
            repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(l: &mut Learner, t: Instant, vals: &[(u32, bool, f32)]) -> Option<Source> {
        let m = vals.iter().map(|(c, a, v)| (*c, (*a, *v))).collect();
        l.feed(t, &m)
    }

    #[test]
    fn learns_trigger_resting_at_minus_one() {
        let mut l = Learner::default();
        let t = Instant::now();
        // The trigger was never reported before: first value already slightly pressed.
        assert_eq!(feed(&mut l, t, &[(2, true, -0.95)]), None);
        assert_eq!(feed(&mut l, t + Duration::from_millis(50), &[(2, true, 1.0)]), None);
        assert_eq!(feed(&mut l, t + Duration::from_millis(100), &[(2, true, -1.0)]), None);
        let s = feed(&mut l, t + Duration::from_millis(400), &[(2, true, -1.0)]);
        assert_eq!(s, Some(Source::Axis { code: 2, rest: -1.0, full: 1.0 }));
        let raw = [(2u32, (true, 0.0f32))].into_iter().collect();
        assert!((value(&raw, &s.unwrap()) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn learns_hat_direction_and_button() {
        let t = Instant::now();
        let mut l = Learner::default();
        // D-pad up on a hat: -1 when pressed, 0 at rest, other axes noise.
        feed(&mut l, t, &[(17, true, -1.0), (0, true, 0.02)]);
        assert_eq!(feed(&mut l, t + Duration::from_millis(80), &[(17, true, 0.0), (0, true, 0.03)]), None);
        assert_eq!(feed(&mut l, t + Duration::from_millis(400), &[(17, true, 0.0), (0, true, 0.03)]), Some(Source::Axis { code: 17, rest: 0.0, full: -1.0 }));
        let mut l = Learner::default();
        feed(&mut l, t, &[(304, false, 1.0)]);
        feed(&mut l, t + Duration::from_millis(60), &[(304, false, 0.0)]);
        assert_eq!(feed(&mut l, t + Duration::from_millis(400), &[(304, false, 0.0)]), Some(Source::Button { code: 304 }));
    }
}
