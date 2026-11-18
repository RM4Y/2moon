//! Game controllers (Xbox 360/One/Series, PS4/PS5, Switch Pro and anything
//! SDL knows) read with gilrs and sent to the host as GameStream gamepad
//! state, like Moonlight. The host sees each of them as an Xbox 360 pad.
//!
//! Up to four controllers, numbered in the order they appear. A controller's
//! state is sent when it changes; when the stream loses focus a neutral state
//! is sent so nothing stays held. Rumble from the host is played back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Replay, Ticks};
use gilrs::{Axis, Button, Gamepad as Pad, GamepadId, Gilrs};
use moon_proto::control::input::{pad, Event, Gamepad};
use tracing::{debug, info, warn};

use crate::control::ControlSender;

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

/// GameStream state of one controller.
pub fn state_of(p: &Pad<'_>) -> Gamepad {
    let mut buttons = 0;
    for (b, flag) in [
        (Button::South, pad::A),
        (Button::East, pad::B),
        (Button::West, pad::X),
        (Button::North, pad::Y),
        (Button::LeftTrigger, pad::LB),
        (Button::RightTrigger, pad::RB),
        (Button::Select, pad::BACK),
        (Button::Start, pad::START),
        (Button::Mode, pad::HOME),
        (Button::LeftThumb, pad::LS),
        (Button::RightThumb, pad::RS),
        (Button::DPadUp, pad::UP),
        (Button::DPadDown, pad::DOWN),
        (Button::DPadLeft, pad::LEFT),
        (Button::DPadRight, pad::RIGHT),
    ] {
        if p.is_pressed(b) {
            buttons |= flag;
        }
    }
    // Some controllers report the d-pad as a hat (axes) rather than buttons.
    let (dx, dy) = (p.value(Axis::DPadX), p.value(Axis::DPadY));
    if dx > 0.5 {
        buttons |= pad::RIGHT;
    } else if dx < -0.5 {
        buttons |= pad::LEFT;
    }
    if dy > 0.5 {
        buttons |= pad::UP;
    } else if dy < -0.5 {
        buttons |= pad::DOWN;
    }
    let trigger = |b: Button, a: Axis| {
        let v = p.button_data(b).map(|d| d.value()).unwrap_or_else(|| (p.value(a) + 1.0) / 2.0 * (p.axis_data(a).is_some() as u8 as f32));
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    // gilrs sticks are up-positive, like GameStream.
    let stick = |a: Axis| (p.value(a).clamp(-1.0, 1.0) * 32767.0).round() as i16;
    Gamepad {
        index: 0,
        active_mask: 0,
        buttons,
        left_trigger: trigger(Button::LeftTrigger2, Axis::LeftZ),
        right_trigger: trigger(Button::RightTrigger2, Axis::RightZ),
        ls_x: stick(Axis::LeftStickX),
        ls_y: stick(Axis::LeftStickY),
        rs_x: stick(Axis::RightStickX),
        rs_y: stick(Axis::RightStickY),
    }
}

/// Assigns controller numbers: the lowest free one, kept while connected.
#[derive(Default)]
pub struct Slots {
    by_id: HashMap<GamepadId, u16>,
}

/// Virtual pads created by a 2moon host on this machine: never send them back.
fn ignored(p: &Pad<'_>) -> bool {
    p.name().starts_with("2moon virtual") || p.os_name().starts_with("2moon virtual")
}

/// Steam Input's virtual pad: a mirror of a physical controller while Steam runs.
fn is_steam_virtual(p: &Pad<'_>) -> bool {
    (p.vendor_id() == Some(0x28de) && p.product_id() == Some(0x11ff)) || p.name().contains("Steam Virtual") || p.os_name().contains("Steam Virtual")
}

/// Controllers to forward, like SDL (and so Moonlight): Steam Input's virtual
/// pads are ignored unless Steam launched us and asks for them
/// (`SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1`). Otherwise, with Steam
/// open on this computer, the host would get a controller nobody is holding,
/// and games switch to controller mode and ignore the keyboard.
fn forwarded(gilrs: &Gilrs) -> Vec<GamepadId> {
    let steam_ok = std::env::var("SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD").is_ok_and(|v| v == "1");
    gilrs.gamepads().filter(|(_, p)| !ignored(p) && (steam_ok || !is_steam_virtual(p))).map(|(id, _)| id).collect()
}

impl Slots {
    pub fn assign(&mut self, id: GamepadId) -> Option<u16> {
        if let Some(i) = self.by_id.get(&id) {
            return Some(*i);
        }
        let i = (0..MAX_PADS as u16).find(|i| !self.by_id.values().any(|v| v == i))?;
        self.by_id.insert(id, i);
        Some(i)
    }
    pub fn release(&mut self, id: GamepadId) -> Option<u16> {
        self.by_id.remove(&id)
    }
    pub fn ids(&self) -> Vec<GamepadId> {
        self.by_id.keys().copied().collect()
    }
    pub fn mask(&self) -> u16 {
        self.by_id.values().fold(0, |m, i| m | 1 << i)
    }
    pub fn id_of(&self, index: u16) -> Option<GamepadId> {
        self.by_id.iter().find(|(_, i)| **i == index).map(|(id, _)| *id)
    }
}

/// Read controllers until `stop`, sending their state to the host while `focus` is set.
pub fn run(ctl: ControlSender, stop: Arc<AtomicBool>, focus: Arc<AtomicBool>, rumble: Receiver<Rumble>) {
    let mut gilrs = match Gilrs::new() {
        Ok(g) => g,
        Err(gilrs::Error::NotImplemented(g)) => {
            warn!("game controllers are not supported on this system");
            g
        }
        Err(e) => {
            warn!("game controllers unavailable: {e}");
            return;
        }
    };
    let mut slots = Slots::default();
    let mut sent: HashMap<u16, Gamepad> = HashMap::new();
    let mut effects: HashMap<u16, gilrs::ff::Effect> = HashMap::new();
    let mut was_focused = true;
    for id in forwarded(&gilrs) {
        if let Some(i) = slots.assign(id) {
            let p = gilrs.gamepad(id);
            info!("controller {}: {} (rumble: {}, {})", i + 1, p.name(), p.is_ff_supported(), describe(&p));
        }
    }
    while !stop.load(Ordering::Relaxed) {
        // Wait for something to happen (bounded, to check stop / focus / rumble).
        let mut first = gilrs.next_event_blocking(Some(Duration::from_millis(8)));
        let mut changed = false;
        while let Some(ev) = first.take().or_else(|| gilrs.next_event()) {
            match ev.event {
                gilrs::EventType::Connected => {
                    if forwarded(&gilrs).contains(&ev.id) {
                        if let Some(i) = slots.assign(ev.id) {
                            let p = gilrs.gamepad(ev.id);
                            info!("controller {} connected: {} (rumble: {}, {})", i + 1, p.name(), p.is_ff_supported(), describe(&p));
                        }
                    }
                }
                gilrs::EventType::Disconnected => {
                    if let Some(i) = slots.release(ev.id) {
                        info!("controller {} disconnected", i + 1);
                        effects.remove(&i);
                        sent.remove(&i);
                        // The host removes its virtual pad when its bit leaves the mask.
                        ctl.input(&Event::Gamepad(Gamepad { index: i, active_mask: slots.mask(), ..Default::default() }));
                    }
                }
                _ => {}
            }
            changed = true;
        }
        while let Ok(r) = rumble.try_recv() {
            play_rumble(&mut gilrs, &slots, &mut effects, r);
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
        // A real controller plugged in after a Steam virtual one: drop the latter.
        let keep = forwarded(&gilrs);
        for id in slots.ids() {
            if !keep.contains(&id) {
                if let Some(i) = slots.release(id) {
                    sent.remove(&i);
                    ctl.input(&Event::Gamepad(Gamepad { index: i, active_mask: slots.mask(), ..Default::default() }));
                }
            }
        }
        let mask = slots.mask();
        for id in keep {
            let p = gilrs.gamepad(id);
            let Some(i) = slots.assign(id) else { continue };
            let mut g = state_of(&p);
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
}

/// How a controller is mapped, for the logs.
fn describe(p: &Pad<'_>) -> String {
    if p.mapping_source() == gilrs::MappingSource::SdlMappings {
        "SDL mapping".into()
    } else {
        "no SDL mapping, standard layout assumed".into()
    }
}

fn play_rumble(gilrs: &mut Gilrs, slots: &Slots, effects: &mut HashMap<u16, gilrs::ff::Effect>, r: Rumble) {
    let Some(id) = slots.id_of(r.index) else { return };
    if !gilrs.gamepad(id).is_ff_supported() {
        return;
    }
    // Replace the previous effect of this controller; (0, 0) just stops it.
    if let Some(e) = effects.remove(&r.index) {
        let _ = e.stop();
    }
    if r.low == 0 && r.high == 0 {
        return;
    }
    // The host repeats the command while the game wants rumble; play a while
    // longer than its period so it does not stutter, and stop on (0, 0).
    let replay = Replay { play_for: Ticks::from_ms(60_000), ..Default::default() };
    let built = EffectBuilder::new()
        .add_effect(BaseEffect { kind: BaseEffectType::Strong { magnitude: r.low }, scheduling: replay, ..Default::default() })
        .add_effect(BaseEffect { kind: BaseEffectType::Weak { magnitude: r.high }, scheduling: replay, ..Default::default() })
        .gamepads(&[id])
        .finish(gilrs);
    match built {
        Ok(e) => {
            if let Err(err) = e.play() {
                debug!("rumble: {err}");
            }
            effects.insert(r.index, e);
        }
        Err(err) => debug!("rumble effect: {err}"),
    }
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
}
