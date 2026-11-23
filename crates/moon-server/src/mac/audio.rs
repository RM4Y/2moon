//! Desktop audio on macOS: what the Mac plays, captured by ScreenCaptureKit
//! (macOS 13+), paced by the clock like the WASAPI loopback on Windows.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_media::{kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, CMBlockBuffer, CMSampleBuffer};
use objc2_screen_capture_kit::SCStreamOutputType;
use tracing::info;

use super::sck;
use crate::capture::AudioSource;

/// Interleaved f32 stereo samples captured and not read yet.
#[derive(Default)]
struct Ring {
    buf: VecDeque<f32>,
}

type Shared = Arc<(Mutex<Ring>, Condvar)>;

pub struct ScreenAudio {
    _capture: sck::Capture,
    shared: Shared,
    rate: u32,
    channels: usize,
    start: Instant,
    produced: u64,
}

/// ScreenCaptureKit captures in stereo; more channels are filled from it.
const CAPTURED: usize = 2;

fn on_sample(shared: &Shared, sample: &CMSampleBuffer) {
    unsafe {
        if !sample.is_valid() {
            return;
        }
        let mut needed = 0usize;
        sample.audio_buffer_list_with_retained_block_buffer(&mut needed, std::ptr::null_mut(), 0, None, None, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, std::ptr::null_mut());
        if needed == 0 {
            return;
        }
        // AudioBufferList is variable length: allocate what CoreMedia asked for (8-byte aligned).
        let mut storage = vec![0u64; needed.div_ceil(8)];
        let list = storage.as_mut_ptr() as *mut AudioBufferList;
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let st = sample.audio_buffer_list_with_retained_block_buffer(std::ptr::null_mut(), list, needed, None, None, kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, &mut block);
        if st != 0 {
            return;
        }
        let n = (*list).mNumberBuffers as usize;
        let bufs: &[AudioBuffer] = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), n);
        let mut out = Vec::new();
        if n >= CAPTURED {
            // One buffer per channel (non-interleaved float32), ScreenCaptureKit's format.
            let l = std::slice::from_raw_parts(bufs[0].mData as *const f32, bufs[0].mDataByteSize as usize / 4);
            let r = std::slice::from_raw_parts(bufs[1].mData as *const f32, bufs[1].mDataByteSize as usize / 4);
            out.reserve(l.len() * 2);
            for (a, b) in l.iter().zip(r) {
                out.push(*a);
                out.push(*b);
            }
        } else if n == 1 {
            let ch = bufs[0].mNumberChannels.max(1) as usize;
            let s = std::slice::from_raw_parts(bufs[0].mData as *const f32, bufs[0].mDataByteSize as usize / 4);
            for f in s.chunks_exact(ch) {
                out.push(f[0]);
                out.push(if ch > 1 { f[1] } else { f[0] });
            }
        }
        if !block.is_null() {
            objc2_core_foundation::CFRetained::<CMBlockBuffer>::from_raw(std::ptr::NonNull::new_unchecked(block));
        }
        let (m, cv) = &**shared;
        let mut ring = m.lock().unwrap();
        ring.buf.extend(out);
        // Keep at most one second.
        let max = 48_000 * CAPTURED;
        if ring.buf.len() > max {
            let extra = ring.buf.len() - max;
            ring.buf.drain(..extra);
        }
        cv.notify_all();
    }
}

impl ScreenAudio {
    pub fn new(rate: u32, channels: u8) -> Result<Self> {
        let displays = sck::displays()?;
        let main = objc2_core_graphics::CGMainDisplayID();
        let Some(display) = displays.iter().find(|d| d.id == main).or(displays.first()) else { bail!("no display (needed by ScreenCaptureKit for audio)") };
        let shared: Shared = Default::default();
        let s = shared.clone();
        let handler: sck::Handler = Box::new(move |sample, kind| {
            if kind == SCStreamOutputType::Audio {
                on_sample(&s, sample);
            }
        });
        let capture = sck::Capture::start(display, &sck::Config { width: 2, height: 2, fps: 1, audio: Some((rate, CAPTURED as u32)), video: false }, handler)?;
        info!("desktop audio: ScreenCaptureKit ({rate} Hz, stereo{})", if channels as usize > CAPTURED { format!(" sent as {channels} channels") } else { String::new() });
        Ok(Self { _capture: capture, shared, rate, channels: channels as usize, start: Instant::now(), produced: 0 })
    }
}

impl AudioSource for ScreenAudio {
    fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let frames = pcm.len() / self.channels;
        let (m, cv) = &*self.shared;
        let mut ring = m.lock().unwrap();
        // Never let captured audio pile up: keep at most ~30 ms waiting.
        let keep = self.rate as usize * CAPTURED * 30 / 1000;
        if ring.buf.len() > keep + frames * CAPTURED {
            let extra = ring.buf.len() - keep;
            ring.buf.drain(..extra);
        }
        // Wait until a little after this packet is due; then pad with silence.
        self.produced += frames as u64;
        let due = self.start + Duration::from_secs_f64(self.produced as f64 / self.rate as f64);
        let deadline = due.max(Instant::now()) + Duration::from_millis(10);
        while ring.buf.len() < frames * CAPTURED {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            ring = cv.wait_timeout(ring, deadline - now).unwrap().0;
        }
        let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        for f in 0..frames {
            let l = ring.buf.pop_front().unwrap_or(0.0);
            let r = ring.buf.pop_front().unwrap_or(0.0);
            let o = &mut pcm[f * self.channels..(f + 1) * self.channels];
            o.fill(0);
            o[0] = to_i16(l);
            if self.channels > 1 {
                o[1] = to_i16(r);
            }
        }
        drop(ring);
        // After a long stall (e.g. the stream paused), restart the clock rather than burst.
        if Instant::now() > due + Duration::from_millis(200) {
            self.start = Instant::now();
            self.produced = 0;
        }
        Ok(())
    }
}
