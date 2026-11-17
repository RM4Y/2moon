//! Client window: presents decoded pictures and forwards keyboard / mouse
//! input to the host.
//!
//! Shortcuts: Ctrl+Alt+Shift+Q quit, Ctrl+Alt+Shift+Z toggle mouse capture
//! (relative mode), Ctrl+Alt+Shift+F toggle fullscreen, Ctrl+Alt+Shift+S stats.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use moon_proto::control::input::{self, Event as In};
use rayon::prelude::*;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowId};

use crate::control::ControlSender;
use crate::session::{Decoded, I420Frame, Stats};

/// A decoded picture converted for display (packed 0x00RRGGBB).
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

#[derive(Debug)]
pub enum UserEvent {
    NewFrame,
    SessionEnded(String),
}

pub struct FrameSlot {
    latest: Mutex<Option<Picture>>,
    /// Buffer handed back by the window, reused for the next conversion.
    spare: Mutex<Option<Picture>>,
    proxy: EventLoopProxy<UserEvent>,
}

impl FrameSlot {
    pub fn put(&self, d: Decoded<'_>) {
        // This window converts on the CPU: zero-copy is never enabled for it.
        match d {
            Decoded::Cpu(f) => self.put_cpu(f),
            Decoded::Cpu16(f) => {
                // HDR10 -> SDR on the CPU (peak: a typical 1000-nit master).
                let mut p = self.spare.lock().unwrap().take().unwrap_or(Picture { width: 0, height: 0, pixels: Vec::new() });
                p.width = f.width;
                p.height = f.height;
                p.pixels.resize(f.width * f.height, 0);
                f.to_xrgb_hdr(&mut p.pixels, 1000.0 / moon_media::hdr::SDR_WHITE_NITS);
                *self.latest.lock().unwrap() = Some(p);
                let _ = self.proxy.send_event(UserEvent::NewFrame);
            }
            _ => {}
        }
    }

    fn put_cpu(&self, f: &I420Frame) {
        let mut p = self.spare.lock().unwrap().take().or_else(|| self.latest.lock().unwrap().take()).unwrap_or(Picture { width: 0, height: 0, pixels: Vec::new() });
        p.width = f.width;
        p.height = f.height;
        p.pixels.resize(f.width * f.height, 0);
        f.to_xrgb(&mut p.pixels);
        *self.latest.lock().unwrap() = Some(p);
        let _ = self.proxy.send_event(UserEvent::NewFrame);
    }
}

pub fn make_event_loop() -> Result<(EventLoop<UserEvent>, Arc<FrameSlot>)> {
    let el = EventLoop::<UserEvent>::with_user_event().build()?;
    let slot = Arc::new(FrameSlot { latest: Mutex::new(None), spare: Mutex::new(None), proxy: el.create_proxy() });
    Ok((el, slot))
}

pub fn proxy(slot: &FrameSlot) -> EventLoopProxy<UserEvent> {
    slot.proxy.clone()
}

struct Gfx {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    _ctx: softbuffer::Context<Rc<Window>>,
}

pub struct App {
    title: String,
    stream_w: u32,
    stream_h: u32,
    slot: Arc<FrameSlot>,
    ctl: ControlSender,
    stats: Arc<Stats>,
    gfx: Option<Gfx>,
    current: Option<Picture>,
    mods: ModifiersState,
    pressed: HashSet<u16>,
    captured: bool,
    show_stats: bool,
    cursor: Option<PhysicalPosition<f64>>,
    last_title: Instant,
    frames_shown: u64,
    pub exit_reason: Option<String>,
}

impl App {
    pub fn new(title: String, stream_w: u32, stream_h: u32, slot: Arc<FrameSlot>, ctl: ControlSender, stats: Arc<Stats>) -> Self {
        Self {
            title,
            stream_w,
            stream_h,
            slot,
            ctl,
            stats,
            gfx: None,
            current: None,
            mods: ModifiersState::empty(),
            pressed: HashSet::new(),
            captured: false,
            show_stats: false,
            cursor: None,
            last_title: Instant::now(),
            frames_shown: 0,
            exit_reason: None,
        }
    }

    fn modifiers(&self) -> u8 {
        let mut m = 0;
        if self.mods.shift_key() {
            m |= input::MOD_SHIFT;
        }
        if self.mods.control_key() {
            m |= input::MOD_CTRL;
        }
        if self.mods.alt_key() {
            m |= input::MOD_ALT;
        }
        if self.mods.super_key() {
            m |= input::MOD_META;
        }
        m
    }

    fn release_all(&mut self) {
        for vk in self.pressed.drain() {
            self.ctl.input(&In::Key { down: false, vk, modifiers: 0 });
        }
    }

