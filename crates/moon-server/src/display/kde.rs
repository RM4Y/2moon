//! KDE Plasma (KWin): `kscreen-doctor`. A custom mode is added once per new
//! resolution (KWin keeps them), then selected. Virtual outputs (krfb-virtualmonitor)
//! accept any mode; real monitors may refuse modes they cannot display.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{run, same, Changed, Mode};

fn outputs() -> Result<Vec<Value>> {
    let v: Value = serde_json::from_str(&run("kscreen-doctor", &["-j"])?).context("kscreen-doctor -j: unexpected output")?;
    Ok(v["outputs"].as_array().cloned().unwrap_or_default())
}

fn modes(o: &Value) -> Vec<(Mode, String)> {
    o["modes"]
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let mode = Mode { w: m["size"]["width"].as_u64()? as u32, h: m["size"]["height"].as_u64()? as u32, hz: m["refreshRate"].as_f64()? };
                    Some((mode, m["id"].as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// An exact mode (refresh rounded to the Hz).
fn exact(o: &Value, w: u32, h: u32, fps: u32) -> Option<(Mode, String)> {
    modes(o).into_iter().find(|(m, _)| m.w == w && m.h == h && m.hz.round() as u32 == fps)
}

fn set(output: &str, id: &str) -> Result<()> {
    run("kscreen-doctor", &[&format!("output.{output}.mode.{id}")])?;
    // The capture must start once KWin has switched.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if outputs()?.iter().any(|o| o["name"].as_str() == Some(output) && o["currentModeId"].as_str() == Some(id)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub fn apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let os = outputs()?;
    let o = os
        .iter()
        .filter(|o| o["enabled"].as_bool() == Some(true))
        .find(|o| want.is_empty() || o["name"].as_str().is_some_and(|n| n.contains(want)))
        .with_context(|| format!("no enabled output matching '{want}'"))?;
    let name = o["name"].as_str().unwrap_or_default().to_string();
    let cur_id = o["currentModeId"].as_str().unwrap_or_default().to_string();
    let from = modes(o).into_iter().find(|(_, id)| *id == cur_id).map(|(m, _)| m).context("no current mode")?;
    let (to, id) = match exact(o, w, h, fps) {
        Some(m) => m,
        None => {
            run("kscreen-doctor", &[&format!("output.{name}.addCustomMode.{w}.{h}.{}.full", fps * 1000)])?;
            let os = outputs()?;
            let o = os.iter().find(|o| o["name"].as_str() == Some(&name)).context("output disappeared")?;
            exact(o, w, h, fps).with_context(|| format!("'{name}' does not accept {w}x{h}@{fps}"))?
        }
    };
    if id == cur_id || same(from, to) {
        return Ok(None);
    }
    set(&name, &id)?;
    let output = name.clone();
    Ok(Some(Changed { output, from, to, restore: Box::new(move || set(&name, &cur_id)) }))
}
