//! Encoder selection: hardware (FFmpeg/NVENC, VAAPI...) with software fallback.

use anyhow::Result;
use tracing::{info, warn};

use crate::ffenc::FfmpegEncoder;
use crate::frame::I420Frame;
use crate::h264::{EncodedFrame, H264Encoder};

pub enum VideoEncoder {
    Software(H264Encoder),
    Ffmpeg(FfmpegEncoder),
}

impl VideoEncoder {
    /// `choice`: "auto" (hardware if available), "software", "nvenc", or any FFmpeg encoder name.
    pub fn open(choice: &str, width: usize, height: usize, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let ff_name = match choice {
            "software" | "openh264" => None,
            "auto" | "nvenc" => Some("h264_nvenc"),
            other => Some(other),
        };
        if let Some(name) = ff_name {
            match FfmpegEncoder::new(name, width, height, fps, bitrate_kbps) {
                Ok(e) => return Ok(Self::Ffmpeg(e)),
                Err(e) if choice == "auto" => warn!("hardware encoder unavailable ({e:#}); using software"),
                Err(e) => return Err(e),
            }
        }
        info!("software encoder (OpenH264) {width}x{height}@{fps}");
        Ok(Self::Software(H264Encoder::new(fps, bitrate_kbps)?))
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Software(_) => "openh264",
            Self::Ffmpeg(e) => &e.codec_name,
        }
    }

    pub fn request_idr(&mut self) {
        match self {
            Self::Software(e) => e.request_idr(),
            Self::Ffmpeg(e) => e.request_idr(),
        }
    }

    pub fn encode(&mut self, f: &I420Frame, pts90k: i64) -> Result<EncodedFrame<'_>> {
        match self {
            Self::Software(e) => e.encode(f),
            Self::Ffmpeg(e) => e.encode(f, pts90k),
        }
    }
}
