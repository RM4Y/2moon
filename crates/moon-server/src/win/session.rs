//! Windows sessions and desktops: following the input desktop (UAC, lock screen) and
//! starting programs as the signed-in user when the host runs as SYSTEM (service).

use std::cell::RefCell;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread, WaitForSingleObject, CREATE_NEW_CONSOLE, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTUPINFOW,
};

thread_local! {
    /// Desktop this thread was last switched to.
    static DESKTOP: RefCell<Option<isize>> = const { RefCell::new(None) };
}

/// Switch the calling thread to the desktop that receives input (the normal one, a UAC
/// prompt or the lock screen); true if the thread is on it now. Only works for a thread
/// without windows, and for the secure desktops only as SYSTEM.
pub fn follow_input_desktop() -> bool {
    const GENERIC_ALL: u32 = 0x1000_0000;
    unsafe {
        let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) else { return false };
        if SetThreadDesktop(desk).is_err() {
            let _ = CloseDesktop(desk);
            return false;
        }
        DESKTOP.with(|d| {
            if let Some(old) = d.borrow_mut().replace(desk.0 as isize) {
                let _ = CloseDesktop(HDESK(old as *mut _));
            }
        });
        true
    }
}

/// Whether this process runs as LocalSystem (the service's host process).
pub fn is_system() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), buf.len() as u32, &mut len).is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return false;
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        // S-1-5-18: SubAuthority[0] == 18 with the NT authority.
        let sid = user.User.Sid.0 as *const u8;
        // SID layout: revision, sub-authority count, 6-byte authority, sub-authorities.
        let count = *sid.add(1);
        let auth = std::slice::from_raw_parts(sid.add(2), 6);
        let first = u32::from_le_bytes(std::slice::from_raw_parts(sid.add(8), 4).try_into().unwrap());
        count == 1 && auth == [0, 0, 0, 0, 0, 5] && first == 18
    }
}

/// Start `line` on the interactive desktop: as the user signed in on the console when
/// this process runs as SYSTEM, otherwise as ourselves.
fn start_as_user(line: &str, flags: PROCESS_CREATION_FLAGS) -> Result<PROCESS_INFORMATION> {
    let mut line: Vec<u16> = line.encode_utf16().chain([0]).collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        let started = if is_system() {
            let session = WTSGetActiveConsoleSessionId();
            let mut token = HANDLE::default();
            WTSQueryUserToken(session, &mut token).context("no user signed in on the console")?;
            let mut env = std::ptr::null_mut();
            let env_ok = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();
            let flags = if env_ok { flags | CREATE_UNICODE_ENVIRONMENT } else { flags };
            let r = CreateProcessAsUserW(Some(token), PCWSTR::null(), Some(PWSTR(line.as_mut_ptr())), None, None, false, flags, env_ok.then_some(env as *const _), PCWSTR::null(), &si, &mut pi);
            if env_ok {
                let _ = DestroyEnvironmentBlock(env);
            }
            let _ = CloseHandle(token);
            r
        } else {
            CreateProcessW(PCWSTR::null(), Some(PWSTR(line.as_mut_ptr())), None, None, false, flags, None, PCWSTR::null(), &si, &mut pi)
        };
        if let Err(e) = started {
            bail!("CreateProcess: {e}");
        }
    }
    Ok(pi)
}

/// Open a URL or a document with its default program, as the signed-in user (a
/// browser started as SYSTEM would give full control of the machine).
pub fn open_for_user(target: &str) -> Result<()> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let pi = start_as_user(&format!("\"{root}\\System32\\rundll32.exe\" url.dll,FileProtocolHandler {target}"), PROCESS_CREATION_FLAGS(0))?;
    unsafe {
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// A program started for an app: `cmd /C <command>` in a job object, so stopping
/// it stops everything it started.
pub struct UserProcess {
    process: HANDLE,
    job: HANDLE,
    pid: u32,
}

unsafe impl Send for UserProcess {}

impl UserProcess {
    /// Start `cmd /C <command>`: as the user signed in on the console when this process
    /// runs as SYSTEM, otherwise as ourselves.
    pub fn spawn(command: &str) -> Result<Self> {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null()).context("CreateJobObject")?;
            let pi = match start_as_user(&format!("\"{comspec}\" /C {command}"), CREATE_SUSPENDED | CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT) {
                Ok(pi) => pi,
                Err(e) => {
                    let _ = CloseHandle(job);
                    return Err(e);
                }
            };
            if let Err(e) = AssignProcessToJobObject(job, pi.hProcess) {
                warn!("app not in a job object ({e}): quitting may leave its programs running");
            }
            ResumeThread(pi.hThread);
            let _ = CloseHandle(pi.hThread);
            Ok(Self { process: pi.hProcess, job, pid: pi.dwProcessId })
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn is_running(&mut self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_TIMEOUT }
    }

    /// Stop the command and everything it started.
    pub fn stop(self) {
        info!("stopping app (pid {})", self.pid);
        unsafe {
            let _ = TerminateJobObject(self.job, 1);
        }
    }
}

impl Drop for UserProcess {
    fn drop(&mut self) {
        unsafe {
            let mut code = 0u32;
            let _ = GetExitCodeProcess(self.process, &mut code);
            let _ = CloseHandle(self.process);
            let _ = CloseHandle(self.job);
        }
    }
}
