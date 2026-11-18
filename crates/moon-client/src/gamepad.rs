//! Game controllers read with SDL3, like Moonlight: the same HIDAPI drivers
//! (PlayStation, Switch, Xbox, Steam controllers) and the same mapping
//! database, so a controller Moonlight understands is understood here. Each is
//! sent to the host as GameStream gamepad state; the host sees an Xbox 360 pad.
//!
//! Up to four controllers, numbered in the order they appear. A controller's
//! state is sent when it changes; when the stream loses focus a neutral state
//! is sent so nothing stays held. Rumble from the host is played back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use moon_proto::control::input::{pad, Event, Gamepad};
use tracing::{debug, info, warn};

use crate::control::ControlSender;
use crate::sdl::{self, axis, button, Api, JoystickId};

/// GameStream hosts (and Moonlight) handle four controllers.
const MAX_PADS: usize = 4;

/// Rumble request from the host: controller number, low- and high-frequency motor (0..65535).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rumble {
    pub index: u16,
    pub low: u16,
    pub high: u16,
}

/// Parse the host's rumble message (4 unused bytes, then three little-endian u16).
pub fn parse_rumble(payload: &[u8]) -> Option<Rumble> {
    let u = |o: usize| payload.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    Some(Rumble { index: u(4)?, low: u(6)?, high: u(8)? })
}

/// SDL trigger (0..32767) to GameStream (0..255), as Moonlight does.
pub fn trigger(v: i16) -> u8 {
    (v.max(0) as u32 * 255 / 32767) as u8
}

/// SDL stick Y (down-positive) to GameStream (up-positive).
pub fn flip_y(v: i16) -> i16 {
    (-(v as i32)).clamp(-32768, 32767) as i16
}

/// GameStream state of one controller.
fn state_of(api: &Api, g: *mut sdl::Gamepad) -> Gamepad {
    let mut buttons = 0;
    for (b, flag) in [
        (button::SOUTH, pad::A),
        (button::EAST, pad::B),
        (button::WEST, pad::X),
        (button::NORTH, pad::Y),
        (button::LEFT_SHOULDER, pad::LB),
        (button::RIGHT_SHOULDER, pad::RB),
        (button::BACK, pad::BACK),
        (button::START, pad::START),
        (button::GUIDE, pad::HOME),
        (button::LEFT_STICK, pad::LS),
        (button::RIGHT_STICK, pad::RS),
        (button::DPAD_UP, pad::UP),
        (button::DPAD_DOWN, pad::DOWN),
        (button::DPAD_LEFT, pad::LEFT),
        (button::DPAD_RIGHT, pad::RIGHT),
    ] {
        if unsafe { (api.gamepad_button)(g, b) } {
            buttons |= flag;
        }
    }
    let a = |x| unsafe { (api.gamepad_axis)(g, x) };
    Gamepad {
        index: 0,
        active_mask: 0,
        buttons,
        left_trigger: trigger(a(axis::LEFT_TRIGGER)),
        right_trigger: trigger(a(axis::RIGHT_TRIGGER)),
        ls_x: a(axis::LEFTX),
        ls_y: flip_y(a(axis::LEFTY)),
        rs_x: a(axis::RIGHTX),
        rs_y: flip_y(a(axis::RIGHTY)),
    }
}

/// Assigns controller numbers: the lowest free one, kept while connected.
#[derive(Default)]
pub struct Slots {
    by_id: HashMap<JoystickId, u16>,
}

impl Slots {
    pub fn assign(&mut self, id: JoystickId) -> Option<u16> {
        if let Some(i) = self.by_id.get(&id) {
            return Some(*i);
        }
        let i = (0..MAX_PADS as u16).find(|i| !self.by_id.values().any(|v| v == i))?;
        self.by_id.insert(id, i);
        Some(i)
    }
    pub fn release(&mut self, id: JoystickId) -> Option<u16> {
        self.by_id.remove(&id)
    }
    pub fn mask(&self) -> u16 {
        self.by_id.values().fold(0, |m, i| m | 1 << i)
    }
    pub fn id_of(&self, index: u16) -> Option<JoystickId> {
        self.by_id.iter().find(|(_, i)| **i == index).map(|(id, _)| *id)
    }
}

/// An open controller.
struct Open {
    pad: *mut sdl::Gamepad,
    name: String,
}

