//! Input injection: virtual keyboard, mice and gamepads through uinput.

use std::collections::HashMap;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent, KeyCode, RelativeAxisCode, UinputAbsSetup};
use moon_proto::control::input::{pad, Event, Gamepad};
use tracing::{debug, info, warn};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
const ABS_RANGE: i32 = 65535;

pub trait Injector: Send {
    fn handle(&mut self, ev: &Event);
    /// Release every key/button still held (on disconnect).
    fn reset(&mut self) {}
}

/// Records events without touching the system (tests, `--no-input`).
#[derive(Default)]
pub struct LogInjector {
    pub count: u64,
}

impl Injector for LogInjector {
    fn handle(&mut self, ev: &Event) {
        self.count += 1;
        info!("input: {ev:?}");
    }
}

pub struct UinputInjector {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    tablet: VirtualDevice,
    pads: HashMap<u16, (VirtualDevice, Gamepad)>,
    held_keys: Vec<u16>,
    held_buttons: Vec<u16>,
    typer: Option<crate::text::TextTyper>,
    /// (layout, variant) the typer was built for.
    typer_layout: (String, String),
    /// Layout wanted now (admin panel setting, or detected on the host).
    layout_fn: LayoutFn,
}

pub type LayoutFn = Box<dyn Fn() -> (String, String) + Send>;

fn make_typer(layout: &(String, String)) -> Option<crate::text::TextTyper> {
    match crate::text::TextTyper::new(&layout.0, &layout.1) {
        Ok(t) => Some(t),
        Err(e) => {
            warn!("text input disabled: {e:#}");
            None
        }
    }
}

fn ev(t: u16, c: u16, v: i32) -> InputEvent {
    InputEvent::new(t, c, v)
}

fn emit(dev: &mut VirtualDevice, events: &[InputEvent]) {
    let mut all = events.to_vec();
    all.push(ev(EV_SYN, SYN_REPORT, 0));
    if let Err(e) = dev.emit(&all) {
        warn!("uinput emit failed: {e}");
    }
}

fn mouse_buttons() -> AttributeSet<KeyCode> {
    let mut b = AttributeSet::<KeyCode>::new();
    for k in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA] {
        b.insert(k);
    }
    b
}

impl UinputInjector {
    pub fn new(layout_fn: LayoutFn) -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()
            .context("open /dev/uinput (is the user in the 'input' group?)")?
            .name("2moon virtual keyboard")
            .with_keys(&keys)?
            .build()?;

        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for a in [RelativeAxisCode::REL_X, RelativeAxisCode::REL_Y, RelativeAxisCode::REL_WHEEL, RelativeAxisCode::REL_HWHEEL, RelativeAxisCode::REL_WHEEL_HI_RES, RelativeAxisCode::REL_HWHEEL_HI_RES] {
            rel.insert(a);
        }
        let mouse = VirtualDevice::builder()?.name("2moon virtual mouse").with_keys(&mouse_buttons())?.with_relative_axes(&rel)?.build()?;

