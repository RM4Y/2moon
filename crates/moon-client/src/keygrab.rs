//! Capture of the system's keyboard shortcuts while the stream has focus, like
//! Moonlight: the Windows/Super key, Alt+Tab, Alt+F4, Alt+Esc… go to the host
//! instead of acting on this computer.
//!
//! - Wayland: the `keyboard-shortcuts-inhibit` protocol on the stream window
//!   (the compositor then hands those keys to the window, egui forwards them);
//! - X11: an active keyboard grab on the window;
//! - Windows: a low-level keyboard hook that sends the system combinations to
//!   the host itself and keeps them from Windows (as SDL does for Moonlight).
//!
//! Our own shortcuts (Ctrl+Alt+Shift+…) keep working in every case.

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
#[allow(unused_imports)]
use tracing::{debug, info, warn};

use crate::control::ControlSender;

pub struct KeyGrab {
    imp: Option<Imp>,
    on: bool,
}

impl KeyGrab {
    /// Set up for the window behind these handles (None if the platform has no way to do it).
    pub fn new(display: RawDisplayHandle, window: RawWindowHandle) -> Option<Self> {
        let imp = Imp::new(display, window)?;
        Some(Self { imp: Some(imp), on: false })
    }

    /// Capture (or release) the shortcuts; `ctl` receives the keys on Windows.
    pub fn set(&mut self, on: bool, ctl: Option<&ControlSender>) {
        if on == self.on {
            return;
        }
        self.on = on;
        if let Some(imp) = self.imp.as_mut() {
            imp.set(on, ctl);
        }
        info!("system shortcuts {}", if on { "captured" } else { "released" });
    }
}

impl KeyGrab {
    /// Whether the compositor granted the capture (Wayland only; None if unknown yet).
    pub fn granted(&mut self) -> Option<bool> {
        self.imp.as_mut()?.granted()
    }
}

impl Drop for KeyGrab {
    fn drop(&mut self) {
        // Release normally happens when the stream ends (`set(false)`). Here the
        // window and its Wayland / X11 connection may already be gone: leave
        // them alone, the compositor drops our capture when we disconnect. The
        // Windows hook is global to the process and must be removed.
        if cfg!(windows) {
            self.set(false, None);
        }
        std::mem::forget(self.imp.take());
    }
}

// ---------------------------------------------------------------- Linux

#[cfg(target_os = "linux")]
enum Imp {
    Wayland(wayland::Inhibit),
    X11(x11::Grab),
}

#[cfg(target_os = "linux")]
impl Imp {
    fn new(display: RawDisplayHandle, window: RawWindowHandle) -> Option<Self> {
        match (display, window) {
            (RawDisplayHandle::Wayland(d), RawWindowHandle::Wayland(w)) => match wayland::Inhibit::new(d.display.as_ptr(), w.surface.as_ptr()) {
                Ok(i) => Some(Self::Wayland(i)),
                Err(e) => {
                    warn!("cannot capture system shortcuts: {e:#}");
                    None
                }
            },
            (RawDisplayHandle::Xlib(d), RawWindowHandle::Xlib(w)) => match x11::Grab::new(d.display?.as_ptr(), w.window) {
                Ok(g) => Some(Self::X11(g)),
                Err(e) => {
                    warn!("cannot capture system shortcuts: {e:#}");
                    None
                }
            },
            _ => None,
        }
    }

    fn set(&mut self, on: bool, _ctl: Option<&ControlSender>) {
        match self {
            Self::Wayland(i) => i.set(on),
            Self::X11(g) => g.set(on),
        }
    }

    fn granted(&mut self) -> Option<bool> {
        match self {
            Self::Wayland(i) => i.granted(),
            Self::X11(_) => None,
        }
    }
}

#[cfg(target_os = "linux")]
mod wayland {
    use std::ffi::c_void;

    use anyhow::{Context, Result};
    use wayland_backend::client::{Backend, ObjectId};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{wl_registry, wl_seat::WlSeat, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager;
    use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1 as Inhibitor};

