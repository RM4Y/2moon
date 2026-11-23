//! Typing Unicode text with a virtual keyboard: each character is looked up
//! in the host's XKB keymap (via runtime-loaded libxkbcommon) to find the key
//! and modifiers (Shift, AltGr) that produce it on the active layout.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};

use anyhow::{bail, Result};
use libloading::Library;
use tracing::info;

pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTALT: u16 = 56;

#[repr(C)]
struct RuleNames {
    rules: *const c_char,
    model: *const c_char,
    layout: *const c_char,
    variant: *const c_char,
    options: *const c_char,
}

/// Key (evdev code) plus the modifier keys to hold while pressing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stroke {
    pub key: u16,
    pub mods: Vec<u16>,
}

pub struct TextTyper {
    pub layout: String,
    table: HashMap<u32, Stroke>,
}

/// Find the host keyboard layout: KDE settings, then systemd-localed config, then "us".
pub fn detect_layout() -> (String, String) {
    let home = std::env::var("HOME").unwrap_or_default();
    if let Ok(s) = std::fs::read_to_string(format!("{home}/.config/kxkbrc")) {
        let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k).map(|v| v.split(',').next().unwrap_or("").trim().to_string()));
        if let Some(l) = get("LayoutList=").filter(|l| !l.is_empty()) {
            return (l, get("VariantList=").unwrap_or_default());
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/X11/xorg.conf.d/00-keyboard.conf") {
        let get = |k: &str| s.lines().find_map(|l| {
            let l = l.trim();
            l.strip_prefix("Option").and_then(|r| r.trim().strip_prefix(&format!("\"{k}\""))).map(|v| v.trim().trim_matches('"').split(',').next().unwrap_or("").to_string())
        });
        if let Some(l) = get("XkbLayout").filter(|l| !l.is_empty()) {
            return (l, get("XkbVariant").unwrap_or_default());
        }
    }
    ("us".into(), String::new())
}

impl TextTyper {
    pub fn new(layout: &str, variant: &str) -> Result<Self> {
        unsafe {
            let lib = Library::new("libxkbcommon.so.0")?;
            let ctx_new: unsafe extern "C" fn(c_int) -> *mut c_void = *lib.get(b"xkb_context_new\0")?;
            let ctx_unref: unsafe extern "C" fn(*mut c_void) = *lib.get(b"xkb_context_unref\0")?;
            let km_new: unsafe extern "C" fn(*mut c_void, *const RuleNames, c_int) -> *mut c_void = *lib.get(b"xkb_keymap_new_from_names\0")?;
            let km_unref: unsafe extern "C" fn(*mut c_void) = *lib.get(b"xkb_keymap_unref\0")?;
            let min_kc: unsafe extern "C" fn(*mut c_void) -> u32 = *lib.get(b"xkb_keymap_min_keycode\0")?;
            let max_kc: unsafe extern "C" fn(*mut c_void) -> u32 = *lib.get(b"xkb_keymap_max_keycode\0")?;
            let levels: unsafe extern "C" fn(*mut c_void, u32, u32) -> u32 = *lib.get(b"xkb_keymap_num_levels_for_key\0")?;
            let syms_by_level: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const u32) -> c_int = *lib.get(b"xkb_keymap_key_get_syms_by_level\0")?;
            let mods_for_level: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, usize) -> usize = *lib.get(b"xkb_keymap_key_get_mods_for_level\0")?;
            let mod_index: unsafe extern "C" fn(*mut c_void, *const c_char) -> u32 = *lib.get(b"xkb_keymap_mod_get_index\0")?;