        // Absolute pointer modelled after virtual-machine tablets (understood by libinput as a pointer).
        let abs = AbsInfo::new(0, 0, ABS_RANGE, 0, 0, 0);
        let tablet = VirtualDevice::builder()?
            .name("2moon virtual absolute pointer")
            .with_keys(&mouse_buttons())?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs))?
            .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs))?
            .build()?;
        info!("uinput devices created (keyboard, mouse, absolute pointer)");
        let typer_layout = layout_fn();
        let typer = make_typer(&typer_layout);
        Ok(Self { keyboard, mouse, tablet, pads: HashMap::new(), held_keys: vec![], held_buttons: vec![], typer, typer_layout, layout_fn })
    }

    fn gamepad(index: u16) -> Result<VirtualDevice> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for k in [
            KeyCode::BTN_SOUTH,
            KeyCode::BTN_EAST,
            KeyCode::BTN_NORTH,
            KeyCode::BTN_WEST,
            KeyCode::BTN_TL,
            KeyCode::BTN_TR,
            KeyCode::BTN_SELECT,
            KeyCode::BTN_START,
            KeyCode::BTN_MODE,
            KeyCode::BTN_THUMBL,
            KeyCode::BTN_THUMBR,
        ] {
            keys.insert(k);
        }
        let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
        let trig = AbsInfo::new(0, 0, 255, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let name = format!("2moon virtual gamepad {index}");
        let mut b = VirtualDevice::builder()?.name(name.as_bytes()).input_id(evdev::InputId::new(evdev::BusType::BUS_USB, 0x045e, 0x028e, 0x110)).with_keys(&keys)?;
        for (a, i) in [
            (AbsoluteAxisCode::ABS_X, stick),
            (AbsoluteAxisCode::ABS_Y, stick),
            (AbsoluteAxisCode::ABS_RX, stick),
            (AbsoluteAxisCode::ABS_RY, stick),
            (AbsoluteAxisCode::ABS_Z, trig),
            (AbsoluteAxisCode::ABS_RZ, trig),
            (AbsoluteAxisCode::ABS_HAT0X, hat),
            (AbsoluteAxisCode::ABS_HAT0Y, hat),
        ] {
            b = b.with_absolute_axis(&UinputAbsSetup::new(a, i))?;
        }
        Ok(b.build()?)
    }

    fn pad_event(&mut self, g: &Gamepad) {
        if g.active_mask & (1 << g.index) == 0 {
            // Controller unplugged on the client.
            self.pads.remove(&g.index);
            return;
        }
        if !self.pads.contains_key(&g.index) {
            match Self::gamepad(g.index) {
                Ok(d) => {
                    self.pads.insert(g.index, (d, Gamepad::default()));
                }
                Err(e) => {
                    warn!("cannot create gamepad: {e:#}");
                    return;
                }
            }
        }
        let (dev, prev) = self.pads.get_mut(&g.index).unwrap();
        let mut evs = Vec::new();
        for (flag, key) in [
            (pad::A, KeyCode::BTN_SOUTH),
            (pad::B, KeyCode::BTN_EAST),
            (pad::X, KeyCode::BTN_WEST),
            (pad::Y, KeyCode::BTN_NORTH),
            (pad::LB, KeyCode::BTN_TL),
            (pad::RB, KeyCode::BTN_TR),
            (pad::BACK, KeyCode::BTN_SELECT),
            (pad::START, KeyCode::BTN_START),
            (pad::HOME, KeyCode::BTN_MODE),
            (pad::LS, KeyCode::BTN_THUMBL),
            (pad::RS, KeyCode::BTN_THUMBR),
        ] {
            let now = g.buttons & flag != 0;
            if now != (prev.buttons & flag != 0) {
                evs.push(ev(EV_KEY, key.code(), now as i32));
            }
        }
        let hx = (g.buttons & pad::RIGHT != 0) as i32 - (g.buttons & pad::LEFT != 0) as i32;
        let hy = (g.buttons & pad::DOWN != 0) as i32 - (g.buttons & pad::UP != 0) as i32;
        // Stick Y is up-positive on the wire, down-positive in evdev.
        for (code, v) in [
            (AbsoluteAxisCode::ABS_X, g.ls_x as i32),
            (AbsoluteAxisCode::ABS_Y, -(g.ls_y as i32).clamp(-32767, 32767)),
            (AbsoluteAxisCode::ABS_RX, g.rs_x as i32),
            (AbsoluteAxisCode::ABS_RY, -(g.rs_y as i32).clamp(-32767, 32767)),
            (AbsoluteAxisCode::ABS_Z, g.left_trigger as i32),
            (AbsoluteAxisCode::ABS_RZ, g.right_trigger as i32),
            (AbsoluteAxisCode::ABS_HAT0X, hx),
            (AbsoluteAxisCode::ABS_HAT0Y, hy),
        ] {
            evs.push(ev(EV_ABS, code.0, v));
        }
        emit(dev, &evs);
        *prev = *g;
    }
}

fn button_code(b: u8) -> Option<u16> {
    Some(match b {
        1 => KeyCode::BTN_LEFT.code(),
        2 => KeyCode::BTN_MIDDLE.code(),
        3 => KeyCode::BTN_RIGHT.code(),
        4 => KeyCode::BTN_SIDE.code(),
        5 => KeyCode::BTN_EXTRA.code(),
        _ => return None,
    })
}

