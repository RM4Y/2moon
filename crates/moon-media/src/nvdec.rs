//! NVIDIA hardware decoding (NVDEC) for H.264, HEVC and AV1, using the
//! CUDA driver API and the NVCUVID parser, both loaded at runtime.
//!
//! Only a handful of plain-data structures are needed; they are written into
//! zeroed byte buffers at their documented offsets so reserved fields stay 0.

use std::ffi::{c_int, c_uint, c_void};
use std::sync::OnceLock;

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
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let cuda = Library::new("libcuda.so.1").map_err(|e| format!("no CUDA driver: {e}"))?;
        let cuvid = Library::new("libnvcuvid.so.1").map_err(|e| format!("no NVDEC library: {e}"))?;
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

/// Decoder state shared with the parser callbacks.
struct Inner {
    codec: Codec,
    decoder: *mut c_void,
    width: usize,
    height: usize,
    surface_height: usize,
    surfaces: u32,
    out: I420Frame,
    ready: bool,
    error: Option<String>,
    nv12: Vec<u8>,
}

pub struct NvDecoder {
    ctx: *mut c_void,
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
    if depth != 0 || chroma != 1 {
        s.error = Some(format!("unsupported stream (bit depth {}, chroma format {chroma})", depth + 8));
        return 0;
    }
    let surfaces = (min_surfaces as u32).max(4) + 2;
    if !s.decoder.is_null() {
        if s.width == w && s.height == h {
            return surfaces as c_int;
        }
        (a.destroy_decoder)(s.decoder);
        s.decoder = std::ptr::null_mut();
    }
    // CUVIDDECODECREATEINFO (Linux, unsigned long = 8 bytes)
    let mut ci = vec![0u8; 512];
    put::<u64>(&mut ci, 0, coded_w as u64); // ulWidth
    put::<u64>(&mut ci, 8, coded_h as u64); // ulHeight
    put::<u64>(&mut ci, 16, surfaces as u64); // ulNumDecodeSurfaces
    put::<c_int>(&mut ci, 24, cuvid_codec(s.codec)); // CodecType
    put::<c_int>(&mut ci, 28, 1); // ChromaFormat 4:2:0
    put::<u64>(&mut ci, 32, 4); // ulCreationFlags = PreferCUVID
    put::<u64>(&mut ci, 40, 0); // bitDepthMinus8
    put::<u64>(&mut ci, 56, coded_w as u64); // ulMaxWidth
    put::<u64>(&mut ci, 64, coded_h as u64); // ulMaxHeight
    put::<i16>(&mut ci, 80, l as i16); // display_area
    put::<i16>(&mut ci, 82, t as i16);
    put::<i16>(&mut ci, 84, r as i16);
    put::<i16>(&mut ci, 86, b as i16);
    put::<c_int>(&mut ci, 88, 0); // OutputFormat NV12
    put::<c_int>(&mut ci, 92, 0); // DeinterlaceMode weave
    put::<u64>(&mut ci, 96, w as u64); // ulTargetWidth
    put::<u64>(&mut ci, 104, h as u64); // ulTargetHeight
    put::<u64>(&mut ci, 112, 2); // ulNumOutputSurfaces
    let r = (a.create_decoder)(&mut s.decoder, ci.as_mut_ptr());
    if r != 0 {
        s.error = Some(format!("cuvidCreateDecoder failed ({r})"));
        s.decoder = std::ptr::null_mut();
        return 0;
    }
    s.width = w;
    s.height = h;
    s.surface_height = h;
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
    if s.out.width != w || s.out.height != h {
        s.out = I420Frame::new(w, h);
    }
    let chroma_rows = h.div_ceil(2);
    let cw = s.out.chroma_width();
    let uv_pitch = cw * 2;
    s.nv12.resize(uv_pitch * chroma_rows, 0);
    let chroma_src = dptr + pitch as u64 * ((s.surface_height as u64 + 1) & !1);
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
            let mut ctx = std::ptr::null_mut();
            ck((a.cu_ctx_create)(&mut ctx, 0, dev), "cuCtxCreate")?;
            let mut inner = Box::new(Inner {
                codec,
                decoder: std::ptr::null_mut(),
                width: 0,
                height: 0,
                surface_height: 0,
                surfaces: 0,
                out: I420Frame::new(2, 2),
                ready: false,
                error: None,
                nv12: Vec::new(),
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
            ck((a.cu_ctx_push)(ctx), "cuCtxPushCurrent")?;
            let r = (a.create_parser)(&mut parser, pp.as_mut_ptr());
            let mut tmp = std::ptr::null_mut();
            (a.cu_ctx_pop)(&mut tmp);
            if r != 0 {
                (a.cu_ctx_destroy)(ctx);
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
        let mut pkt = [0u8; 32];
        put::<u64>(&mut pkt, 0, 8);
        put::<u64>(&mut pkt, 8, data.len() as u64);
        put::<u64>(&mut pkt, 16, data.as_ptr() as u64);
        unsafe {
            ck((a.cu_ctx_push)(self.ctx), "cuCtxPushCurrent")?;
            let r = (a.parse)(self.parser, pkt.as_mut_ptr());
            let mut tmp = std::ptr::null_mut();
            (a.cu_ctx_pop)(&mut tmp);
            if r != 0 {
                bail!("cuvidParseVideoData failed ({r})");
            }
        }
        if let Some(e) = self.inner.error.take() {
            bail!("{e}");
        }
        Ok(if self.inner.ready { Some(&self.inner.out) } else { None })
    }
}

impl Drop for NvDecoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.cu_ctx_push)(self.ctx);
                (a.destroy_parser)(self.parser);
                if !self.inner.decoder.is_null() {
                    (a.destroy_decoder)(self.inner.decoder);
                }
                let mut tmp = std::ptr::null_mut();
                (a.cu_ctx_pop)(&mut tmp);
                (a.cu_ctx_destroy)(self.ctx);
            }
        }
    }
}

/// True if NVDEC can be initialised on this machine.
pub fn available() -> bool {
    NvDecoder::new(Codec::H264).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
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
