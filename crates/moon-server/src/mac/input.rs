//! macOS input: keyboard and mouse as CoreGraphics events (Accessibility permission).
//! No gamepads. Windows key codes are mapped by position (ANSI).

use std::time::{Duration, Instant};

use moon_proto::control::input::Event;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit};
use tracing::{info, warn};

use crate::input::Injector;

/// Windows virtual-key code → macOS key code (kVK_*).
fn mac_keycode(vk: u16) -> Option<u16> {
    Some(match vk {
        // Letters (A-Z).
        0x41 => 0x00, 0x42 => 0x0B, 0x43 => 0x08, 0x44 => 0x02, 0x45 => 0x0E, 0x46 => 0x03, 0x47 => 0x05, 0x48 => 0x04, 0x49 => 0x22,
        0x4A => 0x26, 0x4B => 0x28, 0x4C => 0x25, 0x4D => 0x2E, 0x4E => 0x2D, 0x4F => 0x1F, 0x50 => 0x23, 0x51 => 0x0C, 0x52 => 0x0F,
        0x53 => 0x01, 0x54 => 0x11, 0x55 => 0x20, 0x56 => 0x09, 0x57 => 0x0D, 0x58 => 0x07, 0x59 => 0x10, 0x5A => 0x06,
        // Digits (top row).
        0x30 => 0x1D, 0x31 => 0x12, 0x32 => 0x13, 0x33 => 0x14, 0x34 => 0x15, 0x35 => 0x17, 0x36 => 0x16, 0x37 => 0x1A, 0x38 => 0x1C, 0x39 => 0x19,
        0x08 => 0x33, // Backspace
        0x09 => 0x30, // Tab
        0x0C => 0x47, // Clear
        0x0D => 0x24, // Return
        0x10 | 0xA0 => 0x38, // Shift
        0xA1 => 0x3C,        // Right Shift
        0x11 | 0xA2 => 0x3B, // Control
        0xA3 => 0x3E,        // Right Control
        0x12 | 0xA4 => 0x3A, // Alt → Option
        0xA5 => 0x3D,        // Right Alt → Right Option
        0x5B => 0x37,        // Windows → Command
        0x5C => 0x36,        // Right Windows → Right Command
        0x14 => 0x39,        // Caps Lock
        0x1B => 0x35,        // Escape
        0x20 => 0x31,        // Space
        0x21 => 0x74,        // Page Up
        0x22 => 0x79,        // Page Down
        0x23 => 0x77,        // End
        0x24 => 0x73,        // Home
        0x25 => 0x7B,        // Left
        0x26 => 0x7E,        // Up
        0x27 => 0x7C,        // Right
        0x28 => 0x7D,        // Down
        0x2D => 0x72,        // Insert → Help
        0x2E => 0x75,        // Delete (forward)
        0x5D => 0x6E,        // Menu (context)
        // Keypad.
        0x60 => 0x52, 0x61 => 0x53, 0x62 => 0x54, 0x63 => 0x55, 0x64 => 0x56, 0x65 => 0x57, 0x66 => 0x58, 0x67 => 0x59, 0x68 => 0x5B, 0x69 => 0x5C,
        0x6A => 0x43, // *
        0x6B => 0x45, // +
        0x6D => 0x4E, // -
        0x6E => 0x41, // .
        0x6F => 0x4B, // /
        0x90 => 0x47, // Num Lock → Clear
        // Function keys F1-F20.
        0x70 => 0x7A, 0x71 => 0x78, 0x72 => 0x63, 0x73 => 0x76, 0x74 => 0x60, 0x75 => 0x61, 0x76 => 0x62, 0x77 => 0x64, 0x78 => 0x65, 0x79 => 0x6D,
        0x7A => 0x67, 0x7B => 0x6F, 0x7C => 0x69, 0x7D => 0x6B, 0x7E => 0x71, 0x7F => 0x6A, 0x80 => 0x40, 0x81 => 0x4F, 0x82 => 0x50, 0x83 => 0x5A,
        // Punctuation (US positions).
        0xBA => 0x29, // ;
        0xBB => 0x18, // =
        0xBC => 0x2B, // ,
        0xBD => 0x1B, // -
        0xBE => 0x2F, // .
        0xBF => 0x2C, // /
        0xC0 => 0x32, // `
        0xDB => 0x21, // [
        0xDC => 0x2A, // \
        0xDD => 0x1E, // ]
        0xDE => 0x27, // '
        0xE2 => 0x0A, // ISO key between left Shift and Z
        // Media.
        0xAD => 0x4A, // Mute
        0xAE => 0x49, // Volume down
        0xAF => 0x48, // Volume up
        _ => return None,
    })
}

