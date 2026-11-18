//! Screen capture on Wayland through the XDG desktop portal (ScreenCast)
//! and PipeWire. The first start shows the desktop's "share your screen"
//! dialog; the restore token is saved so later starts are silent.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use moon_media::frame::I420Frame;
use tracing::{info, warn};

use crate::capture::VideoSource;
use crate::pipewire::{PwCapture, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_RGBA, SPA_VIDEO_FORMAT_RGBX};

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
        let pw = PwCapture::connect(Some(fd), Some(node), fps, &[])?;
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
        let pw = PwCapture::connect(None, Some(id), fps, &[])?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), _session: None, _proxy: None, _kwin: None }))
    }

    /// Ask KWin for a dedicated screencast of an output (no dialog, works headless).
    /// `dmabuf`: (DRM fourcc, modifiers) the GPU path can import (empty: memory buffers).
    pub fn from_kwin(output: &str, fps: u32, dmabuf: &[(u32, Vec<u64>)]) -> Result<Arc<Self>> {
        let k = crate::kwin::KwinScreencast::start(output)?;
        let spa: Vec<(u32, Vec<u64>)> = [SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_RGBX, SPA_VIDEO_FORMAT_RGBA]
            .into_iter()
            .filter_map(|f| {
                let fourcc = crate::pipewire::drm_fourcc(f)?;
                dmabuf.iter().find(|(c, _)| *c == fourcc).map(|(_, m)| (f, m.clone()))
            })
            .collect();
        let pw = PwCapture::connect(None, Some(k.node), fps, &spa)?;
        Ok(Arc::new(Self { pw: Mutex::new(pw), _session: None, _proxy: None, _kwin: Some(k) }))
    }
}