    fn set_capture(&mut self, on: bool) {
        let Some(g) = &self.gfx else { return };
        self.captured = on;
        if on {
            let _ = g.window.set_cursor_grab(CursorGrabMode::Locked).or_else(|_| g.window.set_cursor_grab(CursorGrabMode::Confined));
            g.window.set_cursor_visible(false);
        } else {
            // The host's cursor is drawn in the video: keep ours hidden over the window.
            let _ = g.window.set_cursor_grab(CursorGrabMode::None);
        }
    }

    /// Destination rectangle of the video inside the window (aspect fit).
    fn video_rect(&self, ww: u32, wh: u32) -> (u32, u32, u32, u32) {
        let (sw, sh) = (self.stream_w.max(1) as u64, self.stream_h.max(1) as u64);
        let (ww64, wh64) = (ww as u64, wh as u64);
        let (dw, dh) = if ww64 * sh > wh64 * sw { ((wh64 * sw / sh) as u32, wh) } else { (ww, (ww64 * sh / sw) as u32) };
        ((ww - dw) / 2, (wh - dh) / 2, dw.max(1), dh.max(1))
    }

    fn redraw(&mut self) {
        let Some(g) = &mut self.gfx else { return };
        let size = g.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        if g.surface.resize(w, h).is_err() {
            return;
        }
        let (ww, wh) = (size.width, size.height);
        let (sw, sh) = (self.stream_w, self.stream_h);
        let (ox, oy, dw, dh) = {
            let (s_w, s_h) = (sw.max(1) as u64, sh.max(1) as u64);
            let (dw, dh) = if ww as u64 * s_h > wh as u64 * s_w { ((wh as u64 * s_w / s_h) as u32, wh) } else { (ww, (ww as u64 * s_h / s_w) as u32) };
            ((ww - dw) / 2, (wh - dh) / 2, dw.max(1), dh.max(1))
        };
        let Ok(mut buf) = g.surface.buffer_mut() else { return };
        match &self.current {
            Some(p) => {
                let (pw, ph) = (p.width, p.height);
                let xmap: Vec<usize> = (0..dw as usize).map(|x| (x * pw / dw as usize).min(pw - 1)).collect();
                buf.par_chunks_mut(ww as usize).enumerate().for_each(|(y, row)| {
                    let y = y as u32;
                    if y < oy || y >= oy + dh {
                        row.fill(0);
                        return;
                    }
                    let sy = (((y - oy) as usize) * ph / dh as usize).min(ph - 1);
                    let src = &p.pixels[sy * pw..sy * pw + pw];
                    row[..ox as usize].fill(0);
                    let dst = &mut row[ox as usize..(ox + dw) as usize];
                    if dw as usize == pw {
                        dst.copy_from_slice(src);
                    } else {
                        for (d, &sx) in dst.iter_mut().zip(&xmap) {
                            *d = src[sx];
                        }
                    }
                    row[(ox + dw) as usize..].fill(0);
                });
            }
            None => buf.fill(0x00101018),
        }
        let _ = buf.present();
        self.frames_shown += 1;
    }

