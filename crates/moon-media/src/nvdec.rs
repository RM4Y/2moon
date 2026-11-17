//! NVIDIA hardware decoding (NVDEC) for H.264, HEVC and AV1, using the
//! CUDA driver API and the NVCUVID parser, both loaded at runtime.
//!
//! Only a handful of plain-data structures are needed; they are written into
//! zeroed byte buffers at their documented offsets so reserved fields stay 0.

use std::ffi::{c_int, c_uint, c_ulong, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Result};
use libloading::Library;
use tracing::{debug, info};

use crate::encoder::Codec;
use crate::frame::I420Frame;

type CuResult = c_int;

struct Api {
    _cuda: Library,
    _cuvid: Library,
    cu_init: unsafe extern "C" fn(c_uint) -> CuResult,
    cu_device_get: unsafe extern "C" fn(*mut c_int, c_int) -> CuResult,
    cu_ctx_create: unsafe extern "C" fn(*mut *mut c_void, c_uint, c_int) -> CuResult,
    cu_ctx_destroy: unsafe extern "C" fn(*mut c_void) -> CuResult,
    cu_ctx_push: unsafe extern "C" fn(*mut c_void) -> CuResult,
    cu_ctx_pop: unsafe extern "C" fn(*mut *mut c_void) -> CuResult,
    cu_memcpy2d: unsafe extern "C" fn(*const u8) -> CuResult,
    create_parser: unsafe extern "C" fn(*mut *mut c_void, *mut u8) -> CuResult,
    parse: unsafe extern "C" fn(*mut c_void, *mut u8) -> CuResult,
    destroy_parser: unsafe extern "C" fn(*mut c_void) -> CuResult,
    create_decoder: unsafe extern "C" fn(*mut *mut c_void, *mut u8) -> CuResult,
    decode_picture: unsafe extern "C" fn(*mut c_void, *mut c_void) -> CuResult,
    map_frame: unsafe extern "C" fn(*mut c_void, c_int, *mut u64, *mut c_uint, *mut u8) -> CuResult,
    unmap_frame: unsafe extern "C" fn(*mut c_void, u64) -> CuResult,
    destroy_decoder: unsafe extern "C" fn(*mut c_void) -> CuResult,
    get_decoder_caps: unsafe extern "C" fn(*mut u8) -> CuResult,
    mem_alloc_pitch: unsafe extern "C" fn(*mut u64, *mut usize, usize, usize, c_uint) -> CuResult,
    mem_free: unsafe extern "C" fn(u64) -> CuResult,
    gl_register_image: unsafe extern "C" fn(*mut *mut c_void, c_uint, c_uint, c_uint) -> CuResult,
    unregister_resource: unsafe extern "C" fn(*mut c_void) -> CuResult,
    map_resources: unsafe extern "C" fn(c_uint, *mut *mut c_void, *mut c_void) -> CuResult,
    unmap_resources: unsafe extern "C" fn(c_uint, *mut *mut c_void, *mut c_void) -> CuResult,
    mapped_array: unsafe extern "C" fn(*mut *mut c_void, *mut c_void, c_uint, c_uint) -> CuResult,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let (cuda_name, cuvid_name) = if cfg!(windows) { ("nvcuda.dll", "nvcuvid.dll") } else { ("libcuda.so.1", "libnvcuvid.so.1") };
        let cuda = Library::new(cuda_name).map_err(|e| format!("no CUDA driver: {e}"))?;
        let cuvid = Library::new(cuvid_name).map_err(|e| format!("no NVDEC library: {e}"))?;
        macro_rules! sym {
            ($l:expr, $n:literal) => {
                *$l.get($n).map_err(|e| format!("symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        let a = Api {
            cu_init: sym!(cuda, b"cuInit\0"),
            cu_device_get: sym!(cuda, b"cuDeviceGet\0"),
            cu_ctx_create: sym!(cuda, b"cuCtxCreate_v2\0"),
            cu_ctx_destroy: sym!(cuda, b"cuCtxDestroy_v2\0"),
            cu_ctx_push: sym!(cuda, b"cuCtxPushCurrent_v2\0"),
            cu_ctx_pop: sym!(cuda, b"cuCtxPopCurrent_v2\0"),
            cu_memcpy2d: sym!(cuda, b"cuMemcpy2D_v2\0"),
            create_parser: sym!(cuvid, b"cuvidCreateVideoParser\0"),
            parse: sym!(cuvid, b"cuvidParseVideoData\0"),
            destroy_parser: sym!(cuvid, b"cuvidDestroyVideoParser\0"),
            create_decoder: sym!(cuvid, b"cuvidCreateDecoder\0"),
            decode_picture: sym!(cuvid, b"cuvidDecodePicture\0"),
            map_frame: sym!(cuvid, b"cuvidMapVideoFrame64\0"),
            unmap_frame: sym!(cuvid, b"cuvidUnmapVideoFrame64\0"),
            destroy_decoder: sym!(cuvid, b"cuvidDestroyDecoder\0"),
            get_decoder_caps: sym!(cuvid, b"cuvidGetDecoderCaps\0"),
            mem_alloc_pitch: sym!(cuda, b"cuMemAllocPitch_v2\0"),
            mem_free: sym!(cuda, b"cuMemFree_v2\0"),
            gl_register_image: sym!(cuda, b"cuGraphicsGLRegisterImage\0"),
            unregister_resource: sym!(cuda, b"cuGraphicsUnregisterResource\0"),
            map_resources: sym!(cuda, b"cuGraphicsMapResources\0"),
            unmap_resources: sym!(cuda, b"cuGraphicsUnmapResources\0"),
            mapped_array: sym!(cuda, b"cuGraphicsSubResourceGetMappedArray\0"),
            _cuda: cuda,
            _cuvid: cuvid,
        };
        let r = (a.cu_init)(0);
        if r != 0 {
            return Err(format!("cuInit failed ({r})"));
        }
        Ok(a)
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

fn ck(r: CuResult, what: &str) -> Result<()> {
    if r != 0 {
        bail!("{what} failed (CUDA error {r})");
    }
    Ok(())
}

fn put<T: Copy>(buf: &mut [u8], off: usize, v: T) {
    unsafe { std::ptr::write_unaligned(buf.as_mut_ptr().add(off) as *mut T, v) }
}

fn get<T: Copy>(p: *const u8, off: usize) -> T {
    unsafe { std::ptr::read_unaligned(p.add(off) as *const T) }
}

fn cuvid_codec(c: Codec) -> c_int {
    match c {
        Codec::H264 => 4,
        Codec::Hevc => 8,
        Codec::Av1 => 11,
    }
}

/// CUVIDDECODECREATEINFO. `unsigned long` fields are 8 bytes on Linux and 4 on
/// Windows: `c_ulong` gives the right layout on each.
#[repr(C)]
struct DecodeCreateInfo {
    ul_width: c_ulong,
    ul_height: c_ulong,
    ul_num_decode_surfaces: c_ulong,
    codec_type: c_int,
    chroma_format: c_int,
    ul_creation_flags: c_ulong,
    bit_depth_minus8: c_ulong,
    ul_intra_decode_only: c_ulong,
    ul_max_width: c_ulong,
    ul_max_height: c_ulong,
    reserved1: c_ulong,
    display_area: Rect16,
    output_format: c_int,
    deinterlace_mode: c_int,
    ul_target_width: c_ulong,
    ul_target_height: c_ulong,
    ul_num_output_surfaces: c_ulong,
    vid_lock: *mut c_void,
    target_rect: Rect16,
    enable_histogram: c_ulong,
    reserved2: [c_ulong; 4],
}

#[repr(C)]
#[derive(Default)]
struct Rect16 {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

/// CUVIDSOURCEDATAPACKET (same `unsigned long` caveat).
#[repr(C)]
struct SourceDataPacket {
    flags: c_ulong,
    payload_size: c_ulong,
    payload: *const u8,
    timestamp: i64,
}

/// A CUDA context, destroyed when the decoder and every picture it handed out are gone.
struct CudaCtx(*mut c_void);

unsafe impl Send for CudaCtx {}
unsafe impl Sync for CudaCtx {}

impl Drop for CudaCtx {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.cu_ctx_destroy)(self.0) };
        }
    }
}

/// Runs `f` with the context current on this thread.
fn with_ctx<T>(ctx: &CudaCtx, f: impl FnOnce(&Api) -> T) -> Result<T> {
    let a = api()?;
    unsafe {
        ck((a.cu_ctx_push)(ctx.0), "cuCtxPushCurrent")?;
        let r = f(a);
        let mut tmp = std::ptr::null_mut();
        (a.cu_ctx_pop)(&mut tmp);
        Ok(r)
    }
}

/// NV12 picture in video memory: luma rows, then interleaved chroma rows.
struct CudaBuf {
    ctx: Arc<CudaCtx>,
    dptr: u64,
    pitch: usize,
    width: usize,
    height: usize,
}

impl Drop for CudaBuf {
    fn drop(&mut self) {
        let d = self.dptr;
        let _ = with_ctx(&self.ctx, |a| unsafe { (a.mem_free)(d) });
    }
}

/// A decoded picture kept on the GPU (zero-copy): copy it into GL textures
/// with [`CudaGlTarget`]. Its buffer returns to the decoder's pool on drop.
pub struct CudaFrame {
    buf: Arc<CudaBuf>,
    pub width: usize,
    pub height: usize,
    pub csc: crate::frame::Csc,
}

/// Fill a CUDA_MEMCPY2D (128 bytes, Linux 64-bit layout) and run it.
#[allow(clippy::too_many_arguments)]
unsafe fn memcpy2d(a: &Api, src: u64, src_pitch: usize, dst_type: u32, dst: u64, dst_pitch: usize, width: usize, rows: usize) -> CuResult {
    let mut m = [0u8; 128];
    put::<u32>(&mut m, 16, 2); // srcMemoryType = device
    put::<u64>(&mut m, 32, src); // srcDevice
    put::<u64>(&mut m, 48, src_pitch as u64);
    put::<u32>(&mut m, 72, dst_type); // 1 host, 2 device, 3 array
    match dst_type {
        1 => put::<u64>(&mut m, 80, dst),
        2 => put::<u64>(&mut m, 88, dst),
        _ => put::<u64>(&mut m, 96, dst),
    }
    put::<u64>(&mut m, 104, dst_pitch as u64);
    put::<u64>(&mut m, 112, width as u64);
    put::<u64>(&mut m, 120, rows as u64);
    (a.cu_memcpy2d)(m.as_ptr())
}

/// GL textures (`R8` luma, `RG8` chroma, sized for the picture) registered
/// with CUDA, to receive [`CudaFrame`]s without going through system memory.
/// Create and use it with the GL context current.
pub struct CudaGlTarget {
    ctx: Arc<CudaCtx>,
    res: [*mut c_void; 2],
    pub textures: [u32; 2],
    pub size: [usize; 2],
}

unsafe impl Send for CudaGlTarget {}

impl CudaGlTarget {
    const GL_TEXTURE_2D: c_uint = 0x0DE1;
    const WRITE_DISCARD: c_uint = 2;

