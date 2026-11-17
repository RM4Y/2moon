//! VA-API probe (Intel / AMD GPUs, and NVIDIA through nvidia-vaapi-driver):
//! which codecs the GPU can decode, and on which DRM render node. Decoding
//! itself goes through FFmpeg's VAAPI hwaccel (see `ffenc::FfmpegDecoder`).
//! libva is loaded at runtime, so machines without it simply report nothing.

use std::ffi::{c_int, c_void};
use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use libloading::Library;
use tracing::{debug, info};

use crate::encoder::Codec;

const VA_ENTRYPOINT_VLD: c_int = 1;
const VA_PROFILE_H264_MAIN: c_int = 6;
const VA_PROFILE_H264_HIGH: c_int = 7;
const VA_PROFILE_H264_CONSTRAINED_BASELINE: c_int = 13;
const VA_PROFILE_HEVC_MAIN: c_int = 17;
const VA_PROFILE_AV1_PROFILE0: c_int = 32;

#[derive(Debug, Clone, Default)]
pub struct VaapiInfo {
    /// DRM render node, e.g. `/dev/dri/renderD128`.
    pub device: String,
    /// Driver vendor string.
    pub driver: String,
    /// Codecs with a hardware decode (VLD) entrypoint.
    pub codecs: Vec<Codec>,
}

/// Probe once; `None` without libva, a render node or a working driver.
pub fn probe() -> Option<&'static VaapiInfo> {
    static INFO: OnceLock<Option<VaapiInfo>> = OnceLock::new();
    INFO.get_or_init(|| {
        let r = unsafe { probe_inner() };
        match &r {
            Some(i) => info!("VAAPI: {} on {} decodes {:?}", i.driver, i.device, i.codecs),
            None => debug!("VAAPI unavailable"),
        }
        r
    })
    .as_ref()
}

pub fn supports(codec: Codec) -> bool {
    probe().is_some_and(|i| i.codecs.contains(&codec))
}

unsafe fn probe_inner() -> Option<VaapiInfo> {
    let va = Library::new("libva.so.2").ok()?;
    let va_drm = Library::new("libva-drm.so.2").ok()?;
    let get_display: libloading::Symbol<unsafe extern "C" fn(c_int) -> *mut c_void> = va_drm.get(b"vaGetDisplayDRM\0").ok()?;
    let initialize: libloading::Symbol<unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int> = va.get(b"vaInitialize\0").ok()?;
    let terminate: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> = va.get(b"vaTerminate\0").ok()?;
    let vendor: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> *const std::ffi::c_char> = va.get(b"vaQueryVendorString\0").ok()?;
    let max_profiles: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> = va.get(b"vaMaxNumProfiles\0").ok()?;
    let query_profiles: libloading::Symbol<unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int> = va.get(b"vaQueryConfigProfiles\0").ok()?;
    let max_entrypoints: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> = va.get(b"vaMaxNumEntrypoints\0").ok()?;
    let query_entrypoints: libloading::Symbol<unsafe extern "C" fn(*mut c_void, c_int, *mut c_int, *mut c_int) -> c_int> = va.get(b"vaQueryConfigEntrypoints\0").ok()?;

    // Prefer an explicitly configured node, then the render nodes in order.
    let mut nodes: Vec<String> = std::env::var("MOON_VAAPI_DEVICE").ok().into_iter().collect();
    nodes.extend((128..136).map(|n| format!("/dev/dri/renderD{n}")));
    for node in nodes {
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&node) else { continue };
        let dpy = get_display(file.as_raw_fd());
        if dpy.is_null() {
            continue;
        }
        let (mut maj, mut min) = (0, 0);
        if initialize(dpy, &mut maj, &mut min) != 0 {
            continue;
        }
        let driver = {
            let p = vendor(dpy);
            if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        let mut profiles = vec![0 as c_int; max_profiles(dpy).max(0) as usize];
        let mut n = 0;
        let mut has_vld = |p: c_int| -> bool {
            let mut eps = vec![0 as c_int; max_entrypoints(dpy).max(0) as usize];
            let mut m = 0;
            query_entrypoints(dpy, p, eps.as_mut_ptr(), &mut m) == 0 && eps[..(m.max(0) as usize).min(eps.len())].contains(&VA_ENTRYPOINT_VLD)
        };
        let mut codecs = Vec::new();
        if query_profiles(dpy, profiles.as_mut_ptr(), &mut n) == 0 {
            let profiles = &profiles[..(n.max(0) as usize).min(profiles.len())];
            let decodes = |want: &[c_int], has_vld: &mut dyn FnMut(c_int) -> bool| want.iter().any(|p| profiles.contains(p) && has_vld(*p));
            // Order: best compression first.
            if decodes(&[VA_PROFILE_HEVC_MAIN], &mut has_vld) {
                codecs.push(Codec::Hevc);
            }
            if decodes(&[VA_PROFILE_AV1_PROFILE0], &mut has_vld) {
                codecs.push(Codec::Av1);
            }
            if decodes(&[VA_PROFILE_H264_HIGH, VA_PROFILE_H264_MAIN, VA_PROFILE_H264_CONSTRAINED_BASELINE], &mut has_vld) {
                codecs.push(Codec::H264);
            }
        }
        terminate(dpy);
        drop(file);
        if !codecs.is_empty() {
            return Some(VaapiInfo { device: node, driver, codecs });
        }
        debug!("VAAPI on {node} ({driver}) has no usable decoder");
    }
    None
}