    fn update_title(&mut self) {
        if self.last_title.elapsed() < Duration::from_secs(1) {
            return;
        }
        let fps = self.frames_shown as f64 / self.last_title.elapsed().as_secs_f64();
        self.frames_shown = 0;
        self.last_title = Instant::now();
        if let Some(g) = &self.gfx {
            let mut t = format!("{} — {:.0} fps", self.title, fps);
            if self.captured {
                t.push_str(" — souris capturée (Ctrl+Alt+Shift+Z)");
            }
            if self.show_stats {
                t.push_str(&format!(" — {}", self.stats.summary()));
            }
            g.window.set_title(&t);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(&self.title).with_inner_size(PhysicalSize::new(self.stream_w, self.stream_h));
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        // The host's cursor is part of the video.
        window.set_cursor_visible(false);
        let ctx = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&ctx, window.clone()).expect("softbuffer surface");
        self.gfx = Some(Gfx { window, surface, _ctx: ctx });
        el.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::NewFrame => {
                let next = self.slot.latest.lock().unwrap().take();
                if let Some(p) = next {
                    if p.width as u32 != self.stream_w || p.height as u32 != self.stream_h {
                        self.stream_w = p.width as u32;
                        self.stream_h = p.height as u32;
                    }
                    if let Some(old) = self.current.replace(p) {
                        *self.slot.spare.lock().unwrap() = Some(old);
                    }
                    self.redraw();
                    self.update_title();
                }
            }
            UserEvent::SessionEnded(why) => {
                self.exit_reason = Some(why);
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.release_all();
                el.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) => {
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
            }
            WindowEvent::Focused(false) => {
                self.release_all();
                if self.captured {
                    self.set_capture(false);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else { return };
                let down = event.state == ElementState::Pressed;
                let combo = self.mods.control_key() && self.mods.alt_key() && self.mods.shift_key();
                if combo && down {
                    match code {
                        KeyCode::KeyQ => {
                            self.release_all();
                            el.exit();
                            return;
                        }
                        KeyCode::KeyZ => {
                            self.release_all();
                            self.set_capture(!self.captured);
                            return;
                        }
                        KeyCode::KeyF => {
                            if let Some(g) = &self.gfx {
                                let fs = g.window.fullscreen().is_some();
                                g.window.set_fullscreen(if fs { None } else { Some(Fullscreen::Borderless(None)) });
                            }
                            return;
                        }
                        KeyCode::KeyS => {
                            self.show_stats = !self.show_stats;
                            return;
                        }
                        _ => {}
                    }
                }
                let Some(vk) = keycode_to_vk(code) else { return };
                if down {
                    self.pressed.insert(vk);
                } else if !self.pressed.remove(&vk) {
                    return;
                }
                self.ctl.input(&In::Key { down, vk, modifiers: self.modifiers() });
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                if self.captured {
                    return;
                }
                let Some(g) = &self.gfx else { return };
                let size = g.window.inner_size();
                let (ox, oy, dw, dh) = self.video_rect(size.width, size.height);
                let x = ((position.x - ox as f64) * self.stream_w as f64 / dw as f64).clamp(0.0, self.stream_w as f64 - 1.0);
                let y = ((position.y - oy as f64) * self.stream_h as f64 / dh as f64).clamp(0.0, self.stream_h as f64 - 1.0);
                self.ctl.input(&In::MouseAbs { x: x as i16, y: y as i16, width: self.stream_w as i16, height: self.stream_h as i16 });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => input::BUTTON_LEFT,
                    MouseButton::Middle => input::BUTTON_MIDDLE,
                    MouseButton::Right => input::BUTTON_RIGHT,
                    MouseButton::Back => input::BUTTON_X1,
                    MouseButton::Forward => input::BUTTON_X2,
                    MouseButton::Other(_) => return,
                };
                self.ctl.input(&In::MouseButton { down: state == ElementState::Pressed, button: b });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => ((x * 120.0) as i32, (y * 120.0) as i32),
                    MouseScrollDelta::PixelDelta(p) => (p.x as i32, p.y as i32),
                };
                if y != 0 {
                    self.ctl.input(&In::Scroll { amount: y.clamp(-32768, 32767) as i16 });
                }
                if x != 0 {
                    self.ctl.input(&In::HScroll { amount: x.clamp(-32768, 32767) as i16 });
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let (true, DeviceEvent::MouseMotion { delta }) = (self.captured, event) {
            let (dx, dy) = (delta.0.round() as i32, delta.1.round() as i32);
            if dx != 0 || dy != 0 {
                self.ctl.input(&In::MouseRel { dx: dx.clamp(-32768, 32767) as i16, dy: dy.clamp(-32768, 32767) as i16 });
            }
        }
    }
}

/// Map a physical (US-layout position) key to a Windows virtual-key code.
pub fn keycode_to_vk(k: KeyCode) -> Option<u16> {
    use KeyCode::*;
    let letters = [KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ];
    if let Some(i) = letters.iter().position(|&c| c == k) {
        return Some(0x41 + i as u16);
    }
    let digits = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    if let Some(i) = digits.iter().position(|&c| c == k) {
        return Some(0x30 + i as u16);
    }
    let numpad = [Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9];
    if let Some(i) = numpad.iter().position(|&c| c == k) {
        return Some(0x60 + i as u16);
    }
    let fkeys = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19, F20, F21, F22, F23, F24];
    if let Some(i) = fkeys.iter().position(|&c| c == k) {
        return Some(0x70 + i as u16);
    }
    Some(match k {
        Backspace => 0x08,
        Tab => 0x09,
        Enter | NumpadEnter => 0x0D,
        Pause => 0x13,
        CapsLock => 0x14,
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
        PrintScreen => 0x2C,
        Insert => 0x2D,
        Delete => 0x2E,
        SuperLeft => 0x5B,
        SuperRight => 0x5C,
        ContextMenu => 0x5D,
        NumpadMultiply => 0x6A,
        NumpadAdd => 0x6B,
        NumpadSubtract => 0x6D,
        NumpadDecimal => 0x6E,
        NumpadDivide => 0x6F,
        NumLock => 0x90,
        ScrollLock => 0x91,
        ShiftLeft => 0xA0,
        ShiftRight => 0xA1,
        ControlLeft => 0xA2,
        ControlRight => 0xA3,
        AltLeft => 0xA4,
        AltRight => 0xA5,
        Semicolon => 0xBA,
        Equal => 0xBB,
        Comma => 0xBC,
        Minus => 0xBD,
        Period => 0xBE,
        Slash => 0xBF,
        Backquote => 0xC0,
        BracketLeft => 0xDB,
        Backslash => 0xDC,
        BracketRight => 0xDD,
        Quote => 0xDE,
        IntlBackslash => 0xE2,
        _ => return None,
    })
}
