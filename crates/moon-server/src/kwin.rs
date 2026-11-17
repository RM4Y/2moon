//! Headless-friendly screen capture on KDE Plasma: ask KWin for a screencast
//! of an output through its private `zkde_screencast_unstable_v1` Wayland
//! protocol (no portal dialog). KWin only grants the protocol to executables
//! listed in a desktop file with `X-KDE-Wayland-Interfaces`; see
//! [`install_desktop_file`].
//!
//! The resulting PipeWire node is negotiated by us alone, without DMA-BUF
//! modifiers, so KWin delivers CPU-readable (memfd) buffers.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tracing::info;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

#[allow(non_upper_case_globals, non_camel_case_types, dead_code, clippy::all)]
pub mod proto {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use proto::zkde_screencast_stream_unstable_v1::{self as stream_ev, ZkdeScreencastStreamUnstableV1};
use proto::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

const POINTER_EMBEDDED: u32 = 2;

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, String)>,
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(st: &mut Self, out: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            if let Some(o) = st.outputs.iter_mut().find(|(o, _)| o == out) {
                o.1 = name;
            }
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(_: &mut Self, _: &ZkdeScreencastUnstableV1, _: proto::zkde_screencast_unstable_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(st: &mut Self, _: &ZkdeScreencastStreamUnstableV1, ev: stream_ev::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            stream_ev::Event::Created { node } => st.node = Some(node),
            stream_ev::Event::Failed { error } => st.failed = Some(error),
            stream_ev::Event::Closed => st.closed = true,
        }
    }
}

/// Keeps the Wayland connection (and therefore the KWin screencast) alive.
pub struct KwinScreencast {
    pub node: u32,
    _conn: Connection,
    _stream: ZkdeScreencastStreamUnstableV1,
    _queue: Arc<std::sync::Mutex<EventQueue<State>>>,
}

impl KwinScreencast {
    /// Start a screencast of the output whose name contains `output` (any output if empty).
    pub fn start(output: &str) -> Result<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            // Headless servers usually run outside the graphical session's environment.
            std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        }
        let conn = Connection::connect_to_env().context("cannot connect to the Wayland compositor (is WAYLAND_DISPLAY / XDG_RUNTIME_DIR set?)")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("wayland registry")?;
        let qh = queue.handle();
        let mut st = State::default();
        let sc: ZkdeScreencastUnstableV1 = globals.bind(&qh, 1..=1, ()).context(
            "KWin did not expose zkde_screencast_unstable_v1 to this program. Run `moon-server --install-kwin-permission` once, then restart the server",
        )?;
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                let o: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, ());
                st.outputs.push((o, String::new()));
            }
        }
        queue.roundtrip(&mut st)?;
        let names: Vec<String> = st.outputs.iter().map(|(_, n)| n.clone()).collect();
        let (out, name) = st
            .outputs
            .iter()
            .find(|(_, n)| output.is_empty() || n.contains(output))
            .cloned()
            .with_context(|| format!("no output matching '{output}' (available: {names:?})"))?;
        let stream = sc.stream_output(&out, POINTER_EMBEDDED, &qh, ());
        for _ in 0..50 {
            queue.roundtrip(&mut st)?;
            if st.node.is_some() || st.failed.is_some() || st.closed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if let Some(e) = st.failed {
            bail!("KWin refused the screencast: {e}");
        }
        let node = st.node.context("KWin did not create the screencast stream")?;
        info!("KWin screencast of output '{name}' -> PipeWire node {node} (outputs: {names:?})");
        Ok(Self { node, _conn: conn, _stream: stream, _queue: Arc::new(std::sync::Mutex::new(queue)) })
    }
}

/// Write the desktop file that authorises this executable to use KWin's screencast protocol.
pub fn install_desktop_file() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    let dir = base.join("applications");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("2moon-server.desktop");
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=2moon streaming host\nExec={}\nNoDisplay=true\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1\n",
            exe.display()
        ),
    )?;
    Ok(path)
}
