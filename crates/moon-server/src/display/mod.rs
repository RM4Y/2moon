//! Native resolution: during a session the display takes the client's mode, then gets its own back.
//! Backends: KDE (`kscreen-doctor`), GNOME (Mutter D-Bus), Sway / Hyprland / wlroots,
//! X11 (`xrandr`), Windows (`ChangeDisplaySettingsExW`).
//! Without a matching mode nothing changes and the picture is scaled.

use std::sync::Mutex;

use anyhow::Result;
use tracing::{info, warn};

#[cfg(target_os = "linux")]
mod gnome;
#[cfg(target_os = "linux")]
mod kde;
#[cfg(target_os = "linux")]
mod wlroots;
#[cfg(windows)]
mod win32;
#[cfg(target_os = "linux")]
mod x11;

/// A display mode: size and refresh rate (Hz).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    pub w: u32,
    pub h: u32,
    pub hz: f64,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}@{}", self.w, self.h, (self.hz * 100.0).round() / 100.0)
    }
}

/// Puts the display back the way it was.
pub type Restore = Box<dyn FnOnce() -> Result<()> + Send>;

/// What a backend did: the display it changed and how to undo it.
pub struct Changed {
    pub output: String,
    pub from: Mode,
    pub to: Mode,
    pub restore: Restore,
}

/// Among `modes`, the one of the client's size whose refresh rate is closest to `fps`.
pub fn closest<T: Clone>(modes: &[(Mode, T)], w: u32, h: u32, fps: u32) -> Option<(Mode, T)> {
    modes.iter().filter(|(m, _)| m.w == w && m.h == h).min_by(|a, b| (a.0.hz - fps as f64).abs().total_cmp(&(b.0.hz - fps as f64).abs())).cloned()
}

/// Same mode, give or take the rounding of refresh rates.
pub fn same(a: Mode, b: Mode) -> bool {
    a.w == b.w && a.h == b.h && (a.hz - b.hz).abs() < 0.5
}

struct Saved {
    output: String,
    from: Mode,
    restore: Restore,
}

static SAVED: Mutex<Option<Saved>> = Mutex::new(None);

#[cfg(target_os = "linux")]
fn backend_apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let desktop = env("XDG_CURRENT_DESKTOP").to_uppercase();
    let wayland = !env("WAYLAND_DISPLAY").is_empty() || env("XDG_SESSION_TYPE") == "wayland";
    if !wayland && !env("DISPLAY").is_empty() {
        return x11::apply(want, w, h, fps);
    }
    if !env("HYPRLAND_INSTANCE_SIGNATURE").is_empty() {
        return wlroots::hyprland(want, w, h, fps);
    }
    if !env("SWAYSOCK").is_empty() {
        return wlroots::sway(want, w, h, fps);
    }
    if desktop.contains("KDE") {
        return kde::apply(want, w, h, fps);
    }
    if desktop.contains("GNOME") || desktop.contains("UBUNTU") {
        return gnome::apply(want, w, h, fps);
    }
    // Unknown desktop: try what is installed.
    match kde::apply(want, w, h, fps) {
        Ok(r) => Ok(r),
        Err(_) => wlroots::wlr_randr(want, w, h, fps),
    }
}

#[cfg(windows)]
fn backend_apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    win32::apply(want, w, h, fps)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn backend_apply(_: &str, _: u32, _: u32, _: u32) -> Result<Option<Changed>> {
    anyhow::bail!("not supported on this system")
}

/// Switch the captured display (`--output`, empty = the one captured by default)
/// to `w`×`h`@`fps`. The first change of a session remembers the mode to restore.
pub fn apply(want: &str, w: u32, h: u32, fps: u32) {
    let fps = fps.clamp(24, 360);
    match backend_apply(want, w, h, fps) {
        Ok(Some(c)) => {
            info!("native resolution: '{}' {} -> {}", c.output, c.from, c.to);
            let mut saved = SAVED.lock().unwrap();
            match saved.as_ref() {
                // Already changed earlier in this session: keep the original mode to restore.
                Some(s) if s.output == c.output => {}
                _ => *saved = Some(Saved { output: c.output, from: c.from, restore: c.restore }),
            }
        }
        Ok(None) => {}
        Err(e) => warn!("native resolution: {e:#}"),
    }
}

/// Give the display its mode from before the session (no-op if nothing changed).
pub fn restore() {
    let Some(s) = SAVED.lock().unwrap().take() else { return };
    match (s.restore)() {
        Ok(()) => info!("native resolution: '{}' back to {}", s.output, s.from),
        Err(e) => warn!("native resolution: restoring '{}' to {}: {e:#}", s.output, s.from),
    }
}

/// Run a command line tool, failing with its error output.
#[cfg(target_os = "linux")]
pub(crate) fn run(cmd: &str, args: &[&str]) -> Result<String> {
    use anyhow::Context;
    let out = std::process::Command::new(cmd).args(args).output().with_context(|| format!("{cmd} is not available"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).into_owned() } else { err.into_owned() };
        anyhow::bail!("{cmd} {}: {}", args.join(" "), msg.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