    pub fn register(frame: &CudaFrame, tex_y: u32, tex_uv: u32) -> Result<Self> {
        let mut res = [std::ptr::null_mut(); 2];
        with_ctx(&frame.buf.ctx, |a| unsafe {
            for (r, t) in res.iter_mut().zip([tex_y, tex_uv]) {
                ck((a.gl_register_image)(r, t, Self::GL_TEXTURE_2D, Self::WRITE_DISCARD), "cuGraphicsGLRegisterImage")?;
            }
            Ok::<_, anyhow::Error>(())
        })??;
        Ok(Self { ctx: frame.buf.ctx.clone(), res, textures: [tex_y, tex_uv], size: [frame.width, frame.height] })
    }

    /// Usable for this frame (same decoder context and size)?
    pub fn fits(&self, frame: &CudaFrame) -> bool {
        Arc::ptr_eq(&self.ctx, &frame.buf.ctx) && self.size == [frame.width, frame.height]
    }

    pub fn copy(&mut self, frame: &CudaFrame) -> Result<()> {
        let b = &frame.buf;
        let (w, h) = (frame.width, frame.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut res = self.res;
        with_ctx(&self.ctx, |a| unsafe {
            ck((a.map_resources)(2, res.as_mut_ptr(), std::ptr::null_mut()), "cuGraphicsMapResources")?;
            let r = (|| {
                for (i, (src, width, rows)) in [(b.dptr, w, h), (b.dptr + (b.pitch * b.height) as u64, cw * 2, ch)].into_iter().enumerate() {
                    let mut arr = std::ptr::null_mut();
                    ck((a.mapped_array)(&mut arr, res[i], 0, 0), "cuGraphicsSubResourceGetMappedArray")?;
                    ck(memcpy2d(a, src, b.pitch, 3, arr as u64, 0, width, rows), "cuMemcpy2D (to texture)")?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            (a.unmap_resources)(2, res.as_mut_ptr(), std::ptr::null_mut());
            r
        })?
    }
}

impl Drop for CudaGlTarget {
    fn drop(&mut self) {
        let res = self.res;
        let _ = with_ctx(&self.ctx, |a| unsafe {
            for r in res {
                if !r.is_null() {
                    (a.unregister_resource)(r);
                }
            }
        });
    }
}

/// Decoder state shared with the parser callbacks.
struct Inner {
    codec: Codec,
    decoder: *mut c_void,
    width: usize,
    height: usize,
    surface_height: usize,
    /// Stream is 4:4:4 (three full-size planes) rather than NV12.
    yuv444: bool,
    /// 10-bit stream (P016 output, 4:2:0 only).
    bit10: bool,
    out16: crate::hdr::Yuv16Frame,
    /// The last picture is in `out16`.
    ready16: bool,
    p016: Vec<u16>,
    surfaces: u32,
    out: I420Frame,
    ready: bool,
    error: Option<String>,
    nv12: Vec<u8>,
    ctx: Arc<CudaCtx>,
    /// Zero-copy: keep pictures on the GPU while true.
    zero_copy: Option<Arc<AtomicBool>>,
    pool: Vec<Arc<CudaBuf>>,
    gpu_out: Option<CudaFrame>,
}

impl Inner {
    /// A pool buffer nobody else holds (the renderer may still own the last one).
    unsafe fn free_buffer(&mut self, a: &Api, w: usize, h: usize) -> Option<Arc<CudaBuf>> {
        self.pool.retain(|b| b.width == w && b.height == h);
        if let Some(b) = self.pool.iter().find(|b| Arc::strong_count(b) == 1) {
            return Some(b.clone());
        }
        if self.pool.len() >= 4 {
            return None;
        }
        let (mut dptr, mut pitch) = (0u64, 0usize);
        if (a.mem_alloc_pitch)(&mut dptr, &mut pitch, w.div_ceil(2) * 2, h + h.div_ceil(2), 16) != 0 {
            return None;
        }
        let b = Arc::new(CudaBuf { ctx: self.ctx.clone(), dptr, pitch, width: w, height: h });
        self.pool.push(b.clone());
        Some(b)
    }
}

pub struct NvDecoder {
    ctx: Arc<CudaCtx>,
    parser: *mut c_void,
    inner: Box<Inner>,
}

unsafe impl Send for NvDecoder {}

unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut c_void) -> c_int {
    let s = &mut *(user as *mut Inner);
    let fmt = fmt as *const u8;
    let a = match api() {
        Ok(a) => a,
        Err(_) => return 0,
    };
    let coded_w: u32 = get(fmt, 16);
    let coded_h: u32 = get(fmt, 20);
    let (l, t, r, b): (i32, i32, i32, i32) = (get(fmt, 24), get(fmt, 28), get(fmt, 32), get(fmt, 36));
    let chroma: c_int = get(fmt, 40);
    let depth: u8 = get(fmt, 13);
    let min_surfaces: u8 = get(fmt, 15);
    let (w, h) = ((r - l).max(2) as usize, (b - t).max(2) as usize);
    if !(depth == 0 && (chroma == 1 || chroma == 3)) && !(depth == 2 && chroma == 1) {
        s.error = Some(format!("unsupported stream (bit depth {}, chroma format {chroma})", depth + 8));
        return 0;
    }
    let surfaces = (min_surfaces as u32).max(4) + 2;
    if !s.decoder.is_null() {
        if s.width == w && s.height == h && s.yuv444 == (chroma == 3) && s.bit10 == (depth == 2) {
            return surfaces as c_int;
        }
        (a.destroy_decoder)(s.decoder);
        s.decoder = std::ptr::null_mut();
    }
    // CUVIDDECODECREATEINFO (Linux, unsigned long = 8 bytes)
    let rect = Rect16 { left: l as i16, top: t as i16, right: r as i16, bottom: b as i16 };
    let mut ci = DecodeCreateInfo {
        ul_width: coded_w as c_ulong,
        ul_height: coded_h as c_ulong,
        ul_num_decode_surfaces: surfaces as c_ulong,
        codec_type: cuvid_codec(s.codec),
        chroma_format: chroma,
        ul_creation_flags: 4, // PreferCUVID
        bit_depth_minus8: depth as c_ulong,
        ul_intra_decode_only: 0,
        ul_max_width: coded_w as c_ulong,
        ul_max_height: coded_h as c_ulong,
        reserved1: 0,
        display_area: rect,
        output_format: if chroma == 3 { 2 } else if depth == 2 { 1 } else { 0 }, // NV12 / P016 / YUV444
        deinterlace_mode: 0, // weave
        ul_target_width: w as c_ulong,
        ul_target_height: h as c_ulong,
        ul_num_output_surfaces: 2,
        vid_lock: std::ptr::null_mut(),
        target_rect: Rect16::default(),
        enable_histogram: 0,
        reserved2: [0; 4],
    };
    let r = (a.create_decoder)(&mut s.decoder, &mut ci as *mut DecodeCreateInfo as *mut u8);
    if r != 0 {
        s.error = Some(format!("cuvidCreateDecoder failed ({r})"));
        s.decoder = std::ptr::null_mut();
        return 0;
    }
    s.width = w;
    s.height = h;
    s.surface_height = h;
    s.yuv444 = chroma == 3;
    s.bit10 = depth == 2;
    s.surfaces = surfaces;
    info!("NVDEC {:?} decoder {w}x{h} ({surfaces} surfaces)", s.codec);
    surfaces as c_int
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut c_void) -> c_int {
    let s = &mut *(user as *mut Inner);
    let Ok(a) = api() else { return 0 };
    if s.decoder.is_null() {
        return 0;
    }
    let r = (a.decode_picture)(s.decoder, pic);
    if r != 0 {
        s.error = Some(format!("cuvidDecodePicture failed ({r})"));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, info: *mut c_void) -> c_int {
    let s = &mut *(user as *mut Inner);
    let Ok(a) = api() else { return 0 };
    if info.is_null() || s.decoder.is_null() {
        return 1;
    }
    let idx: c_int = get(info as *const u8, 0);
    // CUVIDPROCPARAMS: progressive_frame = 1, the rest zero.
    let mut pp = [0u8; 512];
    put::<c_int>(&mut pp, 0, 1);
    let mut dptr: u64 = 0;
    let mut pitch: c_uint = 0;
    let r = (a.map_frame)(s.decoder, idx, &mut dptr, &mut pitch, pp.as_mut_ptr());
    if r != 0 {
        s.error = Some(format!("cuvidMapVideoFrame failed ({r})"));
        return 0;
    }
    let (w, h) = (s.width, s.height);
    let chroma_src = dptr + pitch as u64 * ((s.surface_height as u64 + 1) & !1);
    s.gpu_out = None;
    s.ready16 = false;
    if s.bit10 {
        // P016: 16-bit luma rows, then 16-bit interleaved chroma rows (no zero-copy).
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (ys, uvs) = (w, cw * 2);
        let mut buf = std::mem::take(&mut s.p016);
        buf.resize(ys * h + uvs * ch, 0);
        let ok = memcpy2d(a, dptr, pitch as usize, 1, buf.as_mut_ptr() as u64, ys * 2, w * 2, h) == 0
            && memcpy2d(a, chroma_src, pitch as usize, 1, buf.as_mut_ptr().add(ys * h) as u64, uvs * 2, uvs * 2, ch) == 0;
        (a.unmap_frame)(s.decoder, dptr);
        if !ok {
            s.p016 = buf;
            s.error = Some("cuMemcpy2D failed".into());
            return 0;
        }
        s.out16.copy_from_p010(w, h, &buf[..ys * h], ys, &buf[ys * h..], uvs);
        s.p016 = buf;
        s.ready = true;
        s.ready16 = true;
        return 1;
    }
    if s.yuv444 {
        // Three full-size planes, one after the other (no zero-copy for 4:4:4).
        if s.out.width != w || s.out.height != h || !s.out.yuv444 {
            let csc = s.out.csc;
            s.out = I420Frame::with_format(w, h, true);
            s.out.csc = csc;
        }
        let plane = pitch as u64 * s.surface_height as u64;
        let mut ok = true;
        for (i, dst) in [s.out.y.as_mut_ptr(), s.out.u.as_mut_ptr(), s.out.v.as_mut_ptr()].into_iter().enumerate() {
            ok &= memcpy2d(a, dptr + plane * i as u64, pitch as usize, 1, dst as u64, w, w, h) == 0;
        }
        (a.unmap_frame)(s.decoder, dptr);
        if !ok {
            s.error = Some("cuMemcpy2D failed".into());
            return 0;
        }
        s.ready = true;
        return 1;
    }
    if s.zero_copy.as_ref().is_some_and(|z| z.load(Ordering::Relaxed)) {
        if let Some(b) = s.free_buffer(a, w, h) {
            let ok = memcpy2d(a, dptr, pitch as usize, 2, b.dptr, b.pitch, w, h) == 0
                && memcpy2d(a, chroma_src, pitch as usize, 2, b.dptr + (b.pitch * h) as u64, b.pitch, w.div_ceil(2) * 2, h.div_ceil(2)) == 0;
            (a.unmap_frame)(s.decoder, dptr);
            if !ok {
                s.error = Some("cuMemcpy2D (device) failed".into());
                return 0;
            }
            s.gpu_out = Some(CudaFrame { buf: b, width: w, height: h, csc: s.out.csc });
            s.ready = true;
            return 1;
        }
    }
    if s.out.width != w || s.out.height != h || s.out.yuv444 {
        let csc = s.out.csc;
        s.out = I420Frame::new(w, h);
        s.out.csc = csc;
    }
    let chroma_rows = h.div_ceil(2);
    let cw = s.out.chroma_width();
    let uv_pitch = cw * 2;
    s.nv12.resize(uv_pitch * chroma_rows, 0);
    let mut ok = true;
    // Luma goes straight into the output frame; interleaved chroma into a scratch buffer.
    let luma_dst = s.out.y.as_mut_ptr();
    let chroma_dst = s.nv12.as_mut_ptr();
    for (src, dst, dst_pitch, width, rows) in [(dptr, luma_dst, w, w, h), (chroma_src, chroma_dst, uv_pitch, uv_pitch.min(pitch as usize), chroma_rows)] {
        // CUDA_MEMCPY2D
        let mut m = [0u8; 128];
        put::<u32>(&mut m, 16, 2); // srcMemoryType = device
        put::<u64>(&mut m, 32, src); // srcDevice
        put::<u64>(&mut m, 48, pitch as u64); // srcPitch
        put::<u32>(&mut m, 72, 1); // dstMemoryType = host
        put::<u64>(&mut m, 80, dst as u64); // dstHost
        put::<u64>(&mut m, 104, dst_pitch as u64); // dstPitch
        put::<u64>(&mut m, 112, width as u64); // WidthInBytes
        put::<u64>(&mut m, 120, rows as u64); // Height
        if (a.cu_memcpy2d)(m.as_ptr()) != 0 {
            ok = false;
        }
    }
    (a.unmap_frame)(s.decoder, dptr);
    if !ok {
        s.error = Some("cuMemcpy2D failed".into());
        return 0;
    }
    // Deinterleave NV12 chroma -> I420 U/V planes.
    let (out_u, out_v) = (&mut s.out.u, &mut s.out.v);
    for ((src, du), dv) in s.nv12.chunks_exact(uv_pitch).zip(out_u.chunks_exact_mut(cw)).zip(out_v.chunks_exact_mut(cw)) {
        for ((pair, u), v) in src.chunks_exact(2).zip(du.iter_mut()).zip(dv.iter_mut()) {
            *u = pair[0];
            *v = pair[1];
        }
    }
    s.ready = true;
    1
}

impl NvDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        let a = api()?;
        unsafe {
            let mut dev = 0;
            ck((a.cu_device_get)(&mut dev, 0), "cuDeviceGet")?;
            let mut raw = std::ptr::null_mut();
            ck((a.cu_ctx_create)(&mut raw, 0, dev), "cuCtxCreate")?;
            let mut tmp = std::ptr::null_mut();
            (a.cu_ctx_pop)(&mut tmp); // made current by cuCtxCreate; pushed explicitly when used
            let ctx = Arc::new(CudaCtx(raw));
            let mut inner = Box::new(Inner {
                codec,
                decoder: std::ptr::null_mut(),
                width: 0,
                height: 0,
                surface_height: 0,
                yuv444: false,
                bit10: false,
                out16: crate::hdr::Yuv16Frame::new(2, 2),
                ready16: false,
                p016: Vec::new(),
                surfaces: 0,
                out: I420Frame::new(2, 2),
                ready: false,
                error: None,
                nv12: Vec::new(),
                ctx: ctx.clone(),
                zero_copy: None,
                pool: Vec::new(),
                gpu_out: None,
            });
            // CUVIDPARSERPARAMS
            let mut pp = vec![0u8; 256];
            put::<c_int>(&mut pp, 0, cuvid_codec(codec)); // CodecType
            put::<c_uint>(&mut pp, 4, 1); // ulMaxNumDecodeSurfaces (raised by the sequence callback)
            put::<c_uint>(&mut pp, 12, 100); // ulErrorThreshold: decode even damaged pictures
            put::<c_uint>(&mut pp, 16, 0); // ulMaxDisplayDelay: lowest latency
            put::<u64>(&mut pp, 40, &mut *inner as *mut Inner as u64); // pUserData
            put::<u64>(&mut pp, 48, on_sequence as *const () as u64);
            put::<u64>(&mut pp, 56, on_decode as *const () as u64);
            put::<u64>(&mut pp, 64, on_display as *const () as u64);
            let mut parser = std::ptr::null_mut();
            ck((a.cu_ctx_push)(ctx.0), "cuCtxPushCurrent")?;
            let r = (a.create_parser)(&mut parser, pp.as_mut_ptr());
            (a.cu_ctx_pop)(&mut tmp);
            if r != 0 {
                bail!("cuvidCreateVideoParser failed ({r})");
            }
            debug!("NVDEC parser ready for {codec:?}");
            Ok(Self { ctx, parser, inner })
        }
    }

    /// Decode one complete access unit / temporal unit.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>> {
        let a = api()?;
        self.inner.ready = false;
        self.inner.error = None;
        // CUVIDSOURCEDATAPACKET: flags (ENDOFPICTURE), payload_size, payload, timestamp
        let mut pkt = SourceDataPacket { flags: 8, payload_size: data.len() as c_ulong, payload: data.as_ptr(), timestamp: 0 }; // CUVID_PKT_ENDOFPICTURE
        unsafe {
            ck((a.cu_ctx_push)(self.ctx.0), "cuCtxPushCurrent")?;
            let r = (a.parse)(self.parser, &mut pkt as *mut SourceDataPacket as *mut u8);
            let mut tmp = std::ptr::null_mut();
            (a.cu_ctx_pop)(&mut tmp);
            if r != 0 {
                bail!("cuvidParseVideoData failed ({r})");
            }
        }
        if let Some(e) = self.inner.error.take() {
            bail!("{e}");
        }
        Ok(if self.inner.ready && self.inner.gpu_out.is_none() && !self.inner.ready16 { Some(&self.inner.out) } else { None })
    }

    /// Keep decoded pictures on the GPU ([`Self::take_gpu_picture`]) while `flag` is true.
    pub fn set_zero_copy(&mut self, flag: Option<Arc<AtomicBool>>) {
        self.inner.zero_copy = flag;
    }

    /// The picture from the last [`Self::decode`] if it stayed on the GPU.
    pub fn take_gpu_picture(&mut self) -> Option<CudaFrame> {
        self.inner.gpu_out.take()
    }

    /// The 10-bit picture of the last decode, if the stream is 10-bit.
    pub fn take_picture16(&mut self) -> Option<&crate::hdr::Yuv16Frame> {
        if self.inner.ready && self.inner.ready16 {
            self.inner.ready16 = false;
            Some(&self.inner.out16)
        } else {
            None
        }
    }

    /// True if this GPU decodes `codec` in 10-bit 4:2:0.
    pub fn supports_10bit(codec: Codec) -> bool {
        supports_caps(codec, 1, 2)
    }

    /// The CPU picture of the last decode (after [`Self::decode`] returned one).
    pub fn decode_last(&self) -> &I420Frame {
        &self.inner.out
    }

    pub fn zero_copy_active(&self) -> bool {
        self.inner.zero_copy.as_ref().is_some_and(|z| z.load(Ordering::Relaxed))
    }
}

impl Drop for NvDecoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.cu_ctx_push)(self.ctx.0);
                (a.destroy_parser)(self.parser);
                if !self.inner.decoder.is_null() {
                    (a.destroy_decoder)(self.inner.decoder);
                }
                let mut tmp = std::ptr::null_mut();
                (a.cu_ctx_pop)(&mut tmp);
                // The context goes when the last CudaFrame / buffer using it does.
            }
        }
    }
}