/// The modifier flag a key code holds, if it is a modifier.
fn modifier_flag(code: u16) -> Option<CGEventFlags> {
    Some(match code {
        0x38 | 0x3C => CGEventFlags::MaskShift,
        0x3B | 0x3E => CGEventFlags::MaskControl,
        0x3A | 0x3D => CGEventFlags::MaskAlternate,
        0x37 | 0x36 => CGEventFlags::MaskCommand,
        0x39 => CGEventFlags::MaskAlphaShift,
        _ => return None,
    })
}

pub struct CgInjector {
    held_keys: Vec<u16>,
    /// Mouse buttons held (protocol numbers: 1 left, 2 middle, 3 right, 4-5 extra).
    held_buttons: Vec<u8>,
    /// Modifier keys held, by Mac key code.
    held_mods: Vec<u16>,
    /// Last click: (button, time, click count) for double clicks.
    last_click: Option<(u8, Instant, i64)>,
}

impl CgInjector {
    pub fn new() -> anyhow::Result<Self> {
        if !objc2_core_graphics::CGPreflightPostEventAccess() {
            // Opens System Settings › Privacy & Security › Accessibility once.
            objc2_core_graphics::CGRequestPostEventAccess();
            warn!("input injection needs the Accessibility permission: allow moon-server in System Settings › Privacy & Security › Accessibility, then restart it");
        } else {
            info!("input: CoreGraphics events");
        }
        Ok(Self { held_keys: Vec::new(), held_buttons: Vec::new(), held_mods: Vec::new(), last_click: None })
    }

    fn flags(&self) -> CGEventFlags {
        self.held_mods.iter().filter_map(|c| modifier_flag(*c)).fold(CGEventFlags::empty(), |a, f| a | f)
    }

    fn post(&self, ev: Option<CFRetained<CGEvent>>) {
        if let Some(e) = ev {
            CGEvent::set_flags(Some(&e), self.flags() | CGEvent::flags(Some(&e)) & CGEventFlags::MaskNumericPad);
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
        }
    }

    fn key(&mut self, code: u16, down: bool) {
        if modifier_flag(code).is_some() {
            if down {
                if !self.held_mods.contains(&code) {
                    self.held_mods.push(code);
                }
            } else {
                self.held_mods.retain(|c| *c != code);
            }
            let ev = CGEvent::new_keyboard_event(None, code, down);
            if let Some(e) = &ev {
                CGEvent::set_type(Some(e), CGEventType::FlagsChanged);
            }
            self.post(ev);
        } else {
            self.post(CGEvent::new_keyboard_event(None, code, down));
        }
    }

    fn cursor() -> CGPoint {
        CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or(CGPoint { x: 0.0, y: 0.0 })
    }