    #[derive(Default)]
    pub struct State {
        active: Option<bool>,
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<WlSeat, ()> for State {
        fn event(_: &mut Self, _: &WlSeat, _: wayland_client::protocol::wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<Manager, ()> for State {
        fn event(_: &mut Self, _: &Manager, _: <Manager as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    }
    impl Dispatch<Inhibitor, ()> for State {
        fn event(st: &mut Self, _: &Inhibitor, ev: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
            match ev {
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => st.active = Some(true),
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => st.active = Some(false),
                _ => {}
            }
        }
    }

    /// Our own objects on the window's Wayland connection.
    pub struct Inhibit {
        conn: Connection,
        queue: EventQueue<State>,
        state: State,
        manager: Manager,
        seat: WlSeat,
        surface: WlSurface,
        inhibitor: Option<Inhibitor>,
    }

    impl Inhibit {
        pub fn new(display: *mut c_void, surface: *mut c_void) -> Result<Self> {
            // Safety: the display and surface belong to the window, which outlives us.
            let backend = unsafe { Backend::from_foreign_display(display.cast()) };
            let conn = Connection::from_backend(backend);
            let (globals, queue) = registry_queue_init::<State>(&conn).context("Wayland registry")?;
            let qh = queue.handle();
            let manager: Manager = globals.bind(&qh, 1..=1, ()).context("the compositor does not support keyboard-shortcuts-inhibit")?;
            let seat: WlSeat = globals.bind(&qh, 1..=9, ()).context("no Wayland seat")?;
            let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }.context("window surface")?;
            let surface = WlSurface::from_id(&conn, id).context("window surface")?;
            Ok(Self { conn, queue, state: State::default(), manager, seat, surface, inhibitor: None })
        }

        pub fn set(&mut self, on: bool) {
            if on && self.inhibitor.is_none() {
                self.inhibitor = Some(self.manager.inhibit_shortcuts(&self.surface, &self.seat, &self.queue.handle(), ()));
            } else if !on {
                if let Some(i) = self.inhibitor.take() {
                    i.destroy();
                }
            }
            let _ = self.conn.flush();
            let _ = self.queue.dispatch_pending(&mut self.state);
        }

        /// Latest answer of the compositor (active / inactive).
        pub fn granted(&mut self) -> Option<bool> {
            let _ = self.queue.roundtrip(&mut self.state);
            self.state.active
        }
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use std::ffi::{c_int, c_ulong, c_void};

    use anyhow::{Context, Result};
    use libloading::Library;

    type GrabKeyboard = unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_int, c_int, c_ulong) -> c_int;
    type UngrabKeyboard = unsafe extern "C" fn(*mut c_void, c_ulong) -> c_int;
    type Flush = unsafe extern "C" fn(*mut c_void) -> c_int;

    pub struct Grab {
        _lib: Library,
        grab: GrabKeyboard,
        ungrab: UngrabKeyboard,
        flush: Flush,
        display: *mut c_void,
        window: c_ulong,
    }

    impl Grab {
        pub fn new(display: *mut c_void, window: c_ulong) -> Result<Self> {
            unsafe {
                let lib = Library::new("libX11.so.6").context("libX11")?;
                let grab: GrabKeyboard = *lib.get(b"XGrabKeyboard\0")?;
                let ungrab: UngrabKeyboard = *lib.get(b"XUngrabKeyboard\0")?;
                let flush: Flush = *lib.get(b"XFlush\0")?;
                Ok(Self { _lib: lib, grab, ungrab, flush, display, window })
            }
        }

        pub fn set(&mut self, on: bool) {
            const GRAB_MODE_ASYNC: c_int = 1;
            const CURRENT_TIME: c_ulong = 0;
            unsafe {
                if on {
                    // owner_events: our window keeps receiving its keys normally.
                    let r = (self.grab)(self.display, self.window, 1, GRAB_MODE_ASYNC, GRAB_MODE_ASYNC, CURRENT_TIME);
                    if r != 0 {
                        tracing::warn!("X11 keyboard grab refused ({r})");
                    }
                } else {
                    (self.ungrab)(self.display, CURRENT_TIME);
                }
                (self.flush)(self.display);
            }
        }
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
struct Imp;

#[cfg(windows)]
impl Imp {
    fn new(_: RawDisplayHandle, _: RawWindowHandle) -> Option<Self> {
        Some(Self)
    }

    fn set(&mut self, on: bool, ctl: Option<&ControlSender>) {
        win::set(on, ctl)
    }

    fn granted(&mut self) -> Option<bool> {
        None
    }
}

#[cfg(windows)]
mod win {
    //! The hook callback is called by Windows from inside the kernel (a user-mode
    //! callback); an exception there can be swallowed: the callback just stops, the key
    //! goes on to Windows (Start menu…) and the hooks installed before ours are skipped,
    //! with no crash and no log. Seen with 0.5.4 on Windows 11: the callback (mutexes, a
    //! HashSet with random hash keys, allocation, logging) never got to its end, not even
    //! for keys it lets through, while a minimal hook in another process worked. It now
    //! only uses atomics, GetAsyncKeyState and PostThreadMessageW; sending to the host
    //! and logging happen in the message loop, outside the callback. A vectored
    //! exception handler reports any exception on the hook thread.

    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use moon_proto::control::input::{self as inp, Event as In};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_USER,
    };

    use crate::control::ControlSender;

    static ON: AtomicBool = AtomicBool::new(false);
    /// Thread running the hook (0: none).
    static THREAD: AtomicU32 = AtomicU32::new(0);
    /// Only used outside the hook callback.
    static SENDER: Mutex<Option<ControlSender>> = Mutex::new(None);
    /// Keys (by virtual-key code) whose press we kept from Windows: their release is kept too.
    static HELD: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

    /// Keys from the callback to the message loop of the same thread, packed as
    /// vk | modifiers << 8 | down << 16 | for the host << 17 | injected << 18.
    const QLEN: usize = 64;
    static QUEUE: [AtomicU32; QLEN] = [const { AtomicU32::new(0) }; QLEN];
    /// Next slot written by the callback / read by the message loop.
    static Q_HEAD: AtomicUsize = AtomicUsize::new(0);
    static Q_TAIL: AtomicUsize = AtomicUsize::new(0);
    /// Posted by the callback: keys are waiting in the queue.
    const WM_HOOK_KEYS: u32 = WM_APP + 0x31;

    /// Diagnostics: callbacks entered / ending with CallNextHookEx / keeping the key.
    static ENTERED: AtomicU64 = AtomicU64::new(0);
    static PASSED: AtomicU64 = AtomicU64::new(0);
    static KEPT: AtomicU64 = AtomicU64::new(0);
    /// Inside the callback right now (for the exception report).
    static IN_HOOK: AtomicBool = AtomicBool::new(false);
    /// Id of the hook thread while it runs (for the exception handler).
    static HOOK_TID: AtomicU32 = AtomicU32::new(0);
    static EXC_COUNT: AtomicU32 = AtomicU32::new(0);
    static EXC_CODE: AtomicU32 = AtomicU32::new(0);
    static EXC_ADDR: AtomicUsize = AtomicUsize::new(0);
    static EXC_IN_HOOK: AtomicBool = AtomicBool::new(false);

    const VK_TAB: u32 = 0x09;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_SPACE: u32 = 0x20;
    const VK_LWIN: u32 = 0x5B;
    const VK_RWIN: u32 = 0x5C;
    const VK_APPS: u32 = 0x5D;
    const VK_F4: u32 = 0x73;
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;

    pub fn set(on: bool, ctl: Option<&ControlSender>) {
        if let Some(c) = ctl {
            *SENDER.lock().unwrap() = Some(c.clone());
        }
        ON.store(on, Ordering::SeqCst);
        if on && THREAD.load(Ordering::SeqCst) == 0 {
            // The hook runs on a thread of its own that does nothing else. On the UI
            // thread (busy drawing, a lot with software OpenGL) it would answer late:
            // Windows then hands the key to the system (Start menu, Alt+Tab here) and,
            // after a few timeouts, silently removes the hook.
            let (tx, rx) = std::sync::mpsc::channel();
            match std::thread::Builder::new().name("moon-keyhook".into()).spawn(move || hook_thread(tx)) {
                Ok(_) => THREAD.store(rx.recv().unwrap_or(0), Ordering::SeqCst),
                Err(e) => tracing::warn!("cannot start the keyboard hook thread: {e}"),
            }
        } else if !on {
            // The hook thread releases on the host the keys it kept, then exits.
            let t = THREAD.swap(0, Ordering::SeqCst);
            if t != 0 {
                unsafe { PostThreadMessageW(t, WM_QUIT, 0, 0) };
            }
        }
    }

    /// Installs the hook, sends this thread's id (0 on failure) and runs the message
    /// loop (it is what calls the hook) until WM_QUIT.
    fn hook_thread(tx: std::sync::mpsc::Sender<u32>) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            // Create this thread's message queue before anyone posts to it.
            PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let tid = GetCurrentThreadId();
            HOOK_TID.store(tid, Ordering::SeqCst);
            install_exception_probe();
            Q_TAIL.store(Q_HEAD.load(Ordering::SeqCst), Ordering::SeqCst);
            let start = (ENTERED.load(Ordering::SeqCst), PASSED.load(Ordering::SeqCst), KEPT.load(Ordering::SeqCst));
            let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), GetModuleHandleW(std::ptr::null()), 0);
            if h.is_null() {
                HOOK_TID.store(0, Ordering::SeqCst);
                tracing::warn!("cannot install the keyboard hook");
                let _ = tx.send(0);
                return;
            }
            tracing::info!("keyboard hook installed (thread {tid})");
            let _ = tx.send(tid);
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_HOOK_KEYS {
                    drain();
                    continue;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            UnhookWindowsHookEx(h);
            HOOK_TID.store(0, Ordering::SeqCst);
            drain();
            release_held();
            let seen = ENTERED.load(Ordering::SeqCst).saturating_sub(start.0);
            let passed = PASSED.load(Ordering::SeqCst).saturating_sub(start.1);
            let kept = KEPT.load(Ordering::SeqCst).saturating_sub(start.2);
            let unfinished = seen.saturating_sub(passed + kept);
            tracing::info!("keyboard hook removed (thread {tid}): {seen} keys seen, {passed} left to Windows, {kept} kept for the host, {unfinished} unfinished");
            report_exceptions();
        }
    }

