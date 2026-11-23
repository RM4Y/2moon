//! X11: `xrandr`. A missing resolution is created from a `cvt` modeline and
//! added to the output (accepted by virtual / dummy outputs and most monitors
//! within their limits).

use anyhow::{Context, Result};

use super::{closest, run, same, Changed, Mode};

struct Output {
    name: String,
    /// (mode, xrandr mode name)
    modes: Vec<(Mode, String)>,
    current: Option<Mode>,
}

/// `xrandr --query`: "NAME connected [primary] WxH+X+Y ..." then indented
/// mode lines "   1920x1080     60.00*+  50.00".
fn query() -> Result<Vec<(Output, bool)>> {
    let text = run("xrandr", &["--query"])?;
    let mut list: Vec<(Output, bool)> = Vec::new();
    for line in text.lines() {
        if !line.starts_with(' ') {
            let mut f = line.split_whitespace();
            let (Some(name), Some(state)) = (f.next(), f.next()) else { continue };
            if state != "connected" {
                list.push((Output { name: name.into(), modes: vec![], current: None }, false));
                continue;
            }
            // Active when it has a geometry (WxH+X+Y).
            let active = line.split_whitespace().any(|t| t.contains('+') && t.contains('x') && t.chars().next().is_some_and(|c| c.is_ascii_digit()));
            list.push((Output { name: name.into(), modes: vec![], current: None }, active));
            continue;
        }
        let Some((o, _)) = list.last_mut() else { continue };
        let mut f = line.split_whitespace();
        let Some(mname) = f.next() else { continue };
        let size = mname.split('_').next().unwrap_or(mname);
        let Some((w, h)) = size.split_once('x') else { continue };
        let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.trim_end_matches('i').parse::<u32>()) else { continue };
        for rate in f {
            let current = rate.contains('*');
            let Ok(hz) = rate.trim_end_matches(['*', '+']).parse::<f64>() else { continue };
            let m = Mode { w, h, hz };
            o.modes.push((m, mname.to_string()));
            if current {
                o.current = Some(m);
            }
        }
    }
    Ok(list)
}

fn set(output: &str, name: &str, hz: f64) -> Result<()> {
    run("xrandr", &["--output", output, "--mode", name, "--rate", &format!("{hz:.2}")]).map(|_| ())
}

pub fn apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    let list = query()?;
    let (o, _) = list.iter().find(|(o, active)| *active && (want.is_empty() || o.name.contains(want))).with_context(|| format!("no active output matching '{want}'"))?;
    let from = o.current.context("no current mode")?;
    let from_name = o.modes.iter().find(|(m, _)| *m == from).map(|(_, n)| n.clone()).context("no current mode")?;
    let (to, to_name) = match closest(&o.modes, w, h, fps) {
        Some((m, n)) if (m.hz - fps as f64).abs() < 1.0 => (m, n),
        _ => {
            // "Modeline "1920x1080_60.00"  173.00  1920 2048 2248 2576  1080 1083 1088 1120 -hsync +vsync"
            let cvt = run("cvt", &[&w.to_string(), &h.to_string(), &fps.to_string()])?;
            let line = cvt.lines().find_map(|l| l.trim().strip_prefix("Modeline ")).context("cvt: no modeline")?;
            let mut f = line.split_whitespace();
            let name = f.next().context("cvt: no mode name")?.trim_matches('"').to_string();
            let params: Vec<&str> = f.collect();
            if !list.iter().any(|(o, _)| o.modes.iter().any(|(_, n)| *n == name)) {
                let mut args = vec!["--newmode", name.as_str()];
                args.extend(&params);
                // Already defined but not attached to any output: fine.
                let _ = run("xrandr", &args);
            }
            if !o.modes.iter().any(|(_, n)| *n == name) {
                run("xrandr", &["--addmode", &o.name, &name])?;
            }
            let hz = params.first().and_then(|clk| {
                let clk: f64 = clk.parse().ok()?;
                let (ht, vt): (f64, f64) = (params.get(4)?.parse().ok()?, params.get(8)?.parse().ok()?);
                Some(clk * 1e6 / (ht * vt))
            });
            (Mode { w, h, hz: hz.unwrap_or(fps as f64) }, name)
        }
    };
    if same(from, to) {
        return Ok(None);
    }
    set(&o.name, &to_name, to.hz)?;
    let output = o.name.clone();
    let name = o.name.clone();
    Ok(Some(Changed { output, from, to, restore: Box::new(move || set(&name, &from_name, from.hz)) }))
}
