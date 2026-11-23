//! Audio playback / capture through cpal (WASAPI on Windows): ring buffer,
//! channel mapping and resampling, like `PulseStream`.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

use crate::resample::Converter;

/// Interleaved f32 samples at the device rate and channel count.
#[derive(Default)]
struct Ring {
    buf: VecDeque<f32>,
}

type Shared = Arc<(Mutex<Ring>, Condvar)>;

fn start<T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static>(dev: &cpal::Device, cfg: &StreamConfig, shared: Shared) -> Result<Stream> {
    let s = dev.build_output_stream::<T, _, _>(
        cfg.clone(),
        move |data: &mut [T], _| {
            let mut ring = shared.0.lock().unwrap();
            for d in data.iter_mut() {
                *d = T::from_sample(ring.buf.pop_front().unwrap_or(0.0));
            }
        },
        |e| tracing::warn!("audio output: {e}"),
        None,
    )?;
    s.play()?;
    Ok(s)
}

pub struct CpalOutput {
    _stream: Stream,
    shared: Shared,
    conv: Converter,
    rate: u32,
    channels: usize,
    max_frames: usize,
    tmp: Vec<f32>,
    out: Vec<f32>,
}

impl CpalOutput {
    /// Playback of `channels`@`rate` interleaved i16 on the default device.
    pub fn open(rate: u32, channels: u8, _latency_ms: u32) -> Result<Self> {
        let dev = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let def = dev.default_output_config()?;
        let shared: Shared = Default::default();
        // Prefer the stream's own format; fall back to the device's mix format (WASAPI shared mode).
        let wanted = StreamConfig { channels: channels as u16, sample_rate: rate, buffer_size: cpal::BufferSize::Default };
        let (stream, cfg) = match start::<f32>(&dev, &wanted, shared.clone()) {
            Ok(s) => (s, wanted),
            Err(_) => {
                let cfg = def.config();
                let s = match def.sample_format() {
                    SampleFormat::I16 => start::<i16>(&dev, &cfg, shared.clone())?,
                    SampleFormat::F32 => start::<f32>(&dev, &cfg, shared.clone())?,
                    f => bail!("unsupported output sample format {f:?}"),
                };
                (s, cfg)
            }
        };
        let dev_ch = cfg.channels as usize;
        Ok(Self {
            _stream: stream,
            shared,
            conv: Converter::new(channels as usize, rate, dev_ch, cfg.sample_rate),
            rate: cfg.sample_rate,
            channels: dev_ch,
            max_frames: cfg.sample_rate as usize / 5, // never queue more than 200 ms
            tmp: Vec::new(),
            out: Vec::new(),
        })
    }

    pub fn write(&mut self, pcm: &[i16]) -> Result<()> {
        self.tmp.clear();
        self.tmp.extend(pcm.iter().map(|s| *s as f32 / 32768.0));
        self.out.clear();
        self.conv.push(&self.tmp, &mut self.out);
        let mut ring = self.shared.0.lock().unwrap();
        ring.buf.extend(self.out.iter().copied());
        let max = self.max_frames * self.channels;
        if ring.buf.len() > max {
            let extra = ring.buf.len() - max;
            ring.buf.drain(..extra);
        }
        Ok(())
    }

    pub fn latency_us(&self) -> u64 {
        let n = self.shared.0.lock().unwrap().buf.len() / self.channels.max(1);
        n as u64 * 1_000_000 / self.rate.max(1) as u64
    }

    pub fn flush(&mut self) {
        self.shared.0.lock().unwrap().buf.clear();
    }
}

pub struct CpalInput {
    _stream: Stream,
    shared: Shared,
}

fn start_in<T: cpal::SizedSample + Send + 'static>(dev: &cpal::Device, cfg: &StreamConfig, shared: Shared, mut conv: Converter) -> Result<Stream>
where
    f32: cpal::FromSample<T>,
{
    let mut tmp = Vec::new();
    let mut out = Vec::new();
    let s = dev.build_input_stream::<T, _, _>(
        cfg.clone(),
        move |data: &[T], _| {
            tmp.clear();
            tmp.extend(data.iter().map(|s| <f32 as cpal::FromSample<T>>::from_sample_(*s)));
            out.clear();
            conv.push(&tmp, &mut out);
            let (m, cv) = &*shared;
            let mut ring = m.lock().unwrap();
            ring.buf.extend(out.iter().copied());
            // keep at most one second
            if ring.buf.len() > 48_000 * 8 {
                let extra = ring.buf.len() - 48_000 * 8;
                ring.buf.drain(..extra);
            }
            cv.notify_all();
        },
        |e| tracing::warn!("audio input: {e}"),
        None,
    )?;
    s.play()?;
    Ok(s)
}

