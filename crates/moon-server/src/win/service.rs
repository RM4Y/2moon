//! Windows service (LocalSystem): keeps one host process, as SYSTEM, in the console session
//! (so it can see UAC and sign-in screens), restarted when the session changes or it stops.
//!   --install-service / --uninstall-service / --service (run by the service manager)
//! Extra host arguments: `<ProgramData>\2moon\host-args.txt`, one per line.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TOKEN_ACCESS_MASK, TOKEN_ALL_ACCESS};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "2moon";
const DISPLAY_NAME: &str = "2moon Host";
const DESCRIPTION: &str = "2moon game streaming host.";

/// Handle the service command-line flags; None when none is given.
pub fn from_args() -> Option<Result<()>> {
    let arg = std::env::args().nth(1)?;
    Some(match arg.as_str() {
        "--service" => service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("service dispatcher"),
        "--install-service" => install(),
        "--uninstall-service" => uninstall(),
        _ => return None,
    })
}

fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("2moon")
}

fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    // Replace an older registration (an update may move the program).
    if let Ok(old) = manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS) {
        let _ = old.stop();
        wait_stopped(&old);
        old.delete()?;
        drop(old);
        std::thread::sleep(Duration::from_millis(500));
    }
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["--service".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description(DESCRIPTION)?;
    // Restart the service if it ever crashes.
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) }; 3]),
    })?;
    service.start::<OsString>(&[])?;
    println!("service '{SERVICE_NAME}' installed and started");
    Ok(())
}

fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let Ok(service) = manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS) else {
        println!("service '{SERVICE_NAME}' is not installed");
        return Ok(());
    };
    let _ = service.stop();
    wait_stopped(&service);
    service.delete()?;
    println!("service '{SERVICE_NAME}' removed");
    Ok(())
}

fn wait_stopped(s: &windows_service::service::Service) {
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until {
        match s.query_status() {
            Ok(st) if st.current_state != ServiceState::Stopped => std::thread::sleep(Duration::from_millis(200)),
            _ => return,
        }
    }
}

define_windows_service!(ffi_service_main, service_main);

enum Signal {
    Stop,
    SessionChanged,
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        log(&format!("service error: {e:#}"));
    }
}

/// A line in the service's own log (<ProgramData>\2moon\logs\service.log).
fn log(msg: &str) {
    use std::io::Write;
    let dir = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("service.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{t}] {msg}");
    }
}

fn run_service() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = move |event| -> ServiceControlHandlerResult {
        match event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = tx.send(Signal::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(_) => {
                let _ = tx.send(Signal::SessionChanged);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let set = |state: ServiceState, accept: ServiceControlAccept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        });
    };
    set(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE);
    log("service started");

    let mut host: Option<(HANDLE, u32)> = None;
    let mut failures = 0u32;
    let mut started = Instant::now();
    loop {
        // Keep a host running in the console session.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if host.is_none() && session != u32::MAX {
            match start_host(session) {
                Ok(h) => {
                    log(&format!("host started in session {session}"));
                    host = Some((h, session));
                    started = Instant::now();
                }
                Err(e) => log(&format!("cannot start the host in session {session}: {e:#}")),
            }
        }
        let wait = Duration::from_millis(500);
        match rx.recv_timeout(wait) {
            Ok(Signal::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Signal::SessionChanged) => {
                // Sign-in, sign-out, user switch: follow the console session.
                std::thread::sleep(Duration::from_millis(500));
                if let Some((h, s)) = &host {
                    if *s != unsafe { WTSGetActiveConsoleSessionId() } {
                        log("console session changed: restarting the host");
                        stop_host(*h);
                        host = None;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some((h, _)) = &host {
            if unsafe { WaitForSingleObject(*h, 0) } == WAIT_OBJECT_0 {
                unsafe {
                    let _ = CloseHandle(*h);
                }
                host = None;
                // Count quick exits only: a host that ran a while starts again at once.
                failures = if started.elapsed() > Duration::from_secs(60) { 0 } else { failures + 1 };
                // Back off when it keeps stopping (bad arguments, missing DLL...).
                let pause = Duration::from_secs(2u64.saturating_pow(failures.min(5)));
                log(&format!("host stopped; starting it again in {}s", pause.as_secs()));
                if matches!(rx.recv_timeout(pause), Ok(Signal::Stop) | Err(mpsc::RecvTimeoutError::Disconnected)) {
                    break;
                }
                continue;
            }
        }
    }
    if let Some((h, _)) = host {
        stop_host(h);
    }
    log("service stopped");
    set(ServiceState::Stopped, ServiceControlAccept::empty());
    Ok(())
}

fn stop_host(h: HANDLE) {
    unsafe {
        let _ = TerminateProcess(h, 0);
        let _ = WaitForSingleObject(h, 5000);
        let _ = CloseHandle(h);
    }
}

/// Start the host in `session` as SYSTEM (the service's own token, moved to that session).
fn start_host(session: u32) -> Result<HANDLE> {
    let exe = std::env::current_exe()?;
    let dir = data_dir();
    let mut args = vec!["--video".to_string(), "dxgi".into(), "--audio".into(), "desktop".into(), "--log-file".into(), dir.join("logs").join("host.log").display().to_string()];
    if let Ok(extra) = std::fs::read_to_string(dir.join("host-args.txt")) {
        args.extend(extra.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from));
    }
    let quote = |s: &str| if s.contains(' ') || s.is_empty() { format!("\"{s}\"") } else { s.to_string() };
    let line = std::iter::once(quote(&exe.display().to_string())).chain(args.iter().map(|a| quote(a))).collect::<Vec<_>>().join(" ");
    let mut line: Vec<u16> = line.encode_utf16().chain([0]).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    let workdir: Vec<u16> = exe.parent().map(|p| p.display().to_string()).unwrap_or_default().encode_utf16().chain([0]).collect();
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).context("OpenProcessToken")?;
        let mut token = HANDLE::default();
        let r = DuplicateTokenEx(own, TOKEN_ACCESS_MASK(TOKEN_ALL_ACCESS.0), None, SecurityImpersonation, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        r.context("DuplicateTokenEx")?;
        let result = (|| -> Result<HANDLE> {
            SetTokenInformation(token, TokenSessionId, (&session as *const u32).cast(), 4).context("SetTokenInformation(TokenSessionId)")?;
            let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
            let mut pi = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(Some(token), PCWSTR::null(), Some(PWSTR(line.as_mut_ptr())), None, None, false, CREATE_NO_WINDOW, None, PCWSTR(workdir.as_ptr()), &si, &mut pi).context("CreateProcessAsUser")?;
            let _ = CloseHandle(pi.hThread);
            Ok(pi.hProcess)
        })();
        let _ = CloseHandle(token);
        result
    }
}