/// Steam Input's virtual pad: a mirror of a physical controller while Steam runs.
fn is_steam_virtual(vendor: u16, product: u16, name: &str) -> bool {
    (vendor == 0x28de && product == 0x11ff) || name.contains("Steam Virtual")
}

/// The Xbox 360 pad a 2moon host on this same computer created: never send it back.
fn is_our_host_pad(vendor: u16, product: u16, path: &str) -> bool {
    if (vendor, product) != (0x045e, 0x028e) || !cfg!(target_os = "linux") {
        return false;
    }
    // uinput devices live under /sys/devices/virtual; a real pad never does.
    let Some(ev) = path.strip_prefix("/dev/input/") else { return false };
    let Ok(real) = std::fs::canonicalize(format!("/sys/class/input/{ev}")) else { return false };
    let name = std::fs::read_to_string(real.join("device/name")).unwrap_or_default();
    real.to_string_lossy().contains("/devices/virtual/") && name.trim() == "Microsoft X-Box 360 pad"
}

/// Open a controller SDL announced, unless it must not be forwarded (like SDL
/// and so Moonlight: Steam Input's virtual pads are ignored unless Steam
/// launched us and asks for them with `SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1`;
/// otherwise, with Steam open on this computer, the host would get a controller
/// nobody is holding, and games switch to controller mode and ignore the keyboard).
fn open(api: &Api, id: JoystickId) -> Option<Open> {
    let pad = unsafe { (api.open_gamepad)(id) };
    if pad.is_null() {
        warn!("cannot open controller {id}: {}", api.error());
        return None;
    }
    let (name, vendor, product, kind, path) = unsafe {
        (
            sdl::cstr((api.gamepad_name)(pad)),
            (api.gamepad_vendor)(pad),
            (api.gamepad_product)(pad),
            sdl::cstr((api.type_string)((api.gamepad_type)(pad))),
            sdl::cstr((api.gamepad_path)(pad)),
        )
    };
    let steam_ok = std::env::var("SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD").is_ok_and(|v| v == "1");
    let skip = if is_steam_virtual(vendor, product, &name) && !steam_ok {
        Some("Steam Input virtual pad")
    } else if is_our_host_pad(vendor, product, &path) {
        Some("a 2moon host's virtual pad")
    } else {
        None
    };
    if let Some(why) = skip {
        info!("controller ignored: {name} ({vendor:04x}:{product:04x}, {why})");
        unsafe { (api.close_gamepad)(pad) };
        return None;
    }
    info!("controller found: {name} ({vendor:04x}:{product:04x}, type {}, {path})", if kind.is_empty() { "?" } else { &kind });
    Some(Open { pad, name })
}