/// Input devices (id + name), for the microphone choice.
pub fn input_devices() -> Vec<crate::audio_io::InputDevice> {
    let Ok(devs) = cpal::default_host().input_devices() else { return Vec::new() };
    devs.filter_map(|d| {
        let id = d.id().ok()?.to_string();
        let label = d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| id.clone());
        Some(crate::audio_io::InputDevice { id, label })
    })
    .collect()
}

impl CpalInput {
    /// Capture `channels`@`rate` from the input device whose id is `device`
    /// (the default one if `None` or not found).
    pub fn open(device: Option<&str>, rate: u32, channels: u8) -> Result<Self> {
        let host = cpal::default_host();
        let wanted = device.and_then(|id| host.input_devices().ok()?.find(|d| d.id().is_ok_and(|i| i.to_string() == id)));
        let dev = match wanted {
            Some(d) => d,
            None => host.default_input_device().ok_or_else(|| anyhow!("no audio input device"))?,
        };
        let def = dev.default_input_config()?;
        let cfg = def.config();
        let shared: Shared = Default::default();
        let conv = Converter::new(cfg.channels as usize, cfg.sample_rate, channels as usize, rate);
        let stream = match def.sample_format() {
            SampleFormat::I16 => start_in::<i16>(&dev, &cfg, shared.clone(), conv)?,
            SampleFormat::F32 => start_in::<f32>(&dev, &cfg, shared.clone(), conv)?,
            f => bail!("unsupported input sample format {f:?}"),
        };
        Ok(Self { _stream: stream, shared })
    }

    /// Capture what the default output device plays (WASAPI loopback), as
    /// `channels`@`rate`.
    pub fn open_loopback(rate: u32, channels: u8) -> Result<(Self, String)> {
        let dev = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
        let def = dev.default_output_config()?;
        let cfg = def.config();
        let shared: Shared = Default::default();
        let conv = Converter::new(cfg.channels as usize, cfg.sample_rate, channels as usize, rate);
        // An input stream on an output device is a loopback capture with WASAPI.
        let stream = match def.sample_format() {
            SampleFormat::I16 => start_in::<i16>(&dev, &cfg, shared.clone(), conv)?,
            SampleFormat::F32 => start_in::<f32>(&dev, &cfg, shared.clone(), conv)?,
            f => bail!("unsupported output sample format {f:?}"),
        };
        Ok((Self { _stream: stream, shared }, format!("{name} ({} ch, {} Hz)", cfg.channels, cfg.sample_rate)))
    }

    /// Fill `pcm` with what has been captured, waiting at most `wait` for it and
    /// padding with silence (a loopback capture gets nothing while nothing plays).
    pub fn read_or_silence(&mut self, pcm: &mut [i16], wait: Duration) {
        let (m, cv) = &*self.shared;
        let mut ring = m.lock().unwrap();
        let deadline = std::time::Instant::now() + wait;
        while ring.buf.len() < pcm.len() {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            ring = cv.wait_timeout(ring, deadline - now).unwrap().0;
        }
        for p in pcm.iter_mut() {
            *p = (ring.buf.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0) * 32767.0) as i16;
        }
    }

    /// Samples waiting to be read.
    pub fn queued(&self) -> usize {
        self.shared.0.lock().unwrap().buf.len()
    }

    /// Drop all but the newest `keep` samples (latency control).
    pub fn trim(&mut self, keep: usize) {
        let mut ring = self.shared.0.lock().unwrap();
        let extra = ring.buf.len().saturating_sub(keep);
        ring.buf.drain(..extra);
    }

    /// Blocking read filling `pcm` completely.
    pub fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let (m, cv) = &*self.shared;
        let mut ring = m.lock().unwrap();
        while ring.buf.len() < pcm.len() {
            let (r, t) = cv.wait_timeout(ring, Duration::from_secs(1)).unwrap();
            ring = r;
            if t.timed_out() && ring.buf.len() < pcm.len() {
                bail!("no audio from the input device");
            }
        }
        for p in pcm.iter_mut() {
            *p = (ring.buf.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        Ok(())
    }
}