    /// Move event type: dragged while a button is held.
    fn move_type(&self) -> (CGEventType, CGMouseButton) {
        if self.held_buttons.contains(&1) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.held_buttons.contains(&3) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.held_buttons.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        }
    }

    fn move_to(&self, p: CGPoint, dx: i64, dy: i64) {
        let (t, b) = self.move_type();
        let ev = CGEvent::new_mouse_event(None, t, p, b);
        if let Some(e) = &ev {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaX, dx);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventDeltaY, dy);
        }
        self.post(ev);
    }

    fn button(&mut self, button: u8, down: bool) {
        let (t, b, number) = match (button, down) {
            (1, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (1, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (3, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 1),
            (3, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 1),
            (2, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 2),
            (2, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 2),
            (4 | 5, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, button as i64 - 1),
            (4 | 5, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, button as i64 - 1),
            _ => return,
        };
        // Click count: double / triple clicks within the system's usual interval.
        let clicks = if down {
            let n = match self.last_click {
                Some((b0, at, n)) if b0 == button && at.elapsed() < Duration::from_millis(400) => n + 1,
                _ => 1,
            };
            self.last_click = Some((button, Instant::now(), n));
            n
        } else {
            self.last_click.map(|(_, _, n)| n).unwrap_or(1)
        };
        if down {
            if !self.held_buttons.contains(&button) {
                self.held_buttons.push(button);
            }
        } else {
            self.held_buttons.retain(|x| *x != button);
        }
        let ev = CGEvent::new_mouse_event(None, t, Self::cursor(), b);
        if let Some(e) = &ev {
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, clicks);
        }
        self.post(ev);
    }
}

/// Client position (x, y in a w x h view) to global coordinates on the captured display.
fn absolute(x: i16, y: i16, w: i16, h: i16) -> CGPoint {
    let (ox, oy, ow, oh) = crate::mac::capture::CAPTURED_RECT.lock().unwrap().unwrap_or_else(|| {
        let b = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID());
        (b.origin.x, b.origin.y, b.size.width, b.size.height)
    });
    let (w, h) = ((w as f64).max(1.0), (h as f64).max(1.0));
    CGPoint { x: ox + (x as f64).clamp(0.0, w - 1.0) * ow / w, y: oy + (y as f64).clamp(0.0, h - 1.0) * oh / h }
}

impl Injector for CgInjector {
    fn handle(&mut self, ev: &Event) {
        match ev {
            Event::Key { down, vk, .. } => {
                let Some(code) = mac_keycode(*vk) else { return };
                if *down {
                    if !self.held_keys.contains(&code) {
                        self.held_keys.push(code);
                    }
                } else {
                    self.held_keys.retain(|k| *k != code);
                }
                self.key(code, *down);
            }
            Event::MouseRel { dx, dy } => {
                let p = Self::cursor();
                // Keep the pointer on screen: the global display bounds clamp it anyway.
                self.move_to(CGPoint { x: p.x + *dx as f64, y: p.y + *dy as f64 }, *dx as i64, *dy as i64);
            }
            Event::MouseAbs { x, y, width, height } => {
                let p = absolute(*x, *y, *width, *height);
                let c = Self::cursor();
                self.move_to(p, (p.x - c.x) as i64, (p.y - c.y) as i64);
            }
            Event::MouseButton { down, button } => self.button(*button, *down),
            Event::Scroll { amount } => {
                // 120 per notch; pixel units keep high-resolution wheels and trackpads smooth.
                self.post(CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 1, *amount as i32 / 3, 0, 0));
            }
            Event::HScroll { amount } => {
                self.post(CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Pixel, 2, 0, -(*amount as i32) / 3, 0));
            }
            Event::Text(t) => {
                // Any character, whatever the Mac's keyboard layout.
                let units: Vec<u16> = t.encode_utf16().collect();
                for chunk in units.chunks(16) {
                    for down in [true, false] {
                        let ev = CGEvent::new_keyboard_event(None, 0, down);
                        if let Some(e) = &ev {
                            unsafe { CGEvent::keyboard_set_unicode_string(Some(e), chunk.len() as _, chunk.as_ptr()) };
                        }
                        self.post(ev);
                    }
                }
            }
            Event::Gamepad(_) => {}
        }
    }

    fn reset(&mut self) {
        for code in std::mem::take(&mut self.held_keys) {
            self.key(code, false);
        }
        self.held_mods.clear();
        for b in std::mem::take(&mut self.held_buttons) {
            self.held_buttons.push(b);
            self.button(b, false);
        }
    }
}
