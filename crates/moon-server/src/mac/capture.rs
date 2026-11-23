//! macOS screen capture (ScreenCaptureKit): BGRA pictures on change, newest kept,
//! converted to YUV by the stream.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use moon_media::frame::I420Frame;
use moon_media::hdr::P010Frame;
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress};
use objc2_screen_capture_kit::SCStreamOutputType;
use tracing::{info, warn};

use super::sck;
use crate::capture::VideoSource;

/// Desktop rectangle of the display being captured, in points (CoreGraphics global
/// coordinates), for absolute mouse positions: (left, top, width, height).
pub static CAPTURED_RECT: Mutex<Option<(f64, f64, f64, f64)>> = Mutex::new(None);

/// A picture in BGRA, rows packed (`width * 4` bytes).
#[derive(Default)]
struct Picture {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

#[derive(Default)]
struct Shared {
    latest: Option<Picture>,
    spare: Option<Picture>,
    seq: u64,
    at: Option<Instant>,
    refresh: f64,
}

#[derive(Default)]
struct Inner {
    shared: Mutex<Shared>,
    cond: Condvar,
}

/// `--video screen`: a display of this Mac.
pub struct ScreenSource {
    output: String,
    inner: Arc<Inner>,
    capture: Option<sck::Capture>,
    /// Picture size asked of ScreenCaptureKit, which scales on the GPU: the
    /// stream's size once known (the display's own size before).
    want: Option<(usize, usize)>,
    fps: u32,
    current: Picture,
    last_seq: u64,
    warned: bool,
}

impl ScreenSource {
    /// `output`: the display number (1 = main) or empty for the main display.
    pub fn new(output: String) -> Self {
        Self { output, inner: Arc::default(), capture: None, want: None, fps: 60, current: Picture::default(), last_seq: 0, warned: false }
    }

    fn ensure_started(&mut self) -> Result<()> {
        if self.capture.is_none() {
            let fps = self.fps;
            self.start(fps, &[])?;
        }
        Ok(())
    }

    fn take_new(&mut self, wait: Duration) -> Result<bool> {
        self.ensure_started()?;
        let inner = self.inner.clone();
        let deadline = Instant::now() + wait;
        let mut sh = inner.shared.lock().unwrap();
        while sh.seq <= self.last_seq {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            sh = inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
        self.last_seq = sh.seq;
        if let Some(p) = sh.latest.take() {
            let old = std::mem::replace(&mut self.current, p);
            if !old.data.is_empty() {
                sh.spare = Some(old);
            }
        }
        Ok(true)
    }

    /// Have ScreenCaptureKit deliver pictures of the stream's size (scaled on the
    /// GPU) instead of converting full Retina pictures on the CPU.
    fn set_size(&mut self, w: usize, h: usize) -> Result<()> {
        if self.want != Some((w, h)) {
            self.want = Some((w, h));
            if self.capture.take().is_some() {
                info!("capture size now {w}x{h}");
            }
        }
        self.ensure_started()
    }

    fn no_picture_yet(&mut self) {
        if !self.warned {
            warn!("no picture from the display yet (Screen Recording permission?)");
            self.warned = true;
        }
    }
}

/// Copy a BGRA sample buffer into the shared slot.
fn on_sample(inner: &Inner, sample: &CMSampleBuffer) {
    unsafe {
        if !sample.is_valid() {
            return;
        }
        // Idle or blank updates come without a picture.
        let Some(buf) = sample.image_buffer() else { return };
        if CVPixelBufferLockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly) != 0 {
            return;
        }
        let (w, h, stride) = (CVPixelBufferGetWidth(&buf), CVPixelBufferGetHeight(&buf), CVPixelBufferGetBytesPerRow(&buf));
        let base = CVPixelBufferGetBaseAddress(&buf) as *const u8;
        if !base.is_null() && w > 0 && h > 0 {
            let mut pic = inner.shared.lock().unwrap().spare.take().unwrap_or_default();
            pic.data.resize(w * h * 4, 0);
            pic.width = w;
            pic.height = h;
            for y in 0..h {
                let src = std::slice::from_raw_parts(base.add(y * stride), w * 4);
                pic.data[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
            }
            let mut sh = inner.shared.lock().unwrap();
            if let Some(old) = sh.latest.replace(pic) {
                sh.spare = Some(old);
            }
            sh.seq += 1;
            sh.at = Some(Instant::now());
            drop(sh);
            inner.cond.notify_all();
        }
        CVPixelBufferUnlockBaseAddress(&buf, CVPixelBufferLockFlags::ReadOnly);
    }
}

impl VideoSource for ScreenSource {
    fn name(&self) -> &'static str {
        "screencapturekit"
    }

