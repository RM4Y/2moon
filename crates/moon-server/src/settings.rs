//! Host settings edited from the admin panel (`settings.json` in the state dir).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Keyboard layouts offered for typing client text: (id, XKB layout, XKB variant, label).
pub const KEYBOARDS: &[(&str, &str, &str, &str)] = &[
    ("fr", "fr", "", "AZERTY · Français (France)"),
    ("be", "be", "", "AZERTY · Français (Belgique)"),
    ("ch-fr", "ch", "fr", "QWERTZ · Français (Suisse)"),
    ("ca", "ca", "", "QWERTY · Français (Canada)"),
    ("fr-bepo", "fr", "bepo", "BÉPO · Français"),
    ("us", "us", "", "QWERTY · English (US)"),
    ("us-intl", "us", "intl", "QWERTY · English (US, international)"),
    ("gb", "gb", "", "QWERTY · English (UK)"),
    ("de", "de", "", "QWERTZ · Deutsch"),
    ("ch", "ch", "", "QWERTZ · Deutsch (Schweiz)"),
    ("es", "es", "", "QWERTY · Español"),
    ("it", "it", "", "QWERTY · Italiano"),
    ("pt", "pt", "", "QWERTY · Português"),
    ("us-dvorak", "us", "dvorak", "Dvorak · English (US)"),
];

pub const LANGUAGES: &[&str] = &["fr", "en"];
pub const PRESETS: &[&str] = &["p1", "p2", "p3", "p4", "p5", "p6", "p7"];
/// Choices for the automatic stop, in minutes (0 = never).
pub const AUTO_QUIT: &[u32] = &[0, 5, 15, 30, 60, 120];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Values {
    /// Admin panel language: "fr" or "en".
    pub language: String,
    /// Keyboard used to type client text: "auto" (detected on the host) or a `KEYBOARDS` id.
    pub keyboard: String,
    /// Name shown to clients; empty = the --name / system host name.
    pub host_name: String,
    /// NVENC preset; empty = the --nvenc-preset of the service.
    pub nvenc_preset: String,
    /// Quit the running app after this many minutes without a client (0 = never).
    pub auto_quit_min: u32,
    /// Advertise the host on the LAN (mDNS).
    pub mdns: bool,
}

impl Default for Values {
    fn default() -> Self {
        Self { language: "fr".into(), keyboard: "auto".into(), host_name: String::new(), nvenc_preset: String::new(), auto_quit_min: 0, mdns: true }
    }
}

pub struct Settings {
    path: PathBuf,
    values: Mutex<Values>,
    /// Bumped on every change (mDNS re-advertises on it).
    pub changed: tokio::sync::watch::Sender<u64>,
}

impl Settings {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("settings.json");
        let values = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Self { path, values: Mutex::new(values), changed: tokio::sync::watch::channel(0).0 }
    }

    pub fn get(&self) -> Values {
        self.values.lock().unwrap().clone()
    }

    /// Apply the fields present in `patch` (others unchanged).
    pub fn update(&self, patch: &serde_json::Value) -> Result<Values> {
        let mut v = self.values.lock().unwrap().clone();
        let str_of = |k: &str| patch.get(k).and_then(|x| x.as_str());
        if let Some(l) = str_of("language") {
            if !LANGUAGES.contains(&l) {
                bail!("langue inconnue");
            }
            v.language = l.into();
        }
        if let Some(k) = str_of("keyboard") {
            if k != "auto" && !KEYBOARDS.iter().any(|kb| kb.0 == k) {
                bail!("disposition de clavier inconnue");
            }
            v.keyboard = k.into();
        }
        if let Some(n) = str_of("host_name") {
            let n = n.trim();
            if n.chars().count() > 63 || n.chars().any(|c| c.is_control() || c == '.') {
                bail!("nom d'hôte invalide (63 caractères maximum, sans point)");
            }
            v.host_name = n.into();
        }
        if let Some(p) = str_of("nvenc_preset") {
            if !p.is_empty() && !PRESETS.contains(&p) {
                bail!("qualité d'encodage inconnue");
            }
            v.nvenc_preset = p.into();
        }
        if let Some(m) = patch.get("auto_quit_min") {
            let m = m.as_u64().filter(|m| AUTO_QUIT.contains(&(*m as u32))).ok_or_else(|| anyhow::anyhow!("délai d'arrêt automatique invalide"))?;
            v.auto_quit_min = m as u32;
        }
        if let Some(b) = patch.get("mdns") {
            v.mdns = b.as_bool().ok_or_else(|| anyhow::anyhow!("requête invalide"))?;
        }
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
        std::fs::rename(&tmp, &self.path)?;
        *self.values.lock().unwrap() = v.clone();
        self.changed.send_modify(|g| *g += 1);
        Ok(v)
    }

    /// XKB (layout, variant) to type client text with.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn keyboard_layout(&self) -> (String, String) {
        let id = self.values.lock().unwrap().keyboard.clone();
        match KEYBOARDS.iter().find(|kb| kb.0 == id) {
            Some((_, l, v, _)) => (l.to_string(), v.to_string()),
            None => crate::text::detect_layout(),
        }
    }
}