/// Diagnostics: capture a KWin output for `secs` seconds and print the frame rate
/// and the spread of the intervals between frames.
pub fn probe_capture(output: &str, secs: u64) -> Result<()> {
    // MOON_PROBE_SHM=1 measures the old memory-buffer capture.
    let dmabuf = if std::env::var_os("MOON_PROBE_SHM").is_some() {
        Vec::new()
    } else {
        let g = moon_media::glconv::GpuConverter::new()?;
        [moon_media::glconv::DRM_FORMAT_XRGB8888, moon_media::glconv::DRM_FORMAT_ARGB8888].into_iter().map(|f| (f, g.modifiers(f))).collect()
    };
    let cap = PortalCapture::from_kwin(output, 60, &dmabuf)?;
    let mut pw = cap.pw.lock().unwrap();
    let start = std::time::Instant::now();
    let mut times = Vec::new();
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(at) = pw.next_arrival(Duration::from_millis(200)) {
            times.push(at);
        }
    }
    if !pw.is_dmabuf() {
        if let Some((nonzero, n)) = pw.with_frame(Duration::from_millis(500), |c| (c.data.iter().step_by(4099).filter(|b| **b != 0).count(), c.data.len() / 4099)) {
            println!("memory frame: {nonzero}/{n} sampled bytes non-zero");
        }
    }
    let gaps: Vec<f64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs_f64() * 1000.0).collect();
    let mut sorted = gaps.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| sorted.get(((sorted.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0.0);
    println!(
        "{} ({}) frames in {secs} s = {:.1} fps; interval ms: p1 {:.2} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2}; >25 ms: {}",
        times.len(),
        if pw.is_dmabuf() { "DMA-BUF" } else { "memory" },
        times.len() as f64 / secs as f64,
        pct(0.01),
        pct(0.5),
        pct(0.9),
        pct(0.99),
        pct(1.0),
        gaps.iter().filter(|g| **g > 25.0).count()
    );
    Ok(())
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
    /// Shared capture (portal / PipeWire node, opened at startup), or the one this
    /// source opened for its stream (KWin).
    cap: Option<Arc<PortalCapture>>,
    /// KWin output to capture for each stream (`None`: use the shared capture).
    kwin_output: Option<String>,
    warned: bool,
    filled: bool,
}

impl PipeWireSource {
    pub fn new(cap: Arc<PortalCapture>) -> Self {
        Self { cap: Some(cap), kwin_output: None, warned: false, filled: false }
    }

    /// Capture a KWin output, from `start` (one screencast per stream).
    pub fn kwin(output: String) -> Self {
        Self { cap: None, kwin_output: Some(output), warned: false, filled: false }
    }

    fn cap(&self) -> Result<&Arc<PortalCapture>> {
        self.cap.as_ref().context("screen capture not started")
    }
}

impl VideoSource for PipeWireSource {
    fn name(&self) -> &'static str {
        if self.kwin_output.is_some() {
            "kwin-screencast"
        } else {
            "pipewire-portal"
        }
    }

    fn start(&mut self, fps: u32, dmabuf: &[(u32, Vec<u64>)]) -> Result<()> {
        if let Some(output) = &self.kwin_output {
            // A fresh screencast negotiates the client's frame rate and the
            // output's current refresh rate, and stops when the stream ends.
            self.cap = None;
            self.cap = Some(PortalCapture::from_kwin(output, fps, dmabuf)?);
            // Let the format negotiation finish so the encoder picks the right path.
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(2) {
                if self.cap()?.pw.lock().unwrap().format().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    fn wait_frame(&mut self, _due: Instant, deadline: Instant) -> bool {
        match self.cap() {
            Ok(c) => c.pw.lock().unwrap().wait_new(deadline),
            Err(_) => false,
        }
    }

    fn pending_arrival(&mut self) -> Option<Instant> {
        self.cap().ok()?.pw.lock().unwrap().pending_arrival()
    }

    fn wait_arrival_after(&mut self, after: Instant, deadline: Instant) -> bool {
        match self.cap() {
            Ok(c) => c.pw.lock().unwrap().wait_arrival_after(after, deadline),
            Err(_) => false,
        }
    }

    fn pictures_received(&self) -> Option<u64> {
        Some(self.cap().ok()?.pw.lock().unwrap().frames())
    }

    fn source_fps(&self) -> Option<f64> {
        let f = self.cap().ok()?.pw.lock().unwrap().format()?;
        let (n, d) = f.max_framerate;
        Some(if d == 0 { 0.0 } else { n as f64 / d as f64 })
    }

    fn is_dmabuf(&self) -> bool {
        self.cap().map(|c| c.pw.lock().unwrap().is_dmabuf()).unwrap_or(false)
    }

    fn with_dmabuf(&mut self, f: &mut dyn FnMut(&moon_media::glconv::DmabufFrame, u64) -> Result<()>) -> Option<Result<()>> {
        let cap = self.cap().ok()?.clone();
        let mut pw = cap.pw.lock().unwrap();
        pw.with_dmabuf(|frame, generation| f(frame, generation))
    }

    fn grab_hdr10(&mut self, out: &mut moon_media::hdr::P010Frame, w: usize, h: usize) -> Result<()> {
        let have_previous = out.width == w && out.height == h && self.filled;
        let wait = if have_previous { Duration::ZERO } else { Duration::from_millis(100) };
        let cap = self.cap()?.clone();
        let mut pw = cap.pw.lock().unwrap();
        let got = pw.with_frame(wait, |c| {
            let rgb = matches!(c.format.format, SPA_VIDEO_FORMAT_RGBX | SPA_VIDEO_FORMAT_RGBA);
            out.from_rgb32_scaled(&c.data, c.format.width as usize, c.format.height as usize, c.stride, rgb, w, h);
        });
        if got.is_some() {
            self.filled = true;
        } else if out.width != w || out.height != h {
            *out = moon_media::hdr::P010Frame::new(w, h);
        }
        Ok(())
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        let have_previous = out.width == w && out.height == h && self.filled;
        // The compositor only sends frames when the screen changes: never stall the
        // encoder waiting for one once we have a picture; re-encode the previous one.
        let wait = if have_previous { Duration::ZERO } else { Duration::from_millis(100) };
        let cap = self.cap()?.clone();
        let mut pw = cap.pw.lock().unwrap();
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
                let (csc, yuv444) = (out.csc, out.yuv444);
                *out = I420Frame::with_format(w, h, yuv444);
                out.csc = csc;
            }
        }
        Ok(())
    }
}