    /// Keep it minimal: atomics, GetAsyncKeyState, PostThreadMessageW. No lock, no
    /// allocation, no thread-local, no logging (see the module comment).
    unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        IN_HOOK.store(true, Ordering::Relaxed);
        ENTERED.fetch_add(1, Ordering::Relaxed);
        if code == 0 && ON.load(Ordering::Relaxed) {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode & 0xff;
            let msg = wparam as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let alt = kb.flags & LLKHF_ALTDOWN != 0;
            let ctrl = GetAsyncKeyState(VK_CONTROL) < 0;
            let shift = GetAsyncKeyState(VK_SHIFT) < 0;
            let system = matches!(vk, VK_LWIN | VK_RWIN | VK_APPS) || (alt && matches!(vk, VK_TAB | VK_ESCAPE | VK_F4 | VK_SPACE)) || (ctrl && vk == VK_ESCAPE);
            let held = &HELD[vk as usize];
            let keep = (down && system) || (!down && held.load(Ordering::Relaxed));
            // Logged by the message loop: what goes to the host, plus the Windows keys and
            // Ctrl+Alt+Shift+… (diagnostics). Plain typing is not.
            if keep || matches!(vk, VK_LWIN | VK_RWIN) || (down && ctrl && alt && shift) {
                let modifiers = (shift as u8) * inp::MOD_SHIFT | (ctrl as u8) * inp::MOD_CTRL | (alt as u8) * inp::MOD_ALT;
                let ev = vk | (modifiers as u32) << 8 | (down as u32) << 16 | (keep as u32) << 17 | ((kb.flags & 0x10 != 0) as u32) << 18;
                if push(ev) {
                    PostThreadMessageW(GetCurrentThreadId(), WM_HOOK_KEYS, 0, 0);
                }
            }
            if keep {
                held.store(down, Ordering::Relaxed);
                KEPT.fetch_add(1, Ordering::Relaxed);
                IN_HOOK.store(false, Ordering::Relaxed);
                return 1; // keep it from Windows
            }
        }
        PASSED.fetch_add(1, Ordering::Relaxed);
        IN_HOOK.store(false, Ordering::Relaxed);
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// Callback side of the queue (false: full, the key is not sent / logged).
    fn push(ev: u32) -> bool {
        let h = Q_HEAD.load(Ordering::Relaxed);
        if h.wrapping_sub(Q_TAIL.load(Ordering::Acquire)) >= QLEN {
            return false;
        }
        QUEUE[h % QLEN].store(ev, Ordering::Relaxed);
        Q_HEAD.store(h.wrapping_add(1), Ordering::Release);
        true
    }

