//! Notification-area icon of the Windows host: the host has no window (the service
//! starts it in the background), so this is how it shows that it runs.
//!
//! - Tooltip: waiting / streaming WxH at N fps / pairing request.
//! - A balloon when a device asks to pair (the PIN is typed in the admin panel);
//!   clicking it opens the panel.
//! - Menu (left or right click): open the admin panel, restart the host (service)
//!   and quit. Under the service, quitting stops the service, so the host stays off
//!   until the next start of Windows (or of the service).
//!
//! The panel opens in the browser of the signed-in user, never as SYSTEM: a browser
//! running as SYSTEM would hand the whole machine to whoever uses it.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, LoadIconW, LoadImageW, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, TrackPopupMenu, TranslateMessage, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, SM_CXSMICON,
    SM_CYSMICON, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::state::Host;

const WM_TRAY: u32 = WM_APP + 1;
/// Legacy (version 0) notify-icon callback: the balloon was clicked.
const NIN_BALLOONUSERCLICK: u32 = 0x0405;
const ID_OPEN: usize = 1;
const ID_RESTART: usize = 2;
const ID_QUIT: usize = 3;
const TIMER_REFRESH: usize = 1;
const ICON_ID: u32 = 1;

struct Texts {
    waiting: &'static str,
    streaming: &'static str,
    pairing: &'static str,
    pairing_balloon: &'static str,
    open: &'static str,
    restart: &'static str,
    quit: &'static str,
}

const FR: Texts = Texts {
    waiting: "en attente",
    streaming: "streaming",
    pairing: "appairage en attente",
    pairing_balloon: "demande l'appairage. Cliquez ici pour saisir le code PIN dans l'administration.",
    open: "Ouvrir l'administration",
    restart: "Redémarrer l'hôte",
    quit: "Quitter",
};

const EN: Texts = Texts {
    waiting: "waiting",
    streaming: "streaming",
    pairing: "pairing request",
    pairing_balloon: "wants to pair. Click here to type the PIN in the admin panel.",
    open: "Open the admin panel",
    restart: "Restart the host",
    quit: "Quit",
};

struct Tray {
    host: Arc<Host>,
    admin_url: Option<String>,
    as_service: bool,
    texts: &'static Texts,
    taskbar_created: u32,
    hwnd: HWND,
    icon: HICON,
    shown: bool,
    tip: String,
    /// Pairing requests already announced by a balloon.
    announced: Vec<String>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

/// Start the icon on its own thread (it runs a Windows message loop).
pub fn spawn(host: Arc<Host>, admin_port: Option<u16>) {
    let admin_url = admin_port.map(|p| format!("https://localhost:{p}/"));
    let r = std::thread::Builder::new().name("tray".into()).spawn(move || {
        if let Err(e) = run(host, admin_url) {
            warn!("notification area icon: {e:#}");
        }
    });
    if let Err(e) = r {
        warn!("notification area icon: {e}");
    }
}

fn run(host: Arc<Host>, admin_url: Option<String>) -> anyhow::Result<()> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let class = w!("2moonHostTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance, lpszClassName: class, ..Default::default() };
        if RegisterClassW(&wc) == 0 {
            anyhow::bail!("RegisterClass failed");
        }
        // A hidden top-level window, not a message-only one: those miss the
        // "TaskbarCreated" broadcast sent when Explorer (re)starts.
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), class, w!("2moon Host"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, Some(instance), None)?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        // The host runs as SYSTEM: let Explorer (a normal user process) reach this window.
        for msg in [taskbar_created, WM_TRAY, WM_COMMAND] {
            let _ = ChangeWindowMessageFilterEx(hwnd, msg, MSGFLT_ALLOW, None);
        }
        // Resource 1 of the executable: the 2moon icon (build.rs), at the small icon size.
        let icon = LoadImageW(Some(instance), PCWSTR(1 as _), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))?;
        let french = GetUserDefaultUILanguage() & 0x3ff == 0x0c;
        let tray = Tray {
            host,
            admin_url,
            as_service: super::session::is_system(),
            texts: if french { &FR } else { &EN },
            taskbar_created,
            hwnd,
            icon,
            shown: false,
            tip: String::new(),
            announced: Vec::new(),
        };
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
        refresh();
        SetTimer(Some(hwnd), TIMER_REFRESH, Duration::from_secs(2).as_millis() as u32, None);
        info!("notification area icon ready");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Copy `s` into a fixed UTF-16 buffer, truncated, NUL-terminated.
fn fill(buf: &mut [u16], s: &str) {
    let max = buf.len() - 1;
    let mut n = 0;
    for c in s.encode_utf16().take(max) {
        buf[n] = c;
        n += 1;
    }
    buf[n] = 0;
}

