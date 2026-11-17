//! Screen capture on Wayland through the XDG desktop portal (ScreenCast)
//! and PipeWire. The first start shows the desktop's "share your screen"
//! dialog; the restore token is saved so later starts are silent.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use moon_media::frame::I420Frame;
use tracing::{info, warn};

use crate::capture::VideoSource;
use crate::pipewire::{PwCapture, SPA_VIDEO_FORMAT_RGBA, SPA_VIDEO_FORMAT_RGBX};

pub struct PortalCapture {
    pub pw: Mutex<PwCapture>,
    // Keeping the portal session alive keeps the screencast running.
    _session: Option<Session<Screencast>>,
    _proxy: Option<Screencast>,
    _kwin: Option<crate::kwin::KwinScreencast>,
}

impl PortalCapture {
    pub async fn open(state_dir: &Path, fps: u32) -> Result<Arc<Self>> {
        let token_path = state_dir.join("portal_restore_token");
        let token = std::fs::read_to_string(&token_path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let proxy = Screencast::new().await.context("xdg-desktop-portal ScreenCast is not available")?;
        let session = proxy.create_session(CreateSessionOptions::default()).await?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::EMBEDDED)
                    .set_sources(SourceType::MONITOR)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?
            .response()?;
        if token.is_none() {
            info!("Waiting for screen-sharing approval in the desktop's portal dialog...");
        }
        let streams = proxy.start(&session, None, StartCastOptions::default()).await?.response().context("screen sharing was refused")?;
        if let Some(t) = streams.restore_token() {
            if let Err(e) = std::fs::write(&token_path, t) {
                warn!("cannot save portal restore token: {e}");
            }
        }
        let stream = streams.streams().first().context("portal returned no stream")?;
        let node = stream.pipe_wire_node_id();
        info!("portal screencast: PipeWire node {node}, size {:?}", stream.size());
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        let pw = PwCapture::connect(Some(fd), Some(node), fps)?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), _session: Some(session), _proxy: Some(proxy), _kwin: None }))
    }

    /// Consume an existing PipeWire video node directly (no portal, no dialog).
    /// `node` is a numeric id or a substring of the node's `media.name`/`node.name`
    /// (e.g. `kwin-screencast`), resolved with `pw-dump`.
    pub fn from_node(node: &str, fps: u32) -> Result<Arc<Self>> {
        let id = match node.parse::<u32>() {
            Ok(id) => id,
            Err(_) => find_node(node)?,
        };
        info!("capturing PipeWire node {id} directly");
        let pw = PwCapture::connect(None, Some(id), fps)?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), _session: None, _proxy: None, _kwin: None }))
    }

    /// Ask KWin for a dedicated screencast of an output (no dialog, works headless).
    pub fn from_kwin(output: &str, fps: u32) -> Result<Arc<Self>> {
        let k = crate::kwin::KwinScreencast::start(output)?;
        let pw = PwCapture::connect(None, Some(k.node), fps)?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), _session: None, _proxy: None, _kwin: Some(k) }))
    }
}

/// Find the first video output node whose name contains `pattern`.
fn find_node(pattern: &str) -> Result<u32> {
    let out = std::process::Command::new("pw-dump").output().context("running pw-dump")?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing pw-dump output")?;
    for o in v.as_array().into_iter().flatten() {
        let props = &o["info"]["props"];
        let class = props["media.class"].as_str().unwrap_or("");
        let names = [props["media.name"].as_str().unwrap_or(""), props["node.name"].as_str().unwrap_or(""), props["node.description"].as_str().unwrap_or("")];
        if class.contains("Video") && class.contains("Output") || class == "Video/Source" {
            if names.iter().any(|n| n.contains(pattern)) {
                if let Some(id) = o["id"].as_u64() {
                    return Ok(id as u32);
                }
            }
        }
    }
    anyhow::bail!("no PipeWire video node matching '{pattern}' (see `pw-dump` / `pw-cli ls Node`)")
}

/// Video source reading frames from a PipeWire stream.
pub struct PipeWireSource {
    pub cap: Arc<PortalCapture>,
    warned: bool,
    filled: bool,
}

impl PipeWireSource {
    pub fn new(cap: Arc<PortalCapture>) -> Self {
        Self { cap, warned: false, filled: false }
    }
}

impl VideoSource for PipeWireSource {
    fn name(&self) -> &'static str {
        "pipewire-portal"
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        let have_previous = out.width == w && out.height == h && self.filled;
        // The compositor only sends frames when the screen changes: never stall the
        // encoder waiting for one once we have a picture; re-encode the previous one.
        let wait = if have_previous { Duration::ZERO } else { Duration::from_millis(100) };
        let mut pw = self.cap.pw.lock().unwrap();
        let got = pw.with_frame(wait, |c| {
            let rgb = matches!(c.format.format, SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_RGBA);
            out.from_rgb32_scaled(&c.data, c.format.width as usize, c.format.height as usize, c.stride, rgb, w, h);
        });
        if got.is_some() {
            self.filled = true;
        } else if !have_previous {
            if !self.warned {
                warn!("no frame from PipeWire yet (state: {})", pw.state());
                self.warned = true;
            }
            if out.width != w || out.height != h {
                *out = I420Frame::new(w, h);
            }
        }
        Ok(())
    }
}
