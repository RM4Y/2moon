//! Audio playback / capture for the client: PulseAudio (or PipeWire) on
//! Linux, WASAPI through cpal on Windows, CoreAudio through cpal on macOS.

use anyhow::Result;

#[cfg(not(any(windows, target_os = "macos")))]
type OutImpl = crate::pulse::PulseStream;
#[cfg(any(windows, target_os = "macos"))]
type OutImpl = crate::cpal_io::CpalOutput;
#[cfg(not(any(windows, target_os = "macos")))]
type InImpl = crate::pulse::PulseStream;
#[cfg(any(windows, target_os = "macos"))]
type InImpl = crate::cpal_io::CpalInput;

/// Playback of interleaved i16 PCM on the default output device.
pub struct AudioOutput(OutImpl);

impl AudioOutput {
    pub fn open(rate: u32, channels: u8, latency_ms: u32) -> Result<Self> {
        #[cfg(not(any(windows, target_os = "macos")))]
        return Ok(Self(crate::pulse::PulseStream::playback(rate, channels, latency_ms)?));
        #[cfg(any(windows, target_os = "macos"))]
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

/// A microphone that [`AudioInput::open`] can record from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    /// What to pass to [`AudioInput::open`] (PulseAudio source name / cpal device id).
    pub id: String,
    /// Human-readable name.
    pub label: String,
}

/// The microphones of this machine (monitors of outputs excluded). Empty if
/// they cannot be listed; recording from the default one still works.
pub fn input_devices() -> Vec<InputDevice> {
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // pulse-simple cannot enumerate: ask pactl (PulseAudio and PipeWire both ship it).
        let Ok(out) = std::process::Command::new("pactl").env("LC_ALL", "C").args(["list", "sources"]).output() else { return Vec::new() };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut v = Vec::new();
        let mut name: Option<String> = None;
        for line in text.lines() {
            let l = line.trim();
            if let Some(n) = l.strip_prefix("Name: ") {
                name = Some(n.to_string());
            } else if let Some(d) = l.strip_prefix("Description: ") {
                if let Some(n) = name.take() {
                    if !n.ends_with(".monitor") {
                        v.push(InputDevice { id: n, label: d.to_string() });
                    }
                }
            }
        }
        v
    }
    #[cfg(any(windows, target_os = "macos"))]
    crate::cpal_io::input_devices()
}

/// Capture of interleaved i16 PCM from the default input device, or a given
/// one (see [`input_devices`]).
pub struct AudioInput(InImpl);

impl AudioInput {
    pub fn open(device: Option<&str>, rate: u32, channels: u8, fragment_ms: u32) -> Result<Self> {
        #[cfg(not(any(windows, target_os = "macos")))]
        return Ok(Self(crate::pulse::PulseStream::record(device, rate, channels, fragment_ms)?));
        #[cfg(any(windows, target_os = "macos"))]
        {
            let _ = fragment_ms;
            Ok(Self(crate::cpal_io::CpalInput::open(device, rate, channels)?))
        }
    }

    /// Blocking read filling `pcm` completely.
    pub fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        self.0.read(pcm)
    }
}
