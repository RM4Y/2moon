//! GNOME (Mutter): the `org.gnome.Mutter.DisplayConfig` D-Bus API, as GNOME
//! Settings uses it. Only modes the display offers can be chosen; the change is
//! "temporary" (not saved to monitors.xml, no confirmation dialog).

use std::collections::HashMap;

use anyhow::{Context, Result};
use ashpd::zbus;
use ashpd::wire::{OwnedVariant, Variant};

use super::{closest, same, Changed, Mode};

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";

type Props = HashMap<String, OwnedVariant>;
/// (connector, vendor, product, serial)
type MonitorId = (String, String, String, String);
/// (id, width, height, refresh, preferred scale, supported scales, properties)
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (MonitorId, Vec<MonitorMode>, Props);
/// (x, y, scale, transform, primary, monitors, properties)
type Logical = (i32, i32, f64, u32, bool, Vec<MonitorId>, Props);
type State = (u32, Vec<Monitor>, Vec<Logical>, Props);
/// Configuration sent back: (x, y, scale, transform, primary, [(connector, mode id, properties)]).
type LogicalConfig = (i32, i32, f64, u32, bool, Vec<(String, String, Props)>);

fn flag(p: &Props, k: &str) -> bool {
    p.get(k).is_some_and(|v| matches!(**v, Variant::Bool(true)))
}

fn block<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => tokio::task::block_in_place(|| h.block_on(f)),
        Err(_) => tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(f),
    }
}

async fn state(c: &zbus::Connection) -> Result<State> {
    let m = c.call_method(Some(DEST), PATH, Some(DEST), "GetCurrentState", &()).await.context("GNOME display settings (Mutter DisplayConfig) unavailable")?;
    Ok(m.body().deserialize::<State>()?)
}

fn current_mode(m: &Monitor) -> Option<&MonitorMode> {
    m.1.iter().find(|x| flag(&x.6, "is-current"))
}

/// Apply the current layout with `connector` switched to `mode_id`.
async fn set(c: &zbus::Connection, connector: &str, mode_id: &str) -> Result<()> {
    let (serial, monitors, logical, _) = state(c).await?;
    let mut config: Vec<LogicalConfig> = Vec::new();
    for (x, y, mut scale, transform, primary, ids, _) in logical {
        let mut mons = Vec::new();
        for id in ids {
            let Some(mon) = monitors.iter().find(|m| m.0 == id) else { continue };
            let mode = if id.0 == connector {
                let mm = mon.1.iter().find(|m| m.0 == mode_id).context("mode not offered")?;
                // The scale must be one the new mode supports.
                if !mm.5.iter().any(|s| (s - scale).abs() < 1e-6) {
                    scale = mm.4;
                }
                mode_id.to_string()
            } else {
                current_mode(mon).map(|m| m.0.clone()).context("monitor without a current mode")?
            };
            mons.push((id.0.clone(), mode, Props::new()));
        }
        config.push((x, y, scale, transform, primary, mons));
    }
    // Method 1: temporary.
    c.call_method(Some(DEST), PATH, Some(DEST), "ApplyMonitorsConfig", &(serial, 1u32, config, Props::new())).await.context("Mutter refused the configuration")?;
    Ok(())
}

pub fn apply(want: &str, w: u32, h: u32, fps: u32) -> Result<Option<Changed>> {
    block(async move {
        let c = zbus::Connection::session().await?;
        let (_, monitors, logical, _) = state(&c).await?;
        // Default: the primary monitor (what the screen-cast portal usually offers first).
        let primary: Option<String> = logical.iter().find(|l| l.4).and_then(|l| l.5.first()).map(|id| id.0.clone());
        let mon = monitors
            .iter()
            .filter(|m| current_mode(m).is_some())
            .find(|m| if want.is_empty() { primary.as_deref().map(|p| m.0 .0 == p).unwrap_or(true) } else { m.0 .0.contains(want) })
            .with_context(|| format!("no active monitor matching '{want}'"))?;
        let connector = mon.0 .0.clone();
        let cur = current_mode(mon).unwrap();
        let mode = |m: &MonitorMode| Mode { w: m.1 as u32, h: m.2 as u32, hz: m.3 };
        let from = mode(cur);
        let from_id = cur.0.clone();
        let offered: Vec<(Mode, String)> = mon.1.iter().map(|m| (mode(m), m.0.clone())).collect();
        let Some((to, to_id)) = closest(&offered, w, h, fps) else {
            anyhow::bail!("'{connector}' has no {w}x{h} mode (GNOME only offers the display's own modes)");
        };
        if same(from, to) || to_id == from_id {
            return Ok(None);
        }
        set(&c, &connector, &to_id).await?;
        let output = connector.clone();
        let restore: super::Restore = Box::new(move || {
            block(async move {
                let c = zbus::Connection::session().await?;
                set(&c, &connector, &from_id).await
            })
        });
        Ok(Some(Changed { output, from, to, restore }))
    })
}
