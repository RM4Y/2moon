//! Desktop audio on Windows: WASAPI loopback of the default playback device.

use std::time::{Duration, Instant};

use anyhow::Result;
use moon_media::cpal_io::CpalInput;
use tracing::info;

use crate::capture::AudioSource;

/// What the computer plays, paced by the clock: a loopback capture only delivers
/// samples while something plays, so silence fills the gaps.
pub struct LoopbackSource {
    input: CpalInput,
    rate: u32,
    channels: usize,
    start: Instant,
    produced: u64,
}

impl LoopbackSource {
    pub fn new(rate: u32, channels: u8) -> Result<Self> {
        let (input, device) = CpalInput::open_loopback(rate, channels)?;
        info!("desktop audio: loopback of {device}");
        Ok(Self { input, rate, channels: channels as usize, start: Instant::now(), produced: 0 })
    }
}

impl AudioSource for LoopbackSource {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let frames = pcm.len() / self.channels;
        // Never let captured audio pile up: keep at most ~30 ms waiting.
        let keep = self.rate as usize * self.channels * 30 / 1000;
        if self.input.queued() > keep + pcm.len() {
            self.input.trim(keep);
        }
        // Wait for the samples until a little after this packet is due; then pad
        // with silence (nothing playing, or the device late).
        self.produced += frames as u64;
        let due = self.start + Duration::from_secs_f64(self.produced as f64 / self.rate as f64);
        let wait = due.saturating_duration_since(Instant::now()) + Duration::from_millis(10);
        self.input.read_or_silence(pcm, wait);
        // After a long stall (e.g. the stream paused), restart the clock rather than burst.
        if Instant::now() > due + Duration::from_millis(200) {
            self.start = Instant::now();
            self.produced = 0;
        }
        Ok(())
    }
}
