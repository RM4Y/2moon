//! Decoder selection: NVDEC (all codecs) with software fallbacks
//! (OpenH264 for H.264, dav1d for AV1).

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::encoder::Codec;
use crate::ffenc::FfmpegDecoder;
use crate::frame::I420Frame;
use crate::h264::H264Decoder;
use crate::nvdec::NvDecoder;

pub enum VideoDecoder {
    Nvdec(NvDecoder),
    OpenH264(H264Decoder),
    Ffmpeg(FfmpegDecoder, &'static str),
}

/// Codecs this machine can decode, best first.
pub fn supported(hardware: bool) -> Vec<Codec> {
    let mut v = Vec::new();
    let nv = hardware && crate::nvdec::available();
    if nv {
        // GPU decoding: the most efficient codecs first.
        v.push(Codec::Hevc);
        v.push(Codec::Av1);
        v.push(Codec::H264);
    } else {
        // CPU decoding: H.264 is by far the cheapest to decode (lowest latency).
        v.push(Codec::H264);
        if FfmpegDecoder::new(&["libdav1d"]).is_ok() {
            v.push(Codec::Av1);
        }
        if FfmpegDecoder::new(&["hevc"]).is_ok() {
            v.push(Codec::Hevc);
        }
    }
    v
}

impl VideoDecoder {
    /// `hardware`: try NVDEC first.
    pub fn open(codec: Codec, hardware: bool) -> Result<Self> {
        if hardware {
            match NvDecoder::new(codec) {
                Ok(d) => {
                    info!("{codec:?} decoding with NVDEC");
                    return Ok(Self::Nvdec(d));
                }
                Err(e) => warn!("NVDEC unavailable ({e:#}); using software decoding"),
            }
        }
        match codec {
            Codec::H264 => match FfmpegDecoder::new(&["h264"]) {
                // FFmpeg's H.264 decoder is markedly faster than OpenH264.
                Ok(d) => {
                    info!("H.264 decoding with FFmpeg (CPU)");
                    Ok(Self::Ffmpeg(d, "H.264 FFmpeg (CPU)"))
                }
                Err(_) => {
                    info!("H.264 decoding with OpenH264 (CPU)");
                    Ok(Self::OpenH264(H264Decoder::new()?))
                }
            },
            Codec::Av1 => {
                info!("AV1 decoding with dav1d (CPU)");
                Ok(Self::Ffmpeg(FfmpegDecoder::new(&["libdav1d", "av1"])?, "AV1 dav1d (CPU)"))
            }
            Codec::Hevc => match FfmpegDecoder::new(&["hevc"]) {
                Ok(d) => {
                    info!("HEVC decoding with FFmpeg (CPU)");
                    Ok(Self::Ffmpeg(d, "HEVC FFmpeg (CPU)"))
                }
                Err(_) => bail!("no HEVC decoder available"),
            },
        }
    }

    /// Short description for statistics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Nvdec(_) => "NVDEC (GPU)",
            Self::OpenH264(_) => "H.264 OpenH264 (CPU)",
            Self::Ffmpeg(_, n) => n,
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>> {
        match self {
            Self::Nvdec(d) => d.decode(data),
            Self::OpenH264(d) => d.decode(data),
            Self::Ffmpeg(d, _) => d.decode(data),
        }
    }
}
