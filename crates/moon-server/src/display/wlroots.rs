//! wlroots compositors: Sway (`swaymsg`), Hyprland (`hyprctl`) and the others
//! (`wlr-randr`). All of them accept custom modes; headless outputs take any.

use anyhow::{Context, Result};
use serde_json::Value;

use super::{closest, run, same, Changed, Mode};

fn pick<'a>(list: &'a [Value], want: &str, enabled: impl Fn(&Value) -> bool) -> Result<&'a Value> {
    list.iter().filter(|o| enabled(o)).find(|o| want.is_empty() || o["name"].as_str().is_some_and(|n| n.contains(want))).with_context(|| format!("no enabled output matching '{want}'"))
}

/// The client's size at the closest listed refresh if it is within 1 Hz, else a custom mode.
fn target(listed: &[(Mode, ())], w: u32, h: u32, fps: u32) -> (Mode, bool) {
    match closest(listed, w, h, fps) {
        Some((m, _)) if (m.hz - fps as f64).abs() < 1.0 => (m, false),
        _ => (Mode { w, h, hz: fps as f64 }, true),
    }
}

pub fn sway(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let v: Value = serde_json::from_str(&run("swaymsg", &["-t", "get_outputs", "-r"])?).context("swaymsg: unexpected output")?;
    let list = v.as_array().cloned().unwrap_or_default();
    let o = pick(&list, want, |o| o["active"].as_bool() == Some(true))?;
    let name = o["name"].as_str().unwrap_or_default().to_string();
    let mode = |m: &Value| -> Option<Mode> { Some(Mode { w: m["width"].as_u64()? as u32, h: m["height"].as_u64()? as u32, hz: m["refresh"].as_f64()? / 1000.0 }) };
    let from = mode(&o["current_mode"]).context("no current mode")?;
    let listed: Vec<(Mode, ())> = o["modes"].as_array().map(|ms| ms.iter().filter_map(|m| Some((mode(m)?, ()))).collect()).unwrap_or_default();
    let (to, custom) = target(&listed, w, h, fps);
    if same(from, to) {
        return Ok(None);
    }
    let set = move |name: &str, m: Mode, custom: bool| -> Result<()> {
        let spec = format!("{}x{}@{:.3}Hz", m.w, m.h, m.hz);
        let mut args = vec!["output", name, "mode"];
        if custom {
            args.push("--custom");
        }
        args.push(&spec);
        run("swaymsg", &args).map(|_| ())
    };
    set(&name, to, custom)?;
    let output = name.clone();
    Ok(Some(Changed { output, from, to, restore: Box::new(move || set(&name, from, false)) }))
}

pub fn hyprland(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let v: Value = serde_json::from_str(&run("hyprctl", &["-j", "monitors"])?).context("hyprctl: unexpected output")?;
    let list = v.as_array().cloned().unwrap_or_default();
    let o = pick(&list, want, |o| o["disabled"].as_bool() != Some(true))?;
    let name = o["name"].as_str().unwrap_or_default().to_string();
    let from = Mode { w: o["width"].as_u64().unwrap_or(0) as u32, h: o["height"].as_u64().unwrap_or(0) as u32, hz: o["refreshRate"].as_f64().unwrap_or(60.0) };
    let to = Mode { w, h, hz: fps as f64 };
    if same(from, to) {
        return Ok(None);
    }
    // Keep the position and scale the monitor has.
    let (x, y, scale) = (o["x"].as_i64().unwrap_or(0), o["y"].as_i64().unwrap_or(0), o["scale"].as_f64().unwrap_or(1.0));
    let set = move |name: &str, m: Mode| -> Result<()> {
        let out = run("hyprctl", &["keyword", "monitor", &format!("{name},{}x{}@{:.3},{x}x{y},{scale}", m.w, m.h, m.hz)])?;
        anyhow::ensure!(out.trim() == "ok", "hyprctl: {}", out.trim());
        Ok(())
    };
    set(&name, to)?;
    let output = name.clone();
    Ok(Some(Changed { output, from, to, restore: Box::new(move || set(&name, from)) }))
}

pub fn wlr_randr(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let v: Value = serde_json::from_str(&run("wlr-randr", &["--json"])?).context("wlr-randr --json: unexpected output (wlr-randr 0.4 or later is needed)")?;
    let list = v.as_array().cloned().unwrap_or_default();
    let o = pick(&list, want, |o| o["enabled"].as_bool() == Some(true))?;
    let name = o["name"].as_str().unwrap_or_default().to_string();
    let ms = o["modes"].as_array().cloned().unwrap_or_default();
    let mode = |m: &Value| -> Option<Mode> { Some(Mode { w: m["width"].as_u64()? as u32, h: m["height"].as_u64()? as u32, hz: m["refresh"].as_f64()? }) };
    let from = ms.iter().find(|m| m["current"].as_bool() == Some(true)).and_then(mode).context("no current mode")?;
    let listed: Vec<(Mode, ())> = ms.iter().filter_map(|m| Some((mode(m)?, ()))).collect();
    let (to, custom) = target(&listed, w, h, fps);
    if same(from, to) {
        return Ok(None);
    }
    let set = move |name: &str, m: Mode, custom: bool| -> Result<()> {
        let spec = format!("{}x{}@{:.3}Hz", m.w, m.h, m.hz);
        run("wlr-randr", &["--output", name, if custom { "--custom-mode" } else { "--mode" }, &spec]).map(|_| ())
    };
    set(&name, to, custom)?;
    let output = name.clone();
    Ok(Some(Changed { output, from, to, restore: Box::new(move || set(&name, from, false)) }))
}
