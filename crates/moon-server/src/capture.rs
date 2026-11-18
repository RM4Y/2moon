//! Video and audio sources for the host.

use std::time::{Duration, Instant};

use anyhow::Result;
use moon_media::frame::I420Frame;
use moon_media::glconv::DmabufFrame;
use moon_media::hdr::P010Frame;

pub trait VideoSource: Send {
    /// Fill `out` with the current picture at the requested size.
    fn grab(&mut self, out: &mut I420Frame, width: usize, height: usize) -> Result<()>;
    /// Same, as 10-bit HDR10 (SDR content with white at 203 nits).
    fn grab_hdr10(&mut self, out: &mut P010Frame, width: usize, height: usize) -> Result<()>;
    fn name(&self) -> &'static str;

    /// Start capturing for a stream at `fps`. `dmabuf` lists the (DRM fourcc, modifiers)
    /// the GPU path can import; empty means CPU copies only.
    fn start(&mut self, _fps: u32, _dmabuf: &[(u32, Vec<u64>)]) -> Result<()> {
        Ok(())
    }

    /// Wait for a picture newer than the last one used, at the latest until `deadline`;
    /// true if there is one. Sources that just render on demand pace themselves to `due`.
    fn wait_frame(&mut self, due: Instant, _deadline: Instant) -> bool {
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        true
    }

    /// Highest frame rate the source may deliver (`Some(0.0)`: variable, unknown).
    fn source_fps(&self) -> Option<f64> {
        None
    }

    /// Frames come as DMA-BUFs: use [`Self::with_dmabuf`] instead of `grab`.
    fn is_dmabuf(&self) -> bool {
        false
    }

    /// Pass the newest DMA-BUF picture, if new, and its buffer generation to `f`.
    fn with_dmabuf(&mut self, _f: &mut dyn FnMut(&DmabufFrame, u64) -> Result<()>) -> Option<Result<()>> {
        None
    }
}

/// Animated synthetic pattern rendered directly in I420 (no colour conversion).
pub struct TestPattern {
    frame: u64,
}

impl TestPattern {
    pub fn new() -> Self {
        Self { frame: 0 }
    }
}

impl VideoSource for TestPattern {
    fn name(&self) -> &'static str {
        "test-pattern"
    }

    fn grab_hdr10(&mut self, out: &mut P010Frame, w: usize, h: usize) -> Result<()> {
        // Same picture as `grab`, rendered to RGB first.
        let mut f = I420Frame::new(w, h);
        self.grab(&mut f, w, h)?;
        let mut rgb = vec![0u32; w * h];
        f.to_xrgb(&mut rgb);
        let bytes: Vec<u8> = rgb.iter().flat_map(|p| p.to_le_bytes()).collect();
        out.from_rgb32_scaled(&bytes, w, h, w * 4, false, w, h);
        Ok(())
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        if out.width != w || out.height != h {
            let (csc, yuv444) = (out.csc, out.yuv444);
            *out = I420Frame::with_format(w, h, yuv444);
            out.csc = csc;
        }
        let t = self.frame as usize;
        self.frame += 1;
        // Luma: diagonal gradient scrolling right, plus a bright square bouncing around.
        let (bx, by) = bounce(t, w.saturating_sub(w / 8), h.saturating_sub(h / 8));
        let bs = (w / 8).max(8);
        for y in 0..h {
            let row = &mut out.y[y * w..(y + 1) * w];
            for (x, p) in row.iter_mut().enumerate() {
                *p = (16 + ((x + y / 2 + t * 4) % 220)) as u8;
            }
            if y >= by && y < by + bs {
                let end = (bx + bs).min(w);
                row[bx..end].fill(235);
            }
        }
        // Chroma: eight colour bands.
        let (cw, ch) = (out.chroma_width(), out.chroma_height());
        const BANDS: [(u8, u8); 8] = [(128, 128), (16, 146), (166, 16), (54, 34), (202, 222), (90, 240), (240, 110), (128, 128)];
        for y in 0..ch {
            for x in 0..cw {
                let (u, v) = BANDS[(x * 8 / cw.max(1)).min(7)];
                out.u[y * cw + x] = u;
                out.v[y * cw + x] = v;
            }
        }
        Ok(())
    }
}

fn bounce(t: usize, w: usize, h: usize) -> (usize, usize) {
    let tri = |p: usize, m: usize| if m == 0 { 0 } else if (p / m) % 2 == 0 { p % m } else { m - p % m };
    (tri(t * 7, w), tri(t * 5, h))
}

pub trait AudioSource: Send {
    /// Read exactly `pcm.len()` interleaved samples of `channels()` channels (blocks at real-time pace).
    fn read(&mut self, pcm: &mut [i16]) -> Result<()>;
}

/// 440 Hz sine tone, paced by the wall clock.
pub struct SineSource {
    channels: usize,
    phase: u64,
    start: Instant,
    produced: u64,
    rate: u32,
}

impl SineSource {
    pub fn new(rate: u32, channels: usize) -> Self {
        Self { channels: channels.max(1), phase: 0, start: Instant::now(), produced: 0, rate }
    }
}

impl AudioSource for SineSource {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let ch = self.channels;
        let frames = pcm.len() / ch;
        for f in 0..frames {
            let s = ((self.phase as f64 * 440.0 / self.rate as f64 * std::f64::consts::TAU).sin() * 6000.0) as i16;
            pcm[f * ch..f * ch + ch].fill(s);
            self.phase += 1;
        }
        self.produced += frames as u64;
        let due = self.start + Duration::from_secs_f64(self.produced as f64 / self.rate as f64);
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        Ok(())
    }
}

/// What the host is playing, captured from the default sink's monitor.
pub struct PulseMonitor(moon_media::pulse::PulseStream);

impl PulseMonitor {
    pub fn new(rate: u32, channels: u8) -> Result<Self> {
        Ok(Self(moon_media::pulse::PulseStream::record(Some("@DEFAULT_MONITOR@"), rate, channels, 5)?))
    }
}

impl AudioSource for PulseMonitor {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        self.0.read(pcm)
    }
}
