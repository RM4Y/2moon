//! Audio playback / capture for the client: PulseAudio (or PipeWire) on
//! Linux, WASAPI through cpal on Windows.

use anyhow::Result;

#[cfg(not(windows))]
type OutImpl = crate::pulse::PulseStream;
#[cfg(windows)]
type OutImpl = crate::cpal_io::CpalOutput;
#[cfg(not(windows))]
type InImpl = crate::pulse::PulseStream;
#[cfg(windows)]
type InImpl = crate::cpal_io::CpalInput;

/// Playback of interleaved i16 PCM on the default output device.
pub struct AudioOutput(OutImpl);

impl AudioOutput {
    pub fn open(rate: u32, channels: u8, latency_ms: u32) -> Result<Self> {
        #[cfg(not(windows))]
        return Ok(Self(crate::pulse::PulseStream::playback(rate, channels, latency_ms)?));
        #[cfg(windows)]
        return Ok(Self(crate::cpal_io::CpalOutput::open(rate, channels, latency_ms)?));
    }

    pub fn write(&mut self, pcm: &[i16]) -> Result<()> {
        self.0.write(pcm)
    }

    /// Audio queued but not played yet.
    pub fn latency_us(&self) -> u64 {
        self.0.latency_us()
    }

    pub fn flush(&mut self) {
        self.0.flush()
    }
}

/// Capture of interleaved i16 PCM from the default input device (or, on Linux,
/// a named PulseAudio source).
pub struct AudioInput(InImpl);

impl AudioInput {
    pub fn open(device: Option<&str>, rate: u32, channels: u8, fragment_ms: u32) -> Result<Self> {
        #[cfg(not(windows))]
        return Ok(Self(crate::pulse::PulseStream::record(device, rate, channels, fragment_ms)?));
        #[cfg(windows)]
        {
            let _ = (device, fragment_ms);
            Ok(Self(crate::cpal_io::CpalInput::open(rate, channels)?))
        }
    }

    /// Blocking read filling `pcm` completely.
    pub fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        self.0.read(pcm)
    }
}