/// True if this GPU's NVDEC decodes `codec` in 8-bit 4:4:4 (HEVC on Turing and later).
pub fn supports_444(codec: Codec) -> bool {
    // Probing opens a decoder (~0.3 s): do it once per codec.
    static CACHE: std::sync::Mutex<Vec<(Codec, bool)>> = std::sync::Mutex::new(Vec::new());
    if let Some(&(_, r)) = CACHE.lock().unwrap().iter().find(|(c, _)| *c == codec) {
        return r;
    }
    let r = probe_444(codec);
    CACHE.lock().unwrap().push((codec, r));
    r
}

fn probe_444(codec: Codec) -> bool {
    probe_caps(codec, 3, 0)
}

/// Cached decoder capability query: `chroma` (1 = 4:2:0, 3 = 4:4:4), `depth_minus8`.
fn supports_caps(codec: Codec, chroma: c_int, depth_minus8: c_uint) -> bool {
    static CACHE: std::sync::Mutex<Vec<(Codec, c_int, c_uint, bool)>> = std::sync::Mutex::new(Vec::new());
    if let Some(&(.., r)) = CACHE.lock().unwrap().iter().find(|(c, ch, d, _)| *c == codec && *ch == chroma && *d == depth_minus8) {
        return r;
    }
    let r = probe_caps(codec, chroma, depth_minus8);
    CACHE.lock().unwrap().push((codec, chroma, depth_minus8, r));
    r
}

