//! Windows: `ChangeDisplaySettingsExW` on the captured display, modes the driver lists.
//! `CDS_FULLSCREEN`: Windows restores the mode if the host dies.

use anyhow::{bail, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_FULLSCREEN, CDS_TYPE, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
};

use super::{closest, same, Changed, Mode};

/// GDI name (\\.\DISPLAYn) of the display the capture picks for `want`.
fn device(want: &str) -> Result<String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(out) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = out.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
                if want.is_empty() || name.to_lowercase().contains(&want.to_lowercase()) {
                    return Ok(name);
                }
            }
        }
    }
    bail!("no display matching '{want}'")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn mode_of(dm: &DEVMODEW) -> Mode {
    Mode { w: dm.dmPelsWidth, h: dm.dmPelsHeight, hz: dm.dmDisplayFrequency as f64 }
}

fn new_devmode() -> DEVMODEW {
    DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() }
}

fn set(name: &[u16], m: Mode) -> Result<()> {
    let mut dm = new_devmode();
    dm.dmPelsWidth = m.w;
    dm.dmPelsHeight = m.h;
    dm.dmDisplayFrequency = m.hz.round() as u32;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_FULLSCREEN, None) };
    if r != DISP_CHANGE_SUCCESSFUL {
        bail!("ChangeDisplaySettingsEx({m}) failed ({})", r.0);
    }
    Ok(())
}

pub fn apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let output = device(want)?;
    let name = wide(&output);
    let mut dm = new_devmode();
    if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool() {
        bail!("cannot read the mode of {output}");
    }
    let from = mode_of(&dm);
    let mut offered: Vec<(Mode, ())> = Vec::new();
    for i in 0.. {
        let mut dm = new_devmode();
        if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm) }.as_bool() {
            break;
        }
        // Same colour depth as now (modes are listed once per depth).
        if dm.dmBitsPerPel == 32 {
            offered.push((mode_of(&dm), ()));
        }
    }
    let Some((to, _)) = closest(&offered, w, h, fps) else {
        bail!("{output} has no {w}x{h} mode (a virtual display driver can add it)");
    };
    if same(from, to) {
        return Ok(None);
    }
    set(&name, to)?;
    let restore: super::Restore = Box::new(move || {
        // No mode: back to the one saved in the registry.
        let r = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), None, None, CDS_TYPE(0), None) };
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("ChangeDisplaySettingsEx(restore) failed ({})", r.0);
        }
        Ok(())
    });
    Ok(Some(Changed { output, from, to, restore }))
}
