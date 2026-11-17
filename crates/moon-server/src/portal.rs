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
    pub size: Option<(i32, i32)>,
    // Keeping the portal session alive keeps the screencast running.
    _session: Session<Screencast>,
    _proxy: Screencast,
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
        let size = stream.size();
        info!("portal screencast: PipeWire node {node}, size {size:?}");
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        let pw = PwCapture::connect(Some(fd), Some(node), fps)?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), size, _session: session, _proxy: proxy }))
    }
}

/// Video source reading frames from a PipeWire stream.
pub struct PipeWireSource {
    pub cap: Arc<PortalCapture>,
    warned: bool,
}

impl PipeWireSource {
    pub fn new(cap: Arc<PortalCapture>) -> Self {
        Self { cap, warned: false }
    }
}

impl VideoSource for PipeWireSource {
    fn name(&self) -> &'static str {
        "pipewire-portal"
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        let mut pw = self.cap.pw.lock().unwrap();
        let got = pw.with_frame(Duration::from_millis(50), |c| {
            let rgb = matches!(c.format.format, SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_RGBA);
            out.from_rgb32_scaled(&c.data, c.format.width as usize, c.format.height as usize, c.stride, rgb, w, h);
        });
        if got.is_none() {
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