    fn start(&mut self, fps: u32, _dmabuf: &[(u32, Vec<u64>)]) -> Result<()> {
        if self.capture.is_some() {
            return Ok(());
        }
        let displays = sck::displays()?;
        let main = objc2_core_graphics::CGMainDisplayID();
        let pick = match self.output.trim().parse::<usize>() {
            Ok(n) if n >= 1 => displays.get(n - 1),
            _ => displays.iter().find(|d| d.id == main).or(displays.first()),
        };
        let Some(display) = pick else { bail!("no display to capture") };
        let (dw, dh, refresh) = sck::display_mode(display.id);
        self.fps = fps;
        let (w, h) = self.want.unwrap_or((dw, dh));
        let b = objc2_core_graphics::CGDisplayBounds(display.id);
        *CAPTURED_RECT.lock().unwrap() = Some((b.origin.x, b.origin.y, b.size.width, b.size.height));
        let id = display.id;
        info!("capturing display {id} ({dw}x{dh} at {refresh:.0} Hz) with ScreenCaptureKit, pictures of {w}x{h}");
        self.inner.shared.lock().unwrap().refresh = refresh;
        let inner = self.inner.clone();
        let handler: sck::Handler = Box::new(move |sample, kind| {
            if kind == SCStreamOutputType::Screen {
                on_sample(&inner, sample);
            }
        });
        self.capture = Some(sck::Capture::start(display, &sck::Config { width: w, height: h, fps: fps.max(30), audio: None, video: true }, handler)?);
        Ok(())
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        self.set_size(w, h)?;
        let have_previous = !self.current.data.is_empty();
        // Re-encode the previous picture rather than stall when nothing changed.
        self.take_new(if have_previous { Duration::ZERO } else { Duration::from_millis(500) })?;
        if self.current.data.is_empty() {
            self.no_picture_yet();
            if out.width != w || out.height != h {
                let (csc, yuv444) = (out.csc, out.yuv444);
                *out = I420Frame::with_format(w, h, yuv444);
                out.csc = csc;
            }
            return Ok(());
        }
        let p = &self.current;
        out.from_rgb32_scaled(&p.data, p.width, p.height, p.width * 4, false, w, h);
        Ok(())
    }

    fn grab_hdr10(&mut self, out: &mut P010Frame, w: usize, h: usize) -> Result<()> {
        self.set_size(w, h)?;
        let have_previous = !self.current.data.is_empty();
        self.take_new(if have_previous { Duration::ZERO } else { Duration::from_millis(500) })?;
        if self.current.data.is_empty() {
            if out.width != w || out.height != h {
                *out = P010Frame::new(w, h);
            }
            return Ok(());
        }
        let p = &self.current;
        out.from_rgb32_scaled(&p.data, p.width, p.height, p.width * 4, false, w, h);
        Ok(())
    }

    fn wait_frame(&mut self, _due: Instant, deadline: Instant) -> bool {
        if self.ensure_started().is_err() {
            return false;
        }
        let mut sh = self.inner.shared.lock().unwrap();
        while sh.seq <= self.last_seq {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            sh = self.inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
        true
    }

    fn pending_arrival(&mut self) -> Option<Instant> {
        let sh = self.inner.shared.lock().unwrap();
        if sh.seq > self.last_seq { sh.at } else { None }
    }

    fn wait_arrival_after(&mut self, after: Instant, deadline: Instant) -> bool {
        if self.ensure_started().is_err() {
            return false;
        }
        let mut sh = self.inner.shared.lock().unwrap();
        loop {
            if sh.seq > self.last_seq && sh.at.is_some_and(|a| a >= after) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            sh = self.inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
    }

    fn pictures_received(&self) -> Option<u64> {
        Some(self.inner.shared.lock().unwrap().seq)
    }

    fn source_fps(&self) -> Option<f64> {
        let r = self.inner.shared.lock().unwrap().refresh;
        // Pictures come when the screen changes, at most at the refresh rate.
        Some(if r > 0.0 { r } else { 0.0 })
    }
}
