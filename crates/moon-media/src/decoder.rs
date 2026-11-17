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
    Ffmpeg(FfmpegDecoder),
}

/// Codecs this machine can decode, best first.
pub fn supported(hardware: bool) -> Vec<Codec> {
    let mut v = Vec::new();
    let nv = hardware && crate::nvdec::available();
    if nv {
        v.push(Codec::Hevc);
    }
    if nv || FfmpegDecoder::new(&["libdav1d"]).is_ok() {
        v.push(Codec::Av1);
    }
    v.push(Codec::H264);
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
            Codec::H264 => {
                info!("H.264 decoding with OpenH264");
                Ok(Self::OpenH264(H264Decoder::new()?))
            }
            Codec::Av1 => {
                info!("AV1 decoding with dav1d");
                Ok(Self::Ffmpeg(FfmpegDecoder::new(&["libdav1d", "av1"])?))
            }
            Codec::Hevc => bail!("no HEVC decoder available (needs NVDEC)"),
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>> {
        match self {
            Self::Nvdec(d) => d.decode(data),
            Self::OpenH264(d) => d.decode(data),
            Self::Ffmpeg(d) => d.decode(data),
        }
    }
}