impl Injector for UinputInjector {
    fn handle(&mut self, e: &Event) {
        match e {
            Event::Key { down, vk, .. } => {
                let Some(code) = vk_to_linux(*vk) else {
                    debug!("unmapped virtual key 0x{vk:02x}");
                    return;
                };
                if *down {
                    self.held_keys.push(code);
                } else {
                    self.held_keys.retain(|k| *k != code);
                }
                emit(&mut self.keyboard, &[ev(EV_KEY, code, *down as i32)]);
            }
            Event::MouseRel { dx, dy } => emit(&mut self.mouse, &[ev(EV_REL, REL_X, *dx as i32), ev(EV_REL, REL_Y, *dy as i32)]),
            Event::MouseAbs { x, y, width, height } => {
                let (w, h) = ((*width).max(1) as i64, (*height).max(1) as i64);
                let ax = (*x as i64).clamp(0, w - 1) * ABS_RANGE as i64 / (w - 1).max(1);
                let ay = (*y as i64).clamp(0, h - 1) * ABS_RANGE as i64 / (h - 1).max(1);
                emit(&mut self.tablet, &[ev(EV_ABS, 0, ax as i32), ev(EV_ABS, 1, ay as i32)]);
            }
            Event::MouseButton { down, button } => {
                let Some(code) = button_code(*button) else { return };
                if *down {
                    self.held_buttons.push(code);
                } else {
                    self.held_buttons.retain(|b| *b != code);
                }
                emit(&mut self.mouse, &[ev(EV_KEY, code, *down as i32)]);
            }
            Event::Scroll { amount } => {
                let a = *amount as i32;
                let mut evs = vec![ev(EV_REL, REL_WHEEL_HI_RES, a)];
                if a % 120 == 0 {
                    evs.push(ev(EV_REL, REL_WHEEL, a / 120));
                }
                emit(&mut self.mouse, &evs);
            }
            Event::HScroll { amount } => {
                let a = *amount as i32;
                let mut evs = vec![ev(EV_REL, REL_HWHEEL_HI_RES, a)];
                if a % 120 == 0 {
                    evs.push(ev(EV_REL, REL_HWHEEL, a / 120));
                }
                emit(&mut self.mouse, &evs);
            }
            Event::Text(t) => {
                // Follow the keyboard chosen in the admin panel without restarting.
                let want = (self.layout_fn)();
                if want != self.typer_layout {
                    info!("text input now uses keyboard layout {}{}", want.0, if want.1.is_empty() { String::new() } else { format!(" ({})", want.1) });
                    self.typer = make_typer(&want);
                    self.typer_layout = want;
                }
                let Some(typer) = &self.typer else { return };
                for c in t.chars() {
                    let Some(st) = typer.stroke(c) else {
                        debug!("character {c:?} not reachable on layout {}", typer.layout);
                        continue;
                    };
                    let mut evs: Vec<InputEvent> = st.mods.iter().map(|m| ev(EV_KEY, *m, 1)).collect();
                    evs.push(ev(EV_KEY, st.key, 1));
                    emit(&mut self.keyboard, &evs);
                    let mut evs = vec![ev(EV_KEY, st.key, 0)];
                    evs.extend(st.mods.iter().rev().map(|m| ev(EV_KEY, *m, 0)));
                    emit(&mut self.keyboard, &evs);
                }
            }
            Event::Gamepad(g) => self.pad_event(g),
        }
    }

    fn reset(&mut self) {
        for k in std::mem::take(&mut self.held_keys) {
            emit(&mut self.keyboard, &[ev(EV_KEY, k, 0)]);
        }
        for b in std::mem::take(&mut self.held_buttons) {
            emit(&mut self.mouse, &[ev(EV_KEY, b, 0)]);
        }
        self.pads.clear();
    }
}

/// Windows virtual-key code to Linux input event code.
pub fn vk_to_linux(vk: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    const DIGITS: [u16; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const NUMPAD: [u16; 10] = [82, 79, 80, 81, 75, 76, 77, 71, 72, 73];
    const FKEYS: [u16; 24] = [59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194];
    Some(match vk {
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x60..=0x69 => NUMPAD[(vk - 0x60) as usize],
        0x70..=0x87 => FKEYS[(vk - 0x70) as usize],
        0x08 => 14,
        0x09 => 15,
        0x0D => 28,
        0x10 | 0xA0 => 42,
        0xA1 => 54,
        0x11 | 0xA2 => 29,
        0xA3 => 97,
        0x12 | 0xA4 => 56,
        0xA5 => 100,
        0x13 => 119,
        0x14 => 58,
        0x1B => 1,
        0x20 => 57,
        0x21 => 104,
        0x22 => 109,
        0x23 => 107,
        0x24 => 102,
        0x25 => 105,
        0x26 => 103,
        0x27 => 106,
        0x28 => 108,
        0x2C => 99,
        0x2D => 110,
        0x2E => 111,
        0x5B => 125,
        0x5C => 126,
        0x5D => 127,
        0x6A => 55,
        0x6B => 78,
        0x6D => 74,
        0x6E => 83,
        0x6F => 98,
        0x90 => 69,
        0x91 => 70,
        0xBA => 39,
        0xBB => 13,
        0xBC => 51,
        0xBD => 12,
        0xBE => 52,
        0xBF => 53,
        0xC0 => 41,
        0xDB => 26,
        0xDC => 43,
        0xDD => 27,
        0xDE => 40,
        0xE2 => 86,
        0xAD => 113, // volume mute
        0xAE => 114, // volume down
        0xAF => 115, // volume up
        0xB0 => 163, // next track
        0xB1 => 165, // previous track
        0xB3 => 164, // play/pause
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap() {
        assert_eq!(vk_to_linux(0x41), Some(30)); // A
        assert_eq!(vk_to_linux(0x5A), Some(44)); // Z
        assert_eq!(vk_to_linux(0x30), Some(11)); // 0
        assert_eq!(vk_to_linux(0x0D), Some(28)); // Enter
        assert_eq!(vk_to_linux(0x7B), Some(88)); // F12
        assert_eq!(vk_to_linux(0xFF), None);
    }
}