            let ctx = ctx_new(0);
            if ctx.is_null() {
                bail!("xkb_context_new failed");
            }
            let (l, v) = (CString::new(layout)?, CString::new(variant)?);
            let names = RuleNames { rules: std::ptr::null(), model: std::ptr::null(), layout: l.as_ptr(), variant: if variant.is_empty() { std::ptr::null() } else { v.as_ptr() }, options: std::ptr::null() };
            let km = km_new(ctx, &names, 0);
            if km.is_null() {
                ctx_unref(ctx);
                bail!("unknown keyboard layout {layout}({variant})");
            }
            // Modifier bit -> key that produces it.
            let mut mod_keys: Vec<(u32, u16)> = Vec::new();
            for (name, key) in [("Shift", KEY_LEFTSHIFT), ("Mod5", KEY_RIGHTALT), ("Control", KEY_LEFTCTRL), ("Mod1", KEY_LEFTALT)] {
                let n = CString::new(name)?;
                let idx = mod_index(km, n.as_ptr());
                if idx != u32::MAX {
                    mod_keys.push((1 << idx, key));
                }
            }
            let lock = {
                let n = CString::new("Lock")?;
                let i = mod_index(km, n.as_ptr());
                if i == u32::MAX { 0 } else { 1 << i }
            };
            let mut table: HashMap<u32, Stroke> = HashMap::new();
            for kc in min_kc(km)..=max_kc(km) {
                if kc < 8 {
                    continue;
                }
                for level in 0..levels(km, kc, 0) {
                    let mut syms: *const u32 = std::ptr::null();
                    if syms_by_level(km, kc, 0, level, &mut syms) != 1 || syms.is_null() {
                        continue;
                    }
                    let sym = *syms;
                    let mut masks = [0u32; 16];
                    let n = mods_for_level(km, kc, 0, level, masks.as_mut_ptr(), masks.len());
                    // Simplest modifier combination that does not rely on Caps Lock.
                    let Some(mask) = masks[..n].iter().copied().filter(|m| m & lock == 0).min_by_key(|m| m.count_ones()) else { continue };
                    let mut mods = Vec::new();
                    let mut rest = mask;
                    for (bit, key) in &mod_keys {
                        if rest & bit != 0 {
                            mods.push(*key);
                            rest &= !bit;
                        }
                    }
                    if rest != 0 {
                        continue; // needs a modifier we cannot press
                    }
                    let stroke = Stroke { key: (kc - 8) as u16, mods };
                    let better = table.get(&sym).map(|s| stroke.mods.len() < s.mods.len()).unwrap_or(true);
                    if better {
                        table.insert(sym, stroke);
                    }
                }
            }
            km_unref(km);
            ctx_unref(ctx);
            info!("text input: {} characters reachable on layout {layout}{}", table.len(), if variant.is_empty() { String::new() } else { format!("({variant})") });
            Ok(Self { layout: layout.to_string(), table })
        }
    }

    /// Keysym for a Unicode code point (XKB convention).
    fn keysym(c: char) -> u32 {
        let cp = c as u32;
        match c {
            '\n' | '\r' => 0xff0d,
            '\t' => 0xff09,
            '\u{8}' => 0xff08,
            _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => cp,
            _ => 0x0100_0000 | cp,
        }
    }

    pub fn stroke(&self, c: char) -> Option<&Stroke> {
        let ks = Self::keysym(c);
        self.table.get(&ks).or_else(|| if ks & 0x0100_0000 == 0 { self.table.get(&(0x0100_0000 | c as u32)) } else { None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(key: u16, mods: &[u16]) -> Option<Stroke> {
        Some(Stroke { key, mods: mods.to_vec() })
    }

    #[test]
    fn azerty_and_qwerty() {
        let fr = TextTyper::new("fr", "azerty").or_else(|_| TextTyper::new("fr", "")).unwrap();
        assert_eq!(fr.stroke('a').cloned(), k(16, &[])); // A sits on the Q key position
        assert_eq!(fr.stroke('A').cloned(), k(16, &[KEY_LEFTSHIFT]));
        assert_eq!(fr.stroke('é').cloned(), k(3, &[])); // 2 key
        assert_eq!(fr.stroke('2').cloned(), k(3, &[KEY_LEFTSHIFT]));
        assert_eq!(fr.stroke('@').cloned(), k(11, &[KEY_RIGHTALT])); // AltGr+0
        assert_eq!(fr.stroke('\n').cloned(), k(28, &[]));
        let us = TextTyper::new("us", "").unwrap();
        assert_eq!(us.stroke('a').cloned(), k(30, &[]));
        assert_eq!(us.stroke('@').cloned(), k(3, &[KEY_LEFTSHIFT]));
        assert!(us.stroke('é').is_none());
    }
}