    /// Message loop side: send the kept keys to the host and log.
    fn drain() {
        let sender = SENDER.lock().unwrap().clone();
        loop {
            let t = Q_TAIL.load(Ordering::Relaxed);
            if t == Q_HEAD.load(Ordering::Acquire) {
                break;
            }
            let ev = QUEUE[t % QLEN].load(Ordering::Relaxed);
            Q_TAIL.store(t.wrapping_add(1), Ordering::Release);
            let (vk, modifiers, down, keep, injected) = ((ev & 0xff) as u16, (ev >> 8) as u8, ev >> 16 & 1 != 0, ev >> 17 & 1 != 0, ev >> 18 & 1 != 0);
            tracing::info!("hook: vk {vk:#04x} {} modifiers {modifiers:#04x} injected {injected} -> {}", if down { "down" } else { "up" }, if keep { "host" } else { "Windows" });
            if keep {
                if let Some(c) = &sender {
                    c.input(&In::Key { down, vk, modifiers });
                }
            }
        }
    }

    /// Release on the host the keys we were holding for it.
    fn release_held() {
        let sender = SENDER.lock().unwrap().clone();
        for (vk, held) in HELD.iter().enumerate() {
            if held.swap(false, Ordering::SeqCst) {
                if let Some(c) = &sender {
                    c.input(&In::Key { down: false, vk: vk as u16, modifiers: 0 });
                }
            }
        }
    }