/// One plane of an exported surface (DRM PRIME, separate layers).
#[derive(Debug, Clone, Copy)]
pub struct DmaBufPlane {
    /// DRM fourcc of this layer (`R8  ` for luma, `GR88` for NV12 chroma...).
    pub drm_format: u32,
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

/// A VAAPI surface exported as DMA-BUF file descriptors (closed on drop).
#[derive(Debug)]
pub struct DmaBufSurface {
    pub width: u32,
    pub height: u32,
    pub planes: Vec<DmaBufPlane>,
    fds: Vec<i32>,
}

impl Drop for DmaBufSurface {
    fn drop(&mut self) {
        for fd in &self.fds {
            unsafe { libc_close(*fd) };
        }
    }
}

extern "C" {
    #[link_name = "close"]
    fn libc_close(fd: c_int) -> c_int;
}

struct ExportApi {
    _va: Library,
    sync: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
    export: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut c_void) -> c_int,
}

fn export_api() -> Option<&'static ExportApi> {
    static API: OnceLock<Option<ExportApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let va = Library::new("libva.so.2").ok()?;
        let sync = *va.get(b"vaSyncSurface\0").ok()?;
        let export = *va.get(b"vaExportSurfaceHandle\0").ok()?;
        Some(ExportApi { _va: va, sync, export })
    })
    .as_ref()
}

/// Export a decoded surface for import into EGL (one layer per plane).
///
/// # Safety
/// `display` must be a live, initialised VADisplay and `surface` one of its surfaces.
pub unsafe fn export_surface(display: *mut c_void, surface: u32) -> anyhow::Result<DmaBufSurface> {
    const MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
    const EXPORT_READ_ONLY: u32 = 0x1;
    const EXPORT_SEPARATE_LAYERS: u32 = 0x4;
    let a = export_api().ok_or_else(|| anyhow::anyhow!("libva has no vaExportSurfaceHandle"))?;
    // VADRMPRIMESurfaceDescriptor (312 bytes; offsets checked against va_drmcommon.h).
    let mut d = [0u8; 312];
    unsafe {
        let r = (a.sync)(display, surface);
        if r != 0 {
            anyhow::bail!("vaSyncSurface failed ({r})");
        }
        let r = (a.export)(display, surface, MEM_TYPE_DRM_PRIME_2, EXPORT_READ_ONLY | EXPORT_SEPARATE_LAYERS, d.as_mut_ptr() as *mut c_void);
        if r != 0 {
            anyhow::bail!("vaExportSurfaceHandle failed ({r})");
        }
    }
    let u32_at = |o: usize| u32::from_ne_bytes(d[o..o + 4].try_into().unwrap());
    let num_objects = (u32_at(12) as usize).min(4);
    let objects: Vec<(i32, u64)> = (0..num_objects).map(|i| {
        let o = 16 + i * 16;
        (u32_at(o) as i32, u64::from_ne_bytes(d[o + 8..o + 16].try_into().unwrap()))
    }).collect();
    let mut s = DmaBufSurface { width: u32_at(4), height: u32_at(8), planes: Vec::new(), fds: objects.iter().map(|o| o.0).collect() };
    let num_layers = (u32_at(80) as usize).min(4);
    for l in 0..num_layers {
        let o = 84 + l * 56;
        if u32_at(o + 4) != 1 {
            anyhow::bail!("unexpected multi-plane layer");
        }
        let obj = u32_at(o + 8) as usize;
        let (fd, modifier) = *objects.get(obj).ok_or_else(|| anyhow::anyhow!("bad object index"))?;
        s.planes.push(DmaBufPlane { drm_format: u32_at(o), fd, offset: u32_at(o + 24), pitch: u32_at(o + 40), modifier });
    }
    Ok(s)
}