fn base_data(t: &Tray) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: t.hwnd, uID: ICON_ID, ..Default::default() }
}

/// Add the icon if it is not shown (yet: Explorer may start after the host), update
/// the tooltip, and announce new pairing requests.
fn refresh() {
    TRAY.with(|cell| {
        // Busy: a message re-entered while the icon was being updated; next tick.
        let Ok(mut guard) = cell.try_borrow_mut() else { return };
        let Some(t) = guard.as_mut() else { return };
        let live = t.host.live.lock().unwrap().clone();
        let pending: Vec<String> = t.host.pending_requests.lock().unwrap().iter().map(|(name, _)| name.clone()).collect();
        let status = if live.active {
            format!("{} {}x{} · {:.0} fps", t.texts.streaming, live.width, live.height, live.fps)
        } else if !pending.is_empty() {
            t.texts.pairing.to_string()
        } else {
            t.texts.waiting.to_string()
        };
        let tip = format!("2moon Host · {}: {status}", t.host.hostname());

        let mut data = base_data(t);
        if !t.shown {
            data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            data.uCallbackMessage = WM_TRAY;
            data.hIcon = t.icon;
            fill(&mut data.szTip, &tip);
            t.shown = unsafe { Shell_NotifyIconW(NIM_ADD, &data).as_bool() };
            if t.shown {
                t.tip = tip;
            }
        } else if tip != t.tip {
            data.uFlags = NIF_TIP;
            fill(&mut data.szTip, &tip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
            }
            t.tip = tip;
        }

        t.announced.retain(|n| pending.contains(n));
        if let Some(name) = pending.iter().find(|n| !t.announced.contains(n)).cloned() {
            if t.shown {
                let mut data = base_data(t);
                data.uFlags = NIF_INFO;
                data.dwInfoFlags = NIIF_INFO;
                fill(&mut data.szInfoTitle, "2moon Host");
                fill(&mut data.szInfo, &format!("« {name} » {}", t.texts.pairing_balloon));
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
                }
            }
            t.announced.push(name);
        }
    });
}

fn open_admin() {
    let url = TRAY.with(|cell| cell.borrow().as_ref().and_then(|t| t.admin_url.clone()));
    if let Some(url) = url {
        if let Err(e) = super::session::open_for_user(&url) {
            warn!("cannot open the admin panel: {e:#}");
        }
    }
}

fn show_menu(hwnd: HWND) {
    let Some((tip, has_admin, as_service, texts)) = TRAY.with(|cell| cell.borrow().as_ref().map(|t| (t.tip.clone(), t.admin_url.is_some(), t.as_service, t.texts))) else { return };
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let status = wide(&tip);
        let open = wide(texts.open);
        let restart = wide(texts.restart);
        let quit = wide(texts.quit);
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if has_admin {
            let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, PCWSTR(open.as_ptr()));
            let _ = SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
        }
        if as_service {
            let _ = AppendMenuW(menu, MF_STRING, ID_RESTART, PCWSTR(restart.as_ptr()));
        }
        if has_admin || as_service {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, PCWSTR(quit.as_ptr()));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Needed for the menu to close when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// Quit for good: under the service, ask the service manager to stop the service
/// (otherwise it would start a new host); the service then ends this process.
fn quit_for_good() {
    let as_service = TRAY.with(|cell| cell.borrow().as_ref().is_some_and(|t| t.as_service));
    if as_service {
        use windows_service::service::ServiceAccess;
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        let stopped = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .and_then(|m| m.open_service(super::service::SERVICE_NAME, ServiceAccess::STOP))
            .and_then(|s| s.stop().map(|_| ()));
        match stopped {
            Ok(()) => info!("service stop requested from the notification area icon"),
            Err(e) => warn!("cannot stop the service: {e}"),
        }
    }
    end();
}

/// Remove the icon, then end the host. Under the service, the service starts a new
/// host a second later: this is the restart.
fn end() {
    TRAY.with(|cell| {
        if let Some(t) = cell.borrow().as_ref() {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &base_data(t));
            }
        }
    });
    info!("quit from the notification area icon");
    std::process::exit(0);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = TRAY.with(|cell| cell.try_borrow().ok().and_then(|t| t.as_ref().map(|t| t.taskbar_created))).unwrap_or(0);
    match msg {
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                NIN_BALLOONUSERCLICK => open_admin(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                ID_OPEN => open_admin(),
                ID_RESTART => end(),
                ID_QUIT => quit_for_good(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            refresh();
            LRESULT(0)
        }
        // Explorer restarted: its notification area is empty again.
        m if m != 0 && m == taskbar_created => {
            TRAY.with(|cell| {
                if let Some(t) = cell.try_borrow_mut().ok().as_deref_mut().and_then(Option::as_mut) {
                    t.shown = false;
                }
            });
            refresh();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
