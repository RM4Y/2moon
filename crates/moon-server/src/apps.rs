//! Applications offered to clients: list (persisted in `apps.json`), box art
//! (`covers/<id>.png`, served on `/appasset` like Sunshine) and the optional
//! command started on launch and stopped when the session is quit.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

use anyhow::{bail, Result};
use moon_proto::xml::App;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Box art larger than this is refused (Moonlight uses ~600×900 PNGs).
pub const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppEntry {
    pub id: u32,
    pub title: String,
    /// Shell command run when the app is launched (empty: just stream the desktop).
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub hdr: bool,
}

pub struct Apps {
    path: PathBuf,
    covers: PathBuf,
    list: Mutex<Vec<AppEntry>>,
    /// The running app command: (app id, process group leader).
    running: Mutex<Option<(u32, Child)>>,
}

impl Apps {
    pub fn load(state_dir: &Path, defaults: &[App]) -> Self {
        let path = state_dir.join("apps.json");
        let list = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<AppEntry>>(&s).ok())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| defaults.iter().map(|a| AppEntry { id: a.id, title: a.title.clone(), cmd: String::new(), hdr: a.hdr_supported }).collect());
        Self { path, covers: state_dir.join("covers"), list: Mutex::new(list), running: Mutex::new(None) }
    }

    pub fn list(&self) -> Vec<AppEntry> {
        self.list.lock().unwrap().clone()
    }

    pub fn get(&self, id: u32) -> Option<AppEntry> {
        self.list.lock().unwrap().iter().find(|a| a.id == id).cloned()
    }

    /// The list as GameStream sees it.
    pub fn xml(&self) -> String {
        let apps: Vec<App> = self.list().into_iter().map(|a| App { id: a.id, title: a.title, hdr_supported: a.hdr }).collect();
        moon_proto::xml::apps_xml(&apps)
    }

    fn save(&self, list: &[AppEntry]) -> Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Create (`id == 0`) or update an app; returns its id.
    pub fn upsert(&self, id: u32, title: &str, cmd: &str, hdr: bool) -> Result<u32> {
        let title = title.trim();
        if title.is_empty() {
            bail!("le nom est obligatoire");
        }
        if title.chars().count() > 80 {
            bail!("nom trop long (80 caractères maximum)");
        }
        let mut list = self.list.lock().unwrap();
        let id = if id == 0 {
            let id = list.iter().map(|a| a.id).max().unwrap_or(0) + 1;
            list.push(AppEntry { id, title: title.into(), cmd: cmd.trim().into(), hdr });
            id
        } else {
            let Some(a) = list.iter_mut().find(|a| a.id == id) else { bail!("application inconnue") };
            a.title = title.into();
            a.cmd = cmd.trim().into();
            a.hdr = hdr;
            id
        };
        self.save(&list)?;
        Ok(id)
    }

    pub fn remove(&self, id: u32) -> Result<()> {
        let mut list = self.list.lock().unwrap();
        if !list.iter().any(|a| a.id == id) {
            bail!("application inconnue");
        }
        if list.len() == 1 {
            bail!("il faut garder au moins une application");
        }
        list.retain(|a| a.id != id);
        self.save(&list)?;
        let _ = std::fs::remove_file(self.cover_path(id));
        Ok(())
    }

    /// Move an app one place up (`-1`) or down (`+1`) in the list.
    pub fn shift(&self, id: u32, dir: i32) -> Result<()> {
        let mut list = self.list.lock().unwrap();
        let Some(i) = list.iter().position(|a| a.id == id) else { bail!("application inconnue") };
        let j = i as i64 + dir.signum() as i64;
        if j >= 0 && (j as usize) < list.len() {
            list.swap(i, j as usize);
            self.save(&list)?;
        }
        Ok(())
    }

    fn cover_path(&self, id: u32) -> PathBuf {
        self.covers.join(format!("{id}.png"))
    }

    pub fn cover(&self, id: u32) -> Option<Vec<u8>> {
        std::fs::read(self.cover_path(id)).ok()
    }

    pub fn has_cover(&self, id: u32) -> bool {
        self.cover_path(id).is_file()
    }

    /// Store a PNG box art (`None` removes it).
    pub fn set_cover(&self, id: u32, png: Option<&[u8]>) -> Result<()> {
        if self.get(id).is_none() {
            bail!("application inconnue");
        }
        match png {
            None => {
                let _ = std::fs::remove_file(self.cover_path(id));
            }
            Some(b) => {
                if !b.starts_with(b"\x89PNG\r\n\x1a\n") {
                    bail!("la vignette doit être une image PNG");
                }
                if b.len() > MAX_COVER_BYTES {
                    bail!("image trop lourde (8 Mo maximum)");
                }
                std::fs::create_dir_all(&self.covers)?;
                let tmp = self.covers.join(format!("{id}.tmp"));
                std::fs::write(&tmp, b)?;
                std::fs::rename(&tmp, self.cover_path(id))?;
            }
        }
        Ok(())
    }

    /// Start the app's command (if any) unless it is already the one running.
    pub fn start(&self, id: u32) {
        let mut running = self.running.lock().unwrap();
        if let Some((rid, child)) = running.as_mut() {
            if *rid == id && matches!(child.try_wait(), Ok(None)) {
                return;
            }
        }
        if let Some((_, child)) = running.take() {
            stop_child(child);
        }
        let Some(app) = self.get(id) else { return };
        if app.cmd.is_empty() {
            return;
        }
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(&app.cmd).stdin(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group, so quitting stops everything the command spawned.
            c.process_group(0);
        }
        match c.spawn() {
            Ok(child) => {
                info!("started '{}' (pid {}): {}", app.title, child.id(), app.cmd);
                *running = Some((id, child));
            }
            Err(e) => warn!("could not start '{}': {e}", app.title),
        }
    }

    /// Stop the running app command, if any.
    pub fn stop(&self) {
        if let Some((_, child)) = self.running.lock().unwrap().take() {
            stop_child(child);
        }
    }
}

/// SIGTERM the process group, then SIGKILL it if still alive after 3 s (in the background).
fn stop_child(mut child: Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let pgid = child.id() as i32;
    info!("stopping app process group {pgid}");
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    std::thread::spawn(move || {
        for _ in 0..30 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let _ = child.wait();
    });
}