/// Read controllers until `stop`, sending their state to the host while `focus` is set.
pub fn run(ctl: ControlSender, stop: Arc<AtomicBool>, focus: Arc<AtomicBool>, rumble: Receiver<Rumble>) {
    let api = match sdl::api() {
        Ok(a) => a,
        Err(e) => {
            warn!("game controllers unavailable: {e:#}");
            return;
        }
    };
    // Set before init, as Moonlight does. No signal handlers: SDL would take Ctrl+C.
    api.hint("SDL_NO_SIGNAL_HANDLERS", "1");
    api.hint("SDL_APP_NAME", "2moon");
    // We have no SDL window: always deliver controller input (focus is ours to handle).
    api.hint("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
    if !unsafe { (api.init_subsystem)(sdl::INIT_GAMEPAD) } {
        warn!("game controllers unavailable: SDL init failed: {}", api.error());
        return;
    }
    info!("game controllers: SDL {}", api.version());

    let mut open_pads: HashMap<JoystickId, Open> = HashMap::new();
    let mut slots = Slots::default();
    let mut sent: HashMap<u16, Gamepad> = HashMap::new();
    let mut was_focused = true;

    let add = |id: JoystickId, open_pads: &mut HashMap<JoystickId, Open>, slots: &mut Slots| {
        if open_pads.contains_key(&id) {
            return;
        }
        if let Some(o) = open(api, id) {
            match slots.assign(id) {
                Some(i) => info!("controller {}: {}", i + 1, o.name),
                None => info!("controller not forwarded (already {MAX_PADS}): {}", o.name),
            }
            open_pads.insert(id, o);
        }
    };
    for id in api.gamepads() {
        add(id, &mut open_pads, &mut slots);
    }

    let mut ev = sdl::Event::new();
    while !stop.load(Ordering::Relaxed) {
        // Wait for something to happen (bounded, to check stop / focus / rumble).
        let mut changed = false;
        let mut got = unsafe { (api.wait_event_timeout)(&mut ev, 8) };
        while got {
            match ev.kind() {
                sdl::EVENT_GAMEPAD_ADDED => add(ev.which(), &mut open_pads, &mut slots),
                sdl::EVENT_GAMEPAD_REMOVED => {
                    if let Some(o) = open_pads.remove(&ev.which()) {
                        unsafe { (api.close_gamepad)(o.pad) };
                    }
                    if let Some(i) = slots.release(ev.which()) {
                        info!("controller {} disconnected", i + 1);
                        sent.remove(&i);
                        // The host removes its virtual pad when its bit leaves the mask.
                        ctl.input(&Event::Gamepad(Gamepad { index: i, active_mask: slots.mask(), ..Default::default() }));
                    }
                }
                sdl::EVENT_GAMEPAD_AXIS_MOTION | sdl::EVENT_GAMEPAD_BUTTON_DOWN | sdl::EVENT_GAMEPAD_BUTTON_UP | sdl::EVENT_GAMEPAD_REMAPPED => {}
                _ => {}
            }
            changed = true;
            got = unsafe { (api.poll_event)(&mut ev) };
        }
        while let Ok(r) = rumble.try_recv() {
            if let Some(o) = slots.id_of(r.index).and_then(|id| open_pads.get(&id)) {
                // The host repeats the command while the game wants rumble; play a
                // while longer than its period so it does not stutter. (0, 0) stops it.
                if !unsafe { (api.rumble_gamepad)(o.pad, r.low, r.high, 60_000) } {
                    debug!("rumble: {}", api.error());
                }
            }
        }
        let focused = focus.load(Ordering::Relaxed);
        if !focused {
            if was_focused {
                // Release everything on the host.
                let mask = slots.mask();
                for (i, s) in sent.iter_mut() {
                    *s = Gamepad { index: *i, active_mask: mask, ..Default::default() };
                    ctl.input(&Event::Gamepad(s.clone()));
                }
            }
            was_focused = false;
            continue;
        }
        if !changed && was_focused {
            continue;
        }
        was_focused = true;
        let mask = slots.mask();
        for (id, o) in &open_pads {
            let Some(i) = slots.assign(*id) else { continue };
            let mut g = state_of(api, o.pad);
            g.index = i;
            g.active_mask = mask | 1 << i;
            if sent.get(&i) != Some(&g) {
                ctl.input(&Event::Gamepad(g.clone()));
                sent.insert(i, g);
            }
        }
    }
    // Leave nothing held, and unplug our pads on the host.
    for i in sent.keys() {
        ctl.input(&Event::Gamepad(Gamepad { index: *i, active_mask: 0, ..Default::default() }));
    }
    for (_, o) in open_pads {
        unsafe { (api.close_gamepad)(o.pad) };
    }
    unsafe { (api.quit_subsystem)(sdl::INIT_GAMEPAD) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rumble_message() {
        let p = [0x00, 0xee, 0xff, 0xc0, 1, 0, 0x34, 0x12, 0xff, 0xff];
        assert_eq!(parse_rumble(&p), Some(Rumble { index: 1, low: 0x1234, high: 0xffff }));
        assert_eq!(parse_rumble(&p[..8]), None);
    }

    #[test]
    fn axes_like_moonlight() {
        assert_eq!(trigger(0), 0);
        assert_eq!(trigger(32767), 255);
        assert_eq!(trigger(-5), 0);
        assert_eq!(flip_y(-32768), 32767);
        assert_eq!(flip_y(32767), -32767);
        assert_eq!(flip_y(0), 0);
    }

    #[test]
    fn slots_lowest_free() {
        let mut s = Slots::default();
        assert_eq!(s.assign(10), Some(0));
        assert_eq!(s.assign(11), Some(1));
        assert_eq!(s.release(10), Some(0));
        assert_eq!(s.assign(12), Some(0));
        assert_eq!(s.mask(), 0b11);
        assert_eq!(s.id_of(1), Some(11));
    }
}
