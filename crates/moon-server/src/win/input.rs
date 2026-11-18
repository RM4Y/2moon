//! Input injection on Windows: keyboard and mouse with SendInput, gamepads as
//! virtual Xbox 360 controllers through the ViGEmBus driver (as Sunshine).
//!
//! The protocol already speaks Windows: keys are virtual-key codes and gamepad
//! buttons are XInput flags. Keys are sent as scan codes, which games read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use moon_proto::control::input::{Event, Gamepad};
use tracing::{debug, info, warn};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN};

use crate::input::Injector;

/// A virtual Xbox 360 pad and the rumble it was last asked for.
struct Pad {
    target: Xbox360Wired<Arc<Client>>,
    rumble: Arc<Mutex<Option<(u8, u8)>>>,
    _notifications: Option<std::thread::JoinHandle<()>>,
}

pub struct SendInputInjector {
    /// Connection to ViGEmBus (None: driver missing, no gamepads).
    vigem: Option<Arc<Client>>,
    vigem_warned: bool,
    pads: HashMap<u16, Pad>,
    held_keys: Vec<u16>,
    held_buttons: Vec<u8>,
}

impl SendInputInjector {
    pub fn new() -> Result<Self> {
        let vigem = match Client::connect() {
            Ok(c) => {
                info!("ViGEmBus connected: gamepads available");
                Some(Arc::new(c))
            }
            Err(e) => {
                warn!("ViGEmBus not available ({e}): gamepads disabled (install ViGEmBus)");
                None
            }
        };
        Ok(Self { vigem, vigem_warned: false, pads: HashMap::new(), held_keys: Vec::new(), held_buttons: Vec::new() })
    }

    fn pad_event(&mut self, g: &Gamepad) {
        if g.active_mask & (1 << g.index) == 0 {
            // Controller unplugged on the client.
            if let Some(mut p) = self.pads.remove(&g.index) {
                let _ = p.target.unplug();
            }
            return;
        }
        if !self.pads.contains_key(&g.index) {
            let Some(client) = self.vigem.clone() else {
                if !self.vigem_warned {
                    warn!("gamepad ignored: ViGEmBus is not installed");
                    self.vigem_warned = true;
                }
                return;
            };
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            if let Err(e) = target.plugin().and_then(|_| target.wait_ready()) {
                warn!("cannot create gamepad {}: {e}", g.index);
                return;
            }
            let rumble = Arc::new(Mutex::new(None));
            let notifications = match target.request_notification() {
                Ok(n) => {
                    let r = rumble.clone();
                    Some(n.spawn_thread(move |_, data| {
                        *r.lock().unwrap() = Some((data.large_motor, data.small_motor));
                    }))
                }
                Err(e) => {
                    debug!("no rumble for gamepad {}: {e}", g.index);
                    None
                }
            };
            info!("virtual Xbox 360 controller {} plugged in", g.index);
            self.pads.insert(g.index, Pad { target, rumble, _notifications: notifications });
        }
        let pad = self.pads.get_mut(&g.index).unwrap();
        let report = XGamepad {
            // Same bit layout as XInput's buttons.
            buttons: XButtons { raw: g.buttons as u16 },
            left_trigger: g.left_trigger,
            right_trigger: g.right_trigger,
            thumb_lx: g.ls_x,
            thumb_ly: g.ls_y,
            thumb_rx: g.rs_x,
            thumb_ry: g.rs_y,
        };
        if let Err(e) = pad.target.update(&report) {
            debug!("gamepad {} update: {e}", g.index);
        }
    }
}

