//! Encoder selection: hardware (FFmpeg/NVENC, VAAPI...) with software fallback.

use anyhow::Result;
use tracing::{info, warn};

use crate::ffenc::FfmpegEncoder;
use crate::frame::I420Frame;
use crate::h264::{EncodedFrame, H264Encoder};

/// Video codecs of the GameStream protocol (`bitStreamFormat` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264 = 0,
    Hevc = 1,
    Av1 = 2,
}

impl Codec {
    pub fn from_format(v: u32) -> Self {
        match v {
            1 => Codec::Hevc,
            2 => Codec::Av1,
            _ => Codec::H264,
        }
    }

    /// FFmpeg NVENC encoder name.
    pub fn nvenc(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::Hevc => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }

    /// FFmpeg encoder name for a hardware family: "nvenc" (NVIDIA), "amf" (AMD),
    /// "qsv" (Intel Quick Sync), "vaapi" (AMD / Intel on Linux), "videotoolbox" (macOS,
    /// H.264 / HEVC); None for anything else.
    pub fn hardware(self, family: &str) -> Option<String> {
        let c = match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        };
        if family == "videotoolbox" && self == Codec::Av1 {
            return None;
        }
        matches!(family, "nvenc" | "amf" | "qsv" | "vaapi" | "videotoolbox").then(|| format!("{c}_{family}"))
    }
}

pub enum VideoEncoder {
    Software(H264Encoder),
    Ffmpeg(FfmpegEncoder),
    /// NVENC driven directly (supports reference frame invalidation).
    #[cfg(target_os = "linux")]
    Nvenc(crate::nvenc::NvencEncoder),
}

impl VideoEncoder {
    /// `choice`: "auto" (hardware if available), "software", a hardware family ("nvenc",
    /// "amf", "qsv"), or any FFmpeg encoder name.
    /// HEVC and AV1 always need a hardware encoder.
    pub fn open(choice: &str, codec: Codec, width: usize, height: usize, fps: u32, bitrate_kbps: u32, opts: &crate::ffenc::EncoderOptions) -> Result<Self> {
        let ff_name = match choice {
            "software" | "openh264" if codec == Codec::H264 => None,
            "software" | "openh264" => anyhow::bail!("{codec:?} needs a hardware encoder"),
            "auto" => Some(codec.nvenc().to_string()),
            family @ ("nvenc" | "amf" | "qsv" | "vaapi" | "videotoolbox") => codec.hardware(family),
            other => Some(other.to_string()),
        };
        let choice = if codec != Codec::H264 && choice == "auto" { "nvenc" } else { choice };
        // "auto" / "nvenc": NVENC directly first (RFI), FFmpeg's NVENC wrapper as fallback.
        #[cfg(target_os = "linux")]
        if matches!(choice, "auto" | "nvenc") {
            let dpb = if opts.rfi { crate::nvenc::RFI_DPB } else { opts.max_refs.max(1) };
            match crate::nvenc::NvencEncoder::new(codec, width, height, fps, bitrate_kbps, opts, dpb) {
                Ok(e) => {
                    info!("{codec:?} encoding with NVENC (direct{}) {width}x{height}@{fps} {bitrate_kbps} kbps", if e.supports_rfi() { ", RFI" } else { "" });
                    return Ok(Self::Nvenc(e));
                }
                Err(e) if opts.hdr10 => {
                    // Only direct NVENC does 10-bit: retry in 8-bit SDR rather than fail.
                    warn!("10-bit HDR encoding unavailable ({e:#}); using 8-bit SDR");
                    return Self::open(choice, codec, width, height, fps, bitrate_kbps, &crate::ffenc::EncoderOptions { hdr10: false, ..opts.clone() });
                }
                Err(e) if opts.yuv444 => {
                    // Only direct NVENC does 4:4:4: retry in 4:2:0 rather than fail.
                    warn!("4:4:4 encoding unavailable ({e:#}); using 4:2:0");
                    return Self::open(choice, codec, width, height, fps, bitrate_kbps, &crate::ffenc::EncoderOptions { yuv444: false, ..opts.clone() });
                }
                Err(e) => warn!("direct NVENC unavailable ({e:#}); trying FFmpeg"),
            }
        }
        if opts.yuv444 || opts.hdr10 {
            return Self::open(choice, codec, width, height, fps, bitrate_kbps, &crate::ffenc::EncoderOptions { yuv444: false, hdr10: false, ..opts.clone() });
        }
        if let Some(name) = ff_name {
            match FfmpegEncoder::with_options(&name, width, height, fps, bitrate_kbps, opts) {
                Ok(e) => return Ok(Self::Ffmpeg(e)),
                Err(e) if choice == "auto" => warn!("hardware encoder unavailable ({e:#}); using software"),
                Err(e) => return Err(e),
            }
        }
        info!("software encoder (OpenH264) {width}x{height}@{fps}");
        Ok(Self::Software(H264Encoder::new(fps, bitrate_kbps)?))
    }

    /// The direct NVENC encoder, if that is what was opened (GPU input path).
    #[cfg(target_os = "linux")]
    pub fn nvenc_mut(&mut self) -> Option<&mut crate::nvenc::NvencEncoder> {
        match self {
            Self::Nvenc(e) => Some(e),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The FFmpeg VAAPI encoder (Intel / AMD), if that is what was opened: its
    /// GPU surfaces can be drawn into directly (GPU input path).
    #[cfg(target_os = "linux")]
    pub fn vaapi_mut(&mut self) -> Option<&mut crate::ffenc::FfmpegEncoder> {
        match self {
            Self::Ffmpeg(e) if e.has_vaapi_surfaces() => Some(e),
            _ => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Software(_) => "openh264",
            Self::Ffmpeg(e) => &e.codec_name,
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => &e.codec_name,
        }
    }

    /// Stop referencing frames `first..=last`; false if only an IDR can recover.
    pub fn invalidate(&mut self, first: u32, last: u32) -> bool {
        let _ = (first, last);
        match self {
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.invalidate(first, last),
            _ => false,
        }
    }

    /// Whether this encoder takes 10-bit P010 pictures and outputs HDR10.
    pub fn hdr10(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.hdr10,
            _ => false,
        }
    }

    /// Encode a 10-bit picture (only when [`Self::hdr10`]).
    pub fn encode_p010(&mut self, f: &crate::hdr::P010Frame, index: u32) -> Result<EncodedFrame<'_>> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.encode_p010(f, index as u64),
            _ => {
                let _ = (f, index);
                anyhow::bail!("this encoder does not take 10-bit pictures")
            }
        }
    }

    /// Whether this encoder takes 4:4:4 pictures.
    pub fn yuv444(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.yuv444,
            _ => false,
        }
    }

    pub fn request_idr(&mut self) {
        match self {
            Self::Software(e) => e.request_idr(),
            Self::Ffmpeg(e) => e.request_idr(),
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.request_idr(),
        }
    }

    /// `index`: the frame number this picture will carry (used to invalidate it later).
    pub fn encode(&mut self, f: &I420Frame, pts90k: i64, index: u32) -> Result<EncodedFrame<'_>> {
        let _ = index;
        match self {
            Self::Software(e) => e.encode(f),
            Self::Ffmpeg(e) => e.encode(f, pts90k),
            #[cfg(target_os = "linux")]
            Self::Nvenc(e) => e.encode(f, index as u64),
        }
    }
}
