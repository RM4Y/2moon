//! Decoder selection, best first:
//! 1. NVDEC (NVIDIA GPUs): H.264, HEVC, AV1
//! 2. VAAPI through FFmpeg (Intel / AMD GPUs): whatever the driver decodes
//! 3. CPU: FFmpeg (h264, hevc, libdav1d) or OpenH264 for H.264

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::encoder::Codec;
use crate::ffenc::FfmpegDecoder;
use crate::frame::I420Frame;
use crate::h264::H264Decoder;
use crate::nvdec::NvDecoder;

/// A decoded picture: in system memory, or still on the GPU (zero-copy).
pub enum Decoded<'a> {
    Cpu(&'a I420Frame),
    Vaapi(crate::ffenc::VaapiFrame),
    /// NVDEC picture kept in video memory (copy it into GL textures with `CudaGlTarget`).
    Cuda(crate::nvdec::CudaFrame),
}

pub enum VideoDecoder {
    Nvdec(NvDecoder),
    /// FFmpeg with VAAPI. Falls back to the CPU by itself for H.264/HEVC if
    /// the GPU rejects a stream; `Self::decode` switches to a CPU decoder otherwise.
    Vaapi(FfmpegDecoder, Codec),
    OpenH264(H264Decoder),
    Ffmpeg(FfmpegDecoder, &'static str),
}

/// FFmpeg decoder names for a codec, CPU first choice first.
fn ffmpeg_names(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["h264"],
        Codec::Hevc => &["hevc"],
        Codec::Av1 => &["libdav1d", "av1"],
    }
}

/// FFmpeg decoders able to drive a hardware accelerator.
fn ffmpeg_hw_names(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["h264"],
        Codec::Hevc => &["hevc"],
        Codec::Av1 => &["av1"],
    }
}

fn vaapi_usable(codec: Codec) -> bool {
    crate::vaapi::supports(codec) && FfmpegDecoder::new(ffmpeg_hw_names(codec)).is_ok()
}

fn cpu_usable(codec: Codec) -> bool {
    match codec {
        Codec::H264 => true, // OpenH264 is built in
        Codec::Hevc => FfmpegDecoder::new(&["hevc"]).is_ok(),
        Codec::Av1 => FfmpegDecoder::new(&["libdav1d"]).is_ok(),
    }
}

/// Codecs this machine can decode, best first.
pub fn supported(hardware: bool) -> Vec<Codec> {
    let order = [Codec::Hevc, Codec::Av1, Codec::H264];
    let mut v = Vec::new();
    if hardware {
        // GPU decoding: the most efficient codecs first.
        let nv = crate::nvdec::available();
        for c in order {
            if nv || vaapi_usable(c) {
                v.push(c);
            }
        }
    }
    // CPU decoding: H.264 is by far the cheapest to decode (lowest latency).
    for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
        if !v.contains(&c) && cpu_usable(c) {
            v.push(c);
        }
    }
    v
}

/// Can this machine decode `codec` in 4:4:4? NVDEC (HEVC, recent GPUs) or the
/// CPU through FFmpeg; VAAPI is not used for 4:4:4.
pub fn supports_444(codec: Codec, hardware: bool) -> bool {
    match codec {
        Codec::H264 => FfmpegDecoder::new(&["h264"]).is_ok(),
        Codec::Hevc => (hardware && crate::nvdec::supports_444(Codec::Hevc)) || FfmpegDecoder::new(&["hevc"]).is_ok(),
        Codec::Av1 => false,
    }
}

