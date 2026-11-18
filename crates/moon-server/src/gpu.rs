//! Zero-copy video path, as Sunshine: the captured picture stays on the GPU.
//! Linux: DMA-BUF capture -> OpenGL colour conversion -> NVENC CUDA input.
//! Elsewhere there is none yet (`GpuPath::new` gives None): pictures go through memory.

use anyhow::Result;
use moon_media::encoder::VideoEncoder;
use moon_media::frame::Csc;
use moon_media::h264::EncodedFrame;

use crate::capture::VideoSource;

#[cfg(target_os = "linux")]
pub struct GpuPath {
    conv: moon_media::glconv::GpuConverter,
    input: moon_media::nvenc::CudaInput,
    /// Buffer generation of the source the converter's imports belong to.
    generation: u64,
}

#[cfg(not(target_os = "linux"))]
pub struct GpuPath(std::convert::Infallible);

impl GpuPath {
    /// The GPU path for this encoder, with the (DRM fourcc, modifiers) the capture
    /// may hand over as DMA-BUFs.
    #[cfg(target_os = "linux")]
    pub fn new(enc: &mut VideoEncoder) -> Option<Result<(Self, Vec<(u32, Vec<u64>)>)>> {
        use moon_media::glconv::{GpuConverter, DRM_FORMAT_ABGR8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_XRGB8888};
        let nv = enc.nvenc_mut()?;
        Some(nv.cuda_input().and_then(|input| GpuConverter::new().map(|c| (c, input))).map(|(conv, input)| {
            let formats = [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888, DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888].into_iter().map(|f| (f, conv.modifiers(f))).filter(|(_, m)| !m.is_empty()).collect();
            (Self { conv, input, generation: u64::MAX }, formats)
        }))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(_enc: &mut VideoEncoder) -> Option<Result<(Self, Vec<(u32, Vec<u64>)>)>> {
        None
    }

    /// Convert the source's newest picture, if new, into the encoder's input.
    #[cfg(target_os = "linux")]
    pub fn convert(&mut self, src: &mut dyn VideoSource, csc: Csc) -> Option<Result<()>> {
        let Self { conv, input, generation } = self;
        src.with_dmabuf(&mut |f, g| {
            if g != *generation {
                conv.forget_buffers();
                *generation = g;
            }
            conv.convert(f, input, csc)
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn convert(&mut self, _src: &mut dyn VideoSource, _csc: Csc) -> Option<Result<()>> {
        match self.0 {}
    }

    /// Encode the picture last converted.
    #[cfg(target_os = "linux")]
    pub fn encode<'a>(&mut self, enc: &'a mut VideoEncoder, index: u32) -> Result<EncodedFrame<'a>> {
        enc.nvenc_mut().expect("GPU path needs direct NVENC").encode_cuda(index as u64)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn encode<'a>(&mut self, _enc: &'a mut VideoEncoder, _index: u32) -> Result<EncodedFrame<'a>> {
        match self.0 {}
    }
}