/// Send input events; if Windows refuses them (the input desktop changed, e.g. a UAC
/// prompt), switch to the current input desktop and try once more.
fn send(inputs: &[INPUT]) {
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if (n as usize) < inputs.len() && crate::win::session::follow_input_desktop() {
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}

fn key_input(vk: u16, down: bool) -> INPUT {
    // Scan code with the 0xE0 prefix for extended keys (arrows, Insert, right Ctrl...).
    let sc = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
    let mut flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    let (wvk, scan) = if sc != 0 {
        flags |= KEYEVENTF_SCANCODE;
        if sc & 0xff00 == 0xe000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        (0, (sc & 0xff) as u16)
    } else {
        (vk, 0)
    };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(wvk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn unicode_input(unit: u16, down: bool) -> INPUT {
    let flags = if down { KEYEVENTF_UNICODE } else { KEYEVENTF_UNICODE | KEYEVENTF_KEYUP };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: unit, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn button_flags(button: u8, down: bool) -> Option<(MOUSE_EVENT_FLAGS, i32)> {
    const XBUTTON1: i32 = 1;
    const XBUTTON2: i32 = 2;
    Some(match (button, down) {
        (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (1, false) => (MOUSEEVENTF_LEFTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (3, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (4, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (5, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (5, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        _ => return None,
    })
}

/// Client position (x, y in a w x h view) to SendInput's absolute coordinates
/// (0..65535 over the virtual desktop), on the display being captured.
fn absolute(x: i16, y: i16, w: i16, h: i16) -> (i32, i32) {
    let (vx, vy, vw, vh) = unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1)) };
    let (ox, oy, ow, oh) = crate::win::capture::CAPTURED_RECT.lock().unwrap().unwrap_or((vx, vy, vw, vh));
    let (w, h) = ((w as i64).max(1), (h as i64).max(1));
    let px = ox as i64 + (x as i64).clamp(0, w - 1) * ow as i64 / w;
    let py = oy as i64 + (y as i64).clamp(0, h - 1) * oh as i64 / h;
    let ax = (px - vx as i64) * 65535 / (vw as i64 - 1).max(1);
    let ay = (py - vy as i64) * 65535 / (vh as i64 - 1).max(1);
    (ax as i32, ay as i32)
}

impl Injector for SendInputInjector {
    fn handle(&mut self, ev: &Event) {
        match ev {
            Event::Key { down, vk, .. } => {
                if *down {
                    if !self.held_keys.contains(vk) {
                        self.held_keys.push(*vk);
                    }
                } else {
                    self.held_keys.retain(|k| k != vk);
                }
                send(&[key_input(*vk, *down)]);
            }
            Event::MouseRel { dx, dy } => send(&[mouse_input(MOUSEEVENTF_MOVE, *dx as i32, *dy as i32, 0)]),
            Event::MouseAbs { x, y, width, height } => {
                let (ax, ay) = absolute(*x, *y, *width, *height);
                send(&[mouse_input(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, ax, ay, 0)]);
            }
            Event::MouseButton { down, button } => {
                let Some((flags, data)) = button_flags(*button, *down) else { return };
                if *down {
                    self.held_buttons.push(*button);
                } else {
                    self.held_buttons.retain(|b| b != button);
                }
                send(&[mouse_input(flags, 0, 0, data)]);
            }
            Event::Scroll { amount } => send(&[mouse_input(MOUSEEVENTF_WHEEL, 0, 0, *amount as i32)]),
            Event::HScroll { amount } => send(&[mouse_input(MOUSEEVENTF_HWHEEL, 0, 0, *amount as i32)]),
            Event::Text(t) => {
                // Unicode input: any character, whatever the host's keyboard layout.
                let inputs: Vec<INPUT> = t.encode_utf16().flat_map(|u| [unicode_input(u, true), unicode_input(u, false)]).collect();
                if !inputs.is_empty() {
                    send(&inputs);
                }
            }
            Event::Gamepad(g) => self.pad_event(g),
        }
    }

    fn reset(&mut self) {
        for vk in std::mem::take(&mut self.held_keys) {
            send(&[key_input(vk, false)]);
        }
        for b in std::mem::take(&mut self.held_buttons) {
            if let Some((flags, data)) = button_flags(b, false) {
                send(&[mouse_input(flags, 0, 0, data)]);
            }
        }
        for (_, mut p) in self.pads.drain() {
            let _ = p.target.unplug();
        }
    }

    fn rumble(&mut self) -> Vec<(u16, u16, u16)> {
        let mut out = Vec::new();
        for (index, p) in &self.pads {
            if let Some((large, small)) = p.rumble.lock().unwrap().take() {
                // 0-255 from XInput to the protocol's 0-65535.
                out.push((*index, large as u16 * 257, small as u16 * 257));
            }
        }
        out
    }
}