impl VideoDecoder {
    /// `hardware`: try NVDEC, then VAAPI, before the CPU. `zero_copy`: let VAAPI
    /// pictures stay on the GPU (the caller must handle [`Decoded::Vaapi`]) while true.
    pub fn open(codec: Codec, hardware: bool, zero_copy: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>, yuv444: bool) -> Result<Self> {
        if yuv444 {
            if hardware && codec == Codec::Hevc && crate::nvdec::supports_444(codec) {
                if let Ok(d) = NvDecoder::new(codec) {
                    info!("{codec:?} 4:4:4 decoding with NVDEC");
                    return Ok(Self::Nvdec(d));
                }
            }
            info!("{codec:?} 4:4:4: decoding on the CPU");
            return Self::open_cpu(codec);
        }
        if hardware {
            match NvDecoder::new(codec) {
                Ok(mut d) => {
                    d.set_zero_copy(zero_copy.clone());
                    info!("{codec:?} decoding with NVDEC");
                    return Ok(Self::Nvdec(d));
                }
                Err(e) => info!("NVDEC unavailable ({e:#})"),
            }
            if let Some(va) = crate::vaapi::probe().filter(|i| i.codecs.contains(&codec)) {
                match FfmpegDecoder::new_hw(ffmpeg_hw_names(codec), "vaapi", Some(&va.device)) {
                    Ok(mut d) => {
                        d.zero_copy = zero_copy;
                        info!("{codec:?} decoding with VAAPI ({}, {})", va.driver, va.device);
                        return Ok(Self::Vaapi(d, codec));
                    }
                    Err(e) => warn!("VAAPI {codec:?} decoder unavailable ({e:#}); using the CPU"),
                }
            }
        }
        Self::open_cpu(codec)
    }

    fn open_cpu(codec: Codec) -> Result<Self> {
        let label = match codec {
            Codec::H264 => "H.264 FFmpeg (CPU)",
            Codec::Hevc => "HEVC FFmpeg (CPU)",
            Codec::Av1 => "AV1 dav1d (CPU)",
        };
        match FfmpegDecoder::new(ffmpeg_names(codec)) {
            Ok(d) => {
                info!("{codec:?} decoding on the CPU with FFmpeg");
                Ok(Self::Ffmpeg(d, label))
            }
            Err(_) if codec == Codec::H264 => {
                info!("H.264 decoding on the CPU with OpenH264");
                Ok(Self::OpenH264(H264Decoder::new()?))
            }
            Err(e) => bail!("no {codec:?} decoder available: {e:#}"),
        }
    }

    /// Short description for statistics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Nvdec(d) if d.zero_copy_active() => "NVDEC (GPU, copie zéro)",
            Self::Nvdec(_) => "NVDEC (GPU)",
            Self::Vaapi(d, _) if !d.hw_active => "VAAPI → CPU (repli FFmpeg)",
            Self::Vaapi(d, _) if d.zero_copy.as_ref().is_some_and(|z| z.load(std::sync::atomic::Ordering::Relaxed)) => "VAAPI (GPU, copie zéro)",
            Self::Vaapi(..) => "VAAPI (GPU)",
            Self::OpenH264(_) => "H.264 OpenH264 (CPU)",
            Self::Ffmpeg(_, n) => n,
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Decoded<'_>>> {
        // The GPU refused the stream (e.g. AV1, which has no CPU path inside
        // FFmpeg's hwaccel decoder): switch to a CPU decoder for good.
        if let Self::Vaapi(d, codec) = self {
            match d.decode_ready(data) {
                Ok(true) => {}
                Ok(false) => return Ok(None),
                Err(e) => {
                    let codec = *codec;
                    warn!("VAAPI decoding failed ({e:#}); switching to the CPU");
                    *self = Self::open_cpu(codec)?;
                    bail!("decoder switched to the CPU");
                }
            }
            let Self::Vaapi(d, _) = self else { unreachable!() };
            return Ok(Some(match d.take_gpu_picture() {
                Some(g) => Decoded::Vaapi(g),
                None => Decoded::Cpu(d.picture()),
            }));
        }
        if let Self::Nvdec(d) = self {
            if d.decode(data)?.is_some() {
                return Ok(Some(Decoded::Cpu(d.decode_last())));
            }
            return Ok(d.take_gpu_picture().map(Decoded::Cuda));
        }
        Ok(match self {
            Self::Nvdec(_) => unreachable!(),
            Self::OpenH264(d) => d.decode(data)?,
            Self::Ffmpeg(d, _) => d.decode(data)?,
            Self::Vaapi(..) => unreachable!(),
        }
        .map(Decoded::Cpu))
    }
}
