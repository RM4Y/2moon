//! Video and audio sources for the host.

use std::time::{Duration, Instant};

use anyhow::Result;
use moon_media::frame::I420Frame;

pub trait VideoSource: Send {
    /// Fill `out` with the current picture at the requested size.
    fn grab(&mut self, out: &mut I420Frame, width: usize, height: usize) -> Result<()>;
    fn name(&self) -> &'static str;
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

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        if out.width != w || out.height != h {
            *out = I420Frame::new(w, h);
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
        let cw = out.chroma_width();
        let ch = h.div_ceil(2);
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
    /// Read exactly `pcm.len()` interleaved stereo samples (blocks at real-time pace).
    fn read(&mut self, pcm: &mut [i16]) -> Result<()>;
}

/// 440 Hz sine tone, paced by the wall clock.
pub struct SineSource {
    phase: u64,
    start: Instant,
    produced: u64,
    rate: u32,
}

impl SineSource {
    pub fn new(rate: u32) -> Self {
        Self { phase: 0, start: Instant::now(), produced: 0, rate }
    }
}

impl AudioSource for SineSource {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let frames = pcm.len() / 2;
        for f in 0..frames {
            let s = ((self.phase as f64 * 440.0 / self.rate as f64 * std::f64::consts::TAU).sin() * 6000.0) as i16;
            pcm[f * 2] = s;
            pcm[f * 2 + 1] = s;
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
    pub fn new(rate: u32) -> Result<Self> {
        Ok(Self(moon_media::pulse::PulseStream::record(Some("@DEFAULT_MONITOR@"), rate, 2, 5)?))
    }
}

impl AudioSource for PulseMonitor {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        self.0.read(pcm)
    }
}