fn probe_caps(codec: Codec, chroma: c_int, depth_minus8: c_uint) -> bool {
    let Ok(d) = NvDecoder::new(codec) else { return false };
    with_ctx(&d.ctx, |a| unsafe {
        // CUVIDDECODECAPS (88 bytes): codec, chroma format, bit depth - 8, ..., bIsSupported at 24.
        let mut caps = [0u8; 128];
        put::<c_int>(&mut caps, 0, cuvid_codec(codec));
        put::<c_int>(&mut caps, 4, chroma);
        put::<c_uint>(&mut caps, 8, depth_minus8);
        (a.get_decoder_caps)(caps.as_mut_ptr()) == 0 && caps[24] != 0
    })
    .unwrap_or(false)
}

/// True if NVDEC can be initialised on this machine.
pub fn available() -> bool {
    NvDecoder::new(Codec::H264).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Linux the structs keep the layout of nvcuvid's headers (8-byte longs).
    #[cfg(target_os = "linux")]
    #[test]
    fn nvdec_struct_layout_linux() {
        let ci = std::mem::MaybeUninit::<DecodeCreateInfo>::uninit();
        let base = ci.as_ptr() as usize;
        let off = |p: *const u8| p as usize - base;
        unsafe {
            let p = ci.as_ptr();
            assert_eq!(off(std::ptr::addr_of!((*p).codec_type) as *const u8), 24);
            assert_eq!(off(std::ptr::addr_of!((*p).bit_depth_minus8) as *const u8), 40);
            assert_eq!(off(std::ptr::addr_of!((*p).display_area) as *const u8), 80);
            assert_eq!(off(std::ptr::addr_of!((*p).output_format) as *const u8), 88);
            assert_eq!(off(std::ptr::addr_of!((*p).ul_num_output_surfaces) as *const u8), 112);
        }
        assert_eq!(std::mem::size_of::<SourceDataPacket>(), 32);
    }

    /// Headless OpenGL context on the NVIDIA GPU (EGL device platform), for tests.
    struct HeadlessGl {
        _lib: Library,
        gp: unsafe extern "C" fn(*const std::ffi::c_char) -> *mut c_void,
    }

    impl HeadlessGl {
        fn new() -> Option<Self> {
            unsafe {
                let lib = Library::new("libEGL.so.1").ok()?;
                let gp: unsafe extern "C" fn(*const std::ffi::c_char) -> *mut c_void = *lib.get(b"eglGetProcAddress\0").ok()?;
                let f = |n: &[u8]| gp(n.as_ptr() as *const _);
                let query_devices: unsafe extern "C" fn(c_int, *mut *mut c_void, *mut c_int) -> c_uint = std::mem::transmute(f(b"eglQueryDevicesEXT\0").as_ref()?);
                let platform_display: unsafe extern "C" fn(c_uint, *mut c_void, *const c_int) -> *mut c_void = std::mem::transmute(f(b"eglGetPlatformDisplayEXT\0").as_ref()?);
                let initialize: unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_uint = *lib.get(b"eglInitialize\0").ok()?;
                let bind_api: unsafe extern "C" fn(c_uint) -> c_uint = *lib.get(b"eglBindAPI\0").ok()?;
                let choose: unsafe extern "C" fn(*mut c_void, *const c_int, *mut *mut c_void, c_int, *mut c_int) -> c_uint = *lib.get(b"eglChooseConfig\0").ok()?;
                let create_ctx: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_int) -> *mut c_void = *lib.get(b"eglCreateContext\0").ok()?;
                let make_current: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> c_uint = *lib.get(b"eglMakeCurrent\0").ok()?;
                let mut devs = [std::ptr::null_mut(); 8];
                let mut n = 0;
                if query_devices(8, devs.as_mut_ptr(), &mut n) == 0 || n == 0 {
                    return None;
                }
                for dev in &devs[..n as usize] {
                    let dpy = platform_display(0x313F, *dev, [0x3038].as_ptr()); // EGL_PLATFORM_DEVICE_EXT
                    let (mut ma, mut mi) = (0, 0);
                    if dpy.is_null() || initialize(dpy, &mut ma, &mut mi) == 0 || bind_api(0x30A2) == 0 {
                        continue;
                    }
                    // EGL_SURFACE_TYPE: PBUFFER, EGL_RENDERABLE_TYPE: OPENGL
                    let attrs = [0x3033, 0x0001, 0x3040, 0x0008, 0x3038];
                    let mut cfg = std::ptr::null_mut();
                    let mut num = 0;
                    if choose(dpy, attrs.as_ptr(), &mut cfg, 1, &mut num) == 0 || num == 0 {
                        continue;
                    }
                    let ctx = create_ctx(dpy, cfg, std::ptr::null_mut(), [0x3038].as_ptr());
                    if !ctx.is_null() && make_current(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) != 0 {
                        return Some(Self { _lib: lib, gp });
                    }
                }
                None
            }
        }

        unsafe fn f<T>(&self, name: &[u8]) -> T {
            let p = (self.gp)(name.as_ptr() as *const _);
            assert!(!p.is_null(), "no GL function {}", String::from_utf8_lossy(name));
            std::mem::transmute_copy(&p)
        }
    }

    /// NVENC -> NVDEC kept on the GPU -> CUDA copy into GL textures -> read back.
    #[test]
    fn nvdec_zero_copy_into_gl_textures() {
        if !available() {
            eprintln!("NVDEC unavailable, skipping");
            return;
        }
        let Some(gl) = HeadlessGl::new() else {
            eprintln!("no headless EGL/OpenGL context, skipping");
            return;
        };
        let (w, h) = (1280usize, 720usize);
        let mut enc = crate::nvenc::NvencEncoder::new(Codec::Hevc, w, h, 60, 8000, &Default::default(), 1).unwrap();
        let mut dec = NvDecoder::new(Codec::Hevc).unwrap();
        dec.set_zero_copy(Some(Arc::new(AtomicBool::new(true))));
        let mut f = I420Frame::new(w, h);
        for (k, p) in f.y.iter_mut().enumerate() {
            *p = (16 + (k % w / 6 + k / w / 4) % 200) as u8;
        }
        for (k, p) in f.u.iter_mut().enumerate() {
            *p = (60 + (k % (w / 2)) / 8 % 120) as u8;
        }
        for (k, p) in f.v.iter_mut().enumerate() {
            *p = (90 + (k / (w / 2)) / 4 % 100) as u8;
        }
        let mut frame = None;
        for i in 1..=3u64 {
            let data = enc.encode(&f, i).unwrap().data.to_vec();
            assert!(dec.decode(&data).unwrap().is_none(), "zero-copy decode must not return a CPU picture");
            frame = dec.take_gpu_picture().or(frame);
        }
        let frame = frame.expect("no GPU picture");
        unsafe {
            let gen: unsafe extern "C" fn(c_int, *mut c_uint) = gl.f(b"glGenTextures\0");
            let bind: unsafe extern "C" fn(c_uint, c_uint) = gl.f(b"glBindTexture\0");
            let image: unsafe extern "C" fn(c_uint, c_int, c_int, c_int, c_int, c_int, c_uint, c_uint, *const c_void) = gl.f(b"glTexImage2D\0");
            let get: unsafe extern "C" fn(c_uint, c_int, c_uint, c_uint, *mut c_void) = gl.f(b"glGetTexImage\0");
            let store: unsafe extern "C" fn(c_uint, c_int) = gl.f(b"glPixelStorei\0");
            let finish: unsafe extern "C" fn() = gl.f(b"glFinish\0");
            let mut tex = [0u32; 2];
            gen(2, tex.as_mut_ptr());
            bind(0x0DE1, tex[0]);
            image(0x0DE1, 0, 0x8229, w as c_int, h as c_int, 0, 0x1903, 0x1401, std::ptr::null()); // R8
            bind(0x0DE1, tex[1]);
            image(0x0DE1, 0, 0x822B, (w / 2) as c_int, (h / 2) as c_int, 0, 0x8227, 0x1401, std::ptr::null()); // RG8
            bind(0x0DE1, 0);
            let mut target = CudaGlTarget::register(&frame, tex[0], tex[1]).unwrap();
            assert!(target.fits(&frame));
            target.copy(&frame).unwrap();
            finish();
            store(0x0D05, 1); // GL_PACK_ALIGNMENT
            let mut y = vec![0u8; w * h];
            let mut uv = vec![0u8; w * h / 2];
            bind(0x0DE1, tex[0]);
            get(0x0DE1, 0, 0x1903, 0x1401, y.as_mut_ptr() as *mut c_void);
            bind(0x0DE1, tex[1]);
            get(0x0DE1, 0, 0x8227, 0x1401, uv.as_mut_ptr() as *mut c_void);
            let ey = y.iter().zip(&f.y).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / (w * h) as u64;
            let eu = uv.chunks_exact(2).zip(&f.u).map(|(p, b)| (p[0] as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / f.u.len() as u64;
            let ev = uv.chunks_exact(2).zip(&f.v).map(|(p, b)| (p[1] as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / f.v.len() as u64;
            eprintln!("zero-copy GL textures: mean error Y {ey}, U {eu}, V {ev}");
            assert!(ey <= 4 && eu <= 4 && ev <= 4, "texture content differs from the source picture");
        }
    }
    use crate::ffenc::FfmpegEncoder;

    /// NVENC -> NVDEC roundtrip for every codec (skipped without an NVIDIA GPU).
    #[test]
    fn nvdec_roundtrip_all_codecs() {
        if !available() {
            eprintln!("NVDEC unavailable, skipping");
            return;
        }
        let (w, h) = (1280, 720);
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let Ok(mut enc) = FfmpegEncoder::new(codec.nvenc(), w, h, 60, 8000) else {
                eprintln!("{codec:?}: no encoder, skipping");
                continue;
            };
            let mut dec = NvDecoder::new(codec).unwrap();
            let mut f = I420Frame::new(w, h);
            let mut decoded = 0;
            let mut err_sum = 0u64;
            let t = std::time::Instant::now();
            for i in 0..60usize {
                for (k, p) in f.y.iter_mut().enumerate() {
                    *p = (16 + (k % w / 4 + i * 2) % 200) as u8;
                }
                let data = enc.encode(&f, i as i64 * 1500).unwrap().data.to_vec();
                if let Some(out) = dec.decode(&data).unwrap() {
                    assert_eq!((out.width, out.height), (w, h), "{codec:?}");
                    err_sum += out.y.iter().zip(&f.y).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / (w * h) as u64;
                    decoded += 1;
                }
            }
            eprintln!("{codec:?}: {decoded}/60 decoded in {:?}, mean luma error {}", t.elapsed(), err_sum / decoded.max(1) as u64);
            assert!(decoded >= 58, "{codec:?} decoded only {decoded}");
            assert!(err_sum / (decoded as u64) <= 6, "{codec:?} picture mismatch");
        }
    }
}