    // ---- exception probe (diagnostics)

    #[repr(C)]
    #[allow(dead_code)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *const ExceptionRecord,
        address: *const c_void,
    }

    #[repr(C)]
    #[allow(dead_code)]
    struct ExceptionPointers {
        record: *const ExceptionRecord,
        context: *const c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: unsafe extern "system" fn(*const ExceptionPointers) -> i32) -> *mut c_void;
    }

    /// Notes exceptions raised on the hook thread (they would be swallowed silently)
    /// and lets them go on as before.
    unsafe extern "system" fn on_exception(info: *const ExceptionPointers) -> i32 {
        let tid = HOOK_TID.load(Ordering::Relaxed);
        if tid != 0 && GetCurrentThreadId() == tid && !info.is_null() && !(*info).record.is_null() {
            let r = &*(*info).record;
            EXC_COUNT.fetch_add(1, Ordering::Relaxed);
            EXC_CODE.store(r.code, Ordering::Relaxed);
            EXC_ADDR.store(r.address as usize, Ordering::Relaxed);
            EXC_IN_HOOK.store(IN_HOOK.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }

    fn install_exception_probe() {
        static DONE: AtomicBool = AtomicBool::new(false);
        if !DONE.swap(true, Ordering::SeqCst) {
            unsafe { AddVectoredExceptionHandler(1, on_exception) };
        }
    }

    fn report_exceptions() {
        let n = EXC_COUNT.swap(0, Ordering::SeqCst);
        if n == 0 {
            return;
        }
        let base = unsafe { GetModuleHandleW(std::ptr::null()) } as usize;
        let addr = EXC_ADDR.load(Ordering::SeqCst);
        tracing::warn!(
            "keyboard hook thread: {n} exception(s), last code {:#010x} at {addr:#x} (moon-client.exe+{:#x}){}",
            EXC_CODE.load(Ordering::SeqCst),
            addr.wrapping_sub(base),
            if EXC_IN_HOOK.load(Ordering::SeqCst) { ", inside the hook callback" } else { "" }
        );
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
struct Imp;

#[cfg(not(any(target_os = "linux", windows)))]
impl Imp {
    fn new(_: RawDisplayHandle, _: RawWindowHandle) -> Option<Self> {
        None
    }
    fn set(&mut self, _: bool, _: Option<&ControlSender>) {}
    fn granted(&mut self) -> Option<bool> {
        None
    }
}
