//! VA-API is Linux-only: on other systems nothing is available.

use std::ffi::c_void;

use crate::encoder::Codec;

#[derive(Debug, Clone, Default)]
pub struct VaapiInfo {
    pub device: String,
    pub driver: String,
    pub codecs: Vec<Codec>,
    pub hevc_444: bool,
}

pub fn probe() -> Option<&'static VaapiInfo> {
    None
}

pub fn supports(_codec: Codec) -> bool {
    false
}

#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
    pub drm_format: u32,
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

#[derive(Debug)]
pub struct DmaBufSurface {
    pub width: u32,
    pub height: u32,
    pub planes: Vec<DmaBufPlane>,
}

/// # Safety
/// Never succeeds on this system.
pub unsafe fn export_surface(_display: *mut c_void, _surface: u32) -> anyhow::Result<DmaBufSurface> {
    anyhow::bail!("VA-API is not available on this system")
}
