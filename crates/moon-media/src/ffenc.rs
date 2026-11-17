//! Hardware video encoding through the system FFmpeg (e.g. `h264_nvenc`),
//! loaded at runtime. Encoder parameters are set through the AVOptions API
//! (by name), so no FFmpeg headers or struct layouts are needed except a few
//! AVFrame/AVPacket fields whose offsets are discovered at runtime.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use libloading::Library;
use tracing::{debug, info};

use crate::frame::I420Frame;
use crate::h264::EncodedFrame;

const AVERROR_EAGAIN: c_int = -11;
const AV_OPT_SEARCH_CHILDREN: c_int = 1;
const AV_PICTURE_TYPE_NONE: i32 = 0;
const AV_PICTURE_TYPE_I: i32 = 1;
const AV_PKT_FLAG_KEY: i32 = 1;
const AV_NOPTS_VALUE: i64 = i64::MIN;

struct Api {
    _avutil: Library,
    _avcodec: Library,
    find_encoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void,
    find_decoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void,
    send_packet: unsafe extern "C" fn(*mut c_void, *const c_void) -> c_int,
    receive_frame: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    free_context: unsafe extern "C" fn(*mut *mut c_void),
    open2: unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> c_int,
    send_frame: unsafe extern "C" fn(*mut c_void, *const c_void) -> c_int,
    receive_packet: unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int,
    packet_alloc: unsafe extern "C" fn() -> *mut c_void,
    packet_unref: unsafe extern "C" fn(*mut c_void),
    packet_free: unsafe extern "C" fn(*mut *mut c_void),
    frame_alloc: unsafe extern "C" fn() -> *mut c_void,
    frame_get_buffer: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    frame_make_writable: unsafe extern "C" fn(*mut c_void) -> c_int,
    frame_free: unsafe extern "C" fn(*mut *mut c_void),
    opt_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    get_pix_fmt: unsafe extern "C" fn(*const c_char) -> c_int,
    strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
    frame_unref: unsafe extern "C" fn(*mut c_void),
    frame_clone: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    avutil_version: unsafe extern "C" fn() -> std::ffi::c_uint,
    opt_find: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int, c_int) -> *const c_void,
    buffer_ref: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    buffer_unref: unsafe extern "C" fn(*mut *mut c_void),
    hwdevice_find_type_by_name: unsafe extern "C" fn(*const c_char) -> c_int,
    hwdevice_ctx_create: unsafe extern "C" fn(*mut *mut c_void, c_int, *const c_char, *mut c_void, c_int) -> c_int,
    hwframe_transfer_data: unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int,
}

fn load() -> std::result::Result<Api, String> {
    unsafe {
        let open = |names: &[&str]| -> std::result::Result<Library, String> {
            let mut last = String::new();
            for n in names {
                match Library::new(*n) {
                    Ok(l) => return Ok(l),
                    Err(e) => last = e.to_string(),
                }
            }
            Err(format!("cannot load {}: {last}", names[0]))
        };
        // FFmpeg shipped with the client (lib/ next to the executable, see
        // scripts/build-ffmpeg.sh) first, then the system's.
        // MOON_FFMPEG=system skips the bundled copy; MOON_FFMPEG_DIR points elsewhere.
        let mut pair = None;
        if std::env::var("MOON_FFMPEG").as_deref() != Ok("system") {
            // Linux: lib/ next to the executable; Windows: the DLLs next to it.
            let sub = if cfg!(windows) { "" } else { "lib" };
            let dir = std::env::var_os("MOON_FFMPEG_DIR").map(std::path::PathBuf::from).or_else(|| std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join(sub))));
            if let Some(dir) = dir {
                let (u, c) = if cfg!(windows) { (dir.join("avutil-60.dll"), dir.join("avcodec-62.dll")) } else { (dir.join("libavutil.so.60"), dir.join("libavcodec.so.62")) };
                if u.exists() && c.exists() {
                    // avutil first: avcodec's dependency then resolves to it.
                    match (Library::new(&u), Library::new(&c)) {
                        (Ok(u), Ok(c)) => {
                            info!("using the bundled FFmpeg from {}", dir.display());
                            pair = Some((u, c));
                        }
                        (Err(e), _) | (_, Err(e)) => tracing::warn!("bundled FFmpeg in {} unusable ({e}); using the system's", dir.display()),
                    }
                }
            }
        }
        // Matching libavcodec / libavutil pairs: FFmpeg 8, 7, 6, 5.
        for (c, u) in [(62, 60), (61, 59), (60, 58), (59, 57)] {
            if pair.is_some() {
                break;
            }
            let (un, cn) = if cfg!(windows) { (format!("avutil-{u}.dll"), format!("avcodec-{c}.dll")) } else { (format!("libavutil.so.{u}"), format!("libavcodec.so.{c}")) };
            if let (Ok(u), Ok(c)) = (open(&[&un]), open(&[&cn])) {
                pair = Some((u, c));
                break;
            }
        }
        let (avutil, avcodec) = match pair {
            Some(p) => p,
            None => (open(&["libavutil.so"])?, open(&["libavcodec.so"])?),
        };
        macro_rules! sym {
            ($l:expr, $n:literal) => {
                *$l.get($n).map_err(|e| format!("ffmpeg symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        Ok(Api {
            find_encoder_by_name: sym!(avcodec, b"avcodec_find_encoder_by_name\0"),
            find_decoder_by_name: sym!(avcodec, b"avcodec_find_decoder_by_name\0"),
            send_packet: sym!(avcodec, b"avcodec_send_packet\0"),
            receive_frame: sym!(avcodec, b"avcodec_receive_frame\0"),
            alloc_context3: sym!(avcodec, b"avcodec_alloc_context3\0"),
            free_context: sym!(avcodec, b"avcodec_free_context\0"),
            open2: sym!(avcodec, b"avcodec_open2\0"),
            send_frame: sym!(avcodec, b"avcodec_send_frame\0"),
            receive_packet: sym!(avcodec, b"avcodec_receive_packet\0"),
            packet_alloc: sym!(avcodec, b"av_packet_alloc\0"),
            packet_unref: sym!(avcodec, b"av_packet_unref\0"),
            packet_free: sym!(avcodec, b"av_packet_free\0"),
            frame_alloc: sym!(avutil, b"av_frame_alloc\0"),
            frame_get_buffer: sym!(avutil, b"av_frame_get_buffer\0"),
            frame_make_writable: sym!(avutil, b"av_frame_make_writable\0"),
            frame_free: sym!(avutil, b"av_frame_free\0"),
            opt_set: sym!(avutil, b"av_opt_set\0"),
            get_pix_fmt: sym!(avutil, b"av_get_pix_fmt\0"),
            strerror: sym!(avutil, b"av_strerror\0"),
            frame_unref: sym!(avutil, b"av_frame_unref\0"),
            frame_clone: sym!(avutil, b"av_frame_clone\0"),
            avutil_version: sym!(avutil, b"avutil_version\0"),
            opt_find: sym!(avutil, b"av_opt_find\0"),
            buffer_ref: sym!(avutil, b"av_buffer_ref\0"),
            buffer_unref: sym!(avutil, b"av_buffer_unref\0"),
            hwdevice_find_type_by_name: sym!(avutil, b"av_hwdevice_find_type_by_name\0"),
            hwdevice_ctx_create: sym!(avutil, b"av_hwdevice_ctx_create\0"),
            hwframe_transfer_data: sym!(avutil, b"av_hwframe_transfer_data\0"),
            _avutil: avutil,
            _avcodec: avcodec,
        })
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(|e| anyhow!("{e}"))
}

fn err_str(code: c_int) -> String {
    let mut buf = [0 as c_char; 256];
    if let Ok(a) = api() {
        unsafe {
            (a.strerror)(code, buf.as_mut_ptr(), buf.len());
            return CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        }
    }
    format!("error {code}")
}

/// Offsets of the AVFrame fields we write, discovered from a fresh frame.
#[derive(Debug, Clone, Copy)]
struct FrameLayout {
    width: usize,
    height: usize,
    format: usize,
    pict_type: usize,
    pts: usize,
}

const FRAME_DATA: usize = 0; // uint8_t *data[8]
const FRAME_LINESIZE: usize = 64; // int linesize[8]
const PKT_DATA: usize = 24;
const PKT_SIZE: usize = 32;
const PKT_FLAGS: usize = 40;

unsafe fn rd_i32(p: *const c_void, off: usize) -> i32 {
    *((p as *const u8).add(off) as *const i32)
}
unsafe fn rd_i64(p: *const c_void, off: usize) -> i64 {
    *((p as *const u8).add(off) as *const i64)
}
unsafe fn wr_i32(p: *mut c_void, off: usize, v: i32) {
    *((p as *mut u8).add(off) as *mut i32) = v;
}
unsafe fn wr_i64(p: *mut c_void, off: usize, v: i64) {
    *((p as *mut u8).add(off) as *mut i64) = v;
}

fn probe_frame_layout(a: &Api) -> Result<FrameLayout> {
    unsafe {
        let mut f = (a.frame_alloc)();
        if f.is_null() {
            bail!("av_frame_alloc failed");
        }
        // data[8], linesize[8], extended_data, then width, height, nb_samples, format(-1).
        let (width, height, format) = (104, 108, 116);
        let ok_head = rd_i32(f, width) == 0 && rd_i32(f, height) == 0 && rd_i32(f, format) == -1;
        // sample_aspect_ratio defaults to {0, 1}; pict_type sits right before it, pts after it.
        let sar = (120..160).step_by(4).find(|&o| rd_i32(f, o) == 0 && rd_i32(f, o + 4) == 1);
        let res = match (ok_head, sar) {
            (true, Some(sar)) => {
                let pts = (sar + 8).div_ceil(8) * 8;
                if rd_i64(f, pts) == AV_NOPTS_VALUE {
                    Ok(FrameLayout { width, height, format, pict_type: sar - 4, pts })
                } else {
                    Err(anyhow!("unexpected AVFrame layout (pts)"))
                }
            }
            _ => Err(anyhow!("unexpected AVFrame layout")),
        };
        (a.frame_free)(&mut f);
        res
    }
}

pub struct FfmpegEncoder {
    ctx: *mut c_void,
    frame: *mut c_void,
    pkt: *mut c_void,
    layout: FrameLayout,
    width: usize,
    height: usize,
    force_idr: bool,
    pts: i64,
    fps: u32,
    out: Vec<u8>,
    pub codec_name: String,
}

unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    /// Low-latency CBR encoder. `codec` is an FFmpeg encoder name such as `h264_nvenc`.
    pub fn new(codec: &str, width: usize, height: usize, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        Self::with_refs(codec, width, height, fps, bitrate_kbps, 1)
    }

    /// `max_refs`: maximum number of reference frames (clients that shrink the
    /// decoder's picture buffer for latency require 1).
    pub fn with_refs(codec: &str, width: usize, height: usize, fps: u32, bitrate_kbps: u32, max_refs: u32) -> Result<Self> {
        Self::with_options(codec, width, height, fps, bitrate_kbps, &EncoderOptions { max_refs, ..Default::default() })
    }

    pub fn with_options(codec: &str, width: usize, height: usize, fps: u32, bitrate_kbps: u32, o: &EncoderOptions) -> Result<Self> {
        let max_refs = o.max_refs;
        let a = api()?;
        let layout = probe_frame_layout(a)?;
        unsafe {
            let cname = CString::new(codec)?;
            let c = (a.find_encoder_by_name)(cname.as_ptr());
            if c.is_null() {
                bail!("FFmpeg has no encoder named {codec}");
            }
            let mut ctx = (a.alloc_context3)(c);
            if ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            let set = |k: &str, v: &str| -> Result<()> {
                let (k, v) = (CString::new(k)?, CString::new(v)?);
                let r = (a.opt_set)(ctx, k.as_ptr(), v.as_ptr(), AV_OPT_SEARCH_CHILDREN);
                if r < 0 {
                    bail!("option {}={}: {}", k.to_string_lossy(), v.to_string_lossy(), err_str(r));
                }
                Ok(())
            };
            let bps = bitrate_kbps as u64 * 1000;
            let common: Vec<(&str, String)> = vec![
                ("video_size", format!("{width}x{height}")),
                ("pixel_format", "yuv420p".into()),
                // NVENC derives its per-frame bit budget from the time base:
                // it must be 1/fps (pts counted in frames), not a 90 kHz clock.
                ("time_base", format!("1/{}", fps.max(1))),
                ("b", bps.to_string()),
                ("maxrate", bps.to_string()),
                // A VBV of about one frame keeps every frame close to the average size.
                ("bufsize", (bps / fps.max(1) as u64 * 2).to_string()),
                ("g", "2147483647".into()),
                ("bf", "0".into()),
                ("refs", max_refs.max(1).to_string()),
                ("threads", "1".into()),
            ];
            for (k, v) in &common {
                if let Err(e) = set(k, v) {
                    (a.free_context)(&mut ctx);
                    return Err(e);
                }
            }
            // Codec-specific low-latency tuning (ignored when not applicable).
            // Colour signalling (VUI) so decoders use the same matrix/range we convert with.
            let (cs, prim, trc) = match o.csc.matrix {
                crate::frame::Matrix::Bt601 => ("smpte170m", "smpte170m", "smpte170m"),
                crate::frame::Matrix::Bt709 => ("bt709", "bt709", "bt709"),
                crate::frame::Matrix::Bt2020 => ("bt2020nc", "bt2020", "bt2020-10"),
            };
            for (k, v) in [("colorspace", cs), ("color_primaries", prim), ("color_trc", trc), ("color_range", if o.csc.full_range { "pc" } else { "tv" })] {
                if let Err(e) = set(k, v) {
                    debug!("{codec}: {e:#}");
                }
            }
            let preset = o.preset.clone();
            let tuning: Vec<(&str, &str)> = if codec.contains("nvenc") {
                vec![("preset", preset.as_str()), ("tune", "ull"), ("zerolatency", "1"), ("rc", "cbr"), ("delay", "0"), ("forced-idr", "1"), ("aud", "0")]
            } else if codec.contains("vaapi") {
                vec![("rc_mode", "CBR"), ("async_depth", "1")]
            } else {
                vec![("preset", "ultrafast"), ("tune", "zerolatency")]
            };
            let fr = format!("{fps}/1");
            let _ = set("framerate", &fr);
            for (k, v) in tuning {
                if let Err(e) = set(k, v) {
                    debug!("{codec}: {e:#}");
                }
            }
            let r = (a.open2)(ctx, c, std::ptr::null_mut());
            if r < 0 {
                (a.free_context)(&mut ctx);
                bail!("cannot open {codec}: {}", err_str(r));
            }
            let frame = (a.frame_alloc)();
            let pkt = (a.packet_alloc)();
            if frame.is_null() || pkt.is_null() {
                bail!("allocation failed");
            }
            let pix = CString::new("yuv420p")?;
            wr_i32(frame, layout.width, width as i32);
            wr_i32(frame, layout.height, height as i32);
            wr_i32(frame, layout.format, (a.get_pix_fmt)(pix.as_ptr()));
            let r = (a.frame_get_buffer)(frame, 0);
            if r < 0 {
                bail!("av_frame_get_buffer: {}", err_str(r));
            }
            info!("hardware encoder {codec} ready ({width}x{height}@{fps}, {bitrate_kbps} kbps)");
            Ok(Self { ctx, frame, pkt, layout, width, height, force_idr: true, pts: -1, fps: fps.max(1), out: Vec::with_capacity(1 << 20), codec_name: codec.to_string() })
        }
    }

    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// Encode one frame; `pts90k` is the presentation time in 90 kHz units.
    pub fn encode(&mut self, f: &I420Frame, pts90k: i64) -> Result<EncodedFrame<'_>> {
        if f.width != self.width || f.height != self.height {
            bail!("frame size {}x{} does not match encoder {}x{}", f.width, f.height, self.width, self.height);
        }
        let a = api()?;
        unsafe {
            let r = (a.frame_make_writable)(self.frame);
            if r < 0 {
                bail!("av_frame_make_writable: {}", err_str(r));
            }
            if f.yuv444 {
                bail!("4:4:4 needs the direct NVENC encoder");
            }
            let cw = f.chroma_width();
            let ch = f.chroma_height();
            for (plane, src, w, h) in [(0usize, &f.y, f.width, f.height), (1, &f.u, cw, ch), (2, &f.v, cw, ch)] {
                let dst = *((self.frame as *const u8).add(FRAME_DATA + plane * 8) as *const *mut u8);
                let ls = rd_i32(self.frame, FRAME_LINESIZE + plane * 4) as usize;
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(src.as_ptr().add(row * w), dst.add(row * ls), w);
                }
            }
            let frames = pts90k * self.fps as i64 / 90_000;
            self.pts = frames.max(self.pts + 1);
            wr_i64(self.frame, self.layout.pts, self.pts);
            wr_i32(self.frame, self.layout.pict_type, if self.force_idr { AV_PICTURE_TYPE_I } else { AV_PICTURE_TYPE_NONE });
            self.force_idr = false;
            let r = (a.send_frame)(self.ctx, self.frame);
            if r < 0 {
                bail!("avcodec_send_frame: {}", err_str(r));
            }
            self.out.clear();
            let mut idr = false;
            loop {
                let r = (a.receive_packet)(self.ctx, self.pkt);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    bail!("avcodec_receive_packet: {}", err_str(r));
                }
                let data = *((self.pkt as *const u8).add(PKT_DATA) as *const *const u8);
                let size = rd_i32(self.pkt, PKT_SIZE) as usize;
                if rd_i32(self.pkt, PKT_FLAGS) & AV_PKT_FLAG_KEY != 0 {
                    idr = true;
                }
                self.out.extend_from_slice(std::slice::from_raw_parts(data, size));
                (a.packet_unref)(self.pkt);
            }
            Ok(EncodedFrame { data: &self.out, idr })
        }
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.packet_free)(&mut self.pkt);
                (a.frame_free)(&mut self.frame);
                (a.free_context)(&mut self.ctx);
            }
        }
    }
}

/// Quality / colour options for hardware encoders.
#[derive(Debug, Clone)]
pub struct EncoderOptions {
    pub max_refs: u32,
    pub csc: crate::frame::Csc,
    /// NVENC preset p1 (fastest) .. p7 (best quality).
    pub preset: String,
    /// The client uses reference frame invalidation: keep several references.
    pub rfi: bool,
    /// Encode 4:4:4 (direct NVENC, H.264 / HEVC only).
    pub yuv444: bool,
    /// Encode 10-bit HDR10 from P010 input (direct NVENC, HEVC / AV1 only).
    pub hdr10: bool,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self { max_refs: 1, csc: Default::default(), preset: "p4".into(), rfi: false, yuv444: false, hdr10: false }
    }
}

/// Decoding through FFmpeg: software (libdav1d, h264, hevc...) or with a
/// hardware accelerator (VAAPI on Intel/AMD, CUDA...) whose frames are copied
/// back to system memory.
pub struct FfmpegDecoder {
    ctx: *mut c_void,
    frame: *mut c_void,
    sw_frame: *mut c_void,
    pkt: *mut c_void,
    out: I420Frame,
    yuv420p: c_int,
    yuvj420p: c_int,
    yuv444p: c_int,
    yuvj444p: c_int,
    nv12: c_int,
    yuv420p10: c_int,
    p010: c_int,
    out16: crate::hdr::Yuv16Frame,
    /// The last picture is 10-bit (in `out16`).
    last16: bool,
    /// Hardware device reference (null for software decoding).
    hw_device: *mut c_void,
    /// Pixel format of hardware frames (-1 for software decoding).
    hw_fmt: c_int,
    /// Whether the last picture really came from the hardware decoder.
    pub hw_active: bool,
    /// Zero-copy output of VAAPI pictures (kept on the GPU) while this is true;
    /// the renderer clears it if it cannot import them.
    pub zero_copy: Option<Arc<AtomicBool>>,
    /// VADisplay of the decoder's device (null unless zero-copy is possible).
    va_display: *mut c_void,
    gpu_out: Option<VaapiFrame>,
}

unsafe impl Send for FfmpegDecoder {}

/// A decoded picture still on the GPU: a VAAPI surface (NV12), referenced
/// until this is dropped. The renderer imports it as textures (DMA-BUF).
pub struct VaapiFrame {
    frame: *mut c_void,
    pub display: *mut c_void,
    pub surface: u32,
    pub width: usize,
    pub height: usize,
    pub csc: crate::frame::Csc,
}

unsafe impl Send for VaapiFrame {}

impl Drop for VaapiFrame {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.frame_free)(&mut self.frame) }
        }
    }
}

/// VADisplay behind an FFmpeg VAAPI device reference: AVBufferRef.data ->
/// AVHWDeviceContext.hwctx -> AVVAAPIDeviceContext.display. These offsets were
/// checked against FFmpeg 8's headers (libavutil 60), the version shipped with
/// the client; with any other version zero-copy is not used.
unsafe fn vaapi_display(a: &Api, device: *mut c_void) -> *mut c_void {
    if device.is_null() || (a.avutil_version)() >> 16 != 60 {
        return std::ptr::null_mut();
    }
    let ctx = *((device as *const u8).add(8) as *const *const u8);
    if ctx.is_null() {
        return std::ptr::null_mut();
    }
    let hwctx = *(ctx.add(16) as *const *const *mut c_void);
    if hwctx.is_null() {
        return std::ptr::null_mut();
    }
    *hwctx
}

/// Offset of `AVCodecContext.hw_device_ctx`. Not exposed through AVOptions,
/// but in every FFmpeg version it is the pointer right before `hwaccel_flags`,
/// which is.
unsafe fn hw_device_ctx_offset(a: &Api, ctx: *mut c_void) -> Result<usize> {
    let name = CString::new("hwaccel_flags")?;
    let opt = (a.opt_find)(ctx, name.as_ptr(), std::ptr::null(), 0, 0);
    if opt.is_null() {
        bail!("AVCodecContext has no hwaccel_flags option");
    }
    let off = rd_i32(opt, 16) as usize; // AVOption.offset
    if off < 8 || !off.is_multiple_of(8) {
        bail!("unexpected hwaccel_flags offset {off}");
    }
    let slot = off - 8;
    // A fresh context has no device: the slot must be a null pointer.
    if *((ctx as *const u8).add(slot) as *const usize) != 0 {
        bail!("unexpected AVCodecContext layout");
    }
    Ok(slot)
}

impl FfmpegDecoder {
    /// Software decoder: the first of `names` that FFmpeg has.
    pub fn new(names: &[&str]) -> Result<Self> {
        Self::open(names, None)
    }

    /// Hardware-accelerated decoder. `hw` is an FFmpeg device type ("vaapi",
    /// "cuda"...) and `device` an optional device path (DRM render node for VAAPI).
    pub fn new_hw(names: &[&str], hw: &str, device: Option<&str>) -> Result<Self> {
        Self::open(names, Some((hw, device)))
    }

    fn open(names: &[&str], hw: Option<(&str, Option<&str>)>) -> Result<Self> {
        let a = api()?;
        probe_frame_layout(a)?;
        unsafe {
            let mut codec = std::ptr::null();
            for n in names {
                let c = CString::new(*n)?;
                codec = (a.find_decoder_by_name)(c.as_ptr());
                if !codec.is_null() {
                    break;
                }
            }
            if codec.is_null() {
                bail!("FFmpeg has none of the decoders {names:?}");
            }
            let mut ctx = (a.alloc_context3)(codec);
            if ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            // Low latency: frame threading delays output by one frame per thread,
            // so only slice/tile threads; dav1d must output each frame at once.
            for (k, v) in [("threads", "4"), ("thread_type", "slice"), ("flags", "+low_delay"), ("max_frame_delay", "1")] {
                let (k, v) = (CString::new(k)?, CString::new(v)?);
                (a.opt_set)(ctx, k.as_ptr(), v.as_ptr(), AV_OPT_SEARCH_CHILDREN);
            }
            let fmt = |n: &str| CString::new(n).map(|c| (a.get_pix_fmt)(c.as_ptr())).unwrap_or(-1);
            let mut hw_device = std::ptr::null_mut();
            let mut hw_fmt = -1;
            if let Some((kind, device)) = hw {
                let r = (|| -> Result<()> {
                    let k = CString::new(kind)?;
                    let ty = (a.hwdevice_find_type_by_name)(k.as_ptr());
                    if ty <= 0 {
                        bail!("FFmpeg has no {kind} support");
                    }
                    let dev = device.map(CString::new).transpose()?;
                    let r = (a.hwdevice_ctx_create)(&mut hw_device, ty, dev.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), std::ptr::null_mut(), 0);
                    if r < 0 {
                        hw_device = std::ptr::null_mut();
                        bail!("cannot open the {kind} device: {}", err_str(r));
                    }
                    let off = hw_device_ctx_offset(a, ctx)?;
                    // FFmpeg's default get_format picks the hardware format of the supplied device.
                    *((ctx as *mut u8).add(off) as *mut *mut c_void) = (a.buffer_ref)(hw_device);
                    hw_fmt = fmt(kind);
                    Ok(())
                })();
                if let Err(e) = r {
                    if !hw_device.is_null() {
                        (a.buffer_unref)(&mut hw_device);
                    }
                    (a.free_context)(&mut ctx);
                    return Err(e);
                }
            }
            let r = (a.open2)(ctx, codec, std::ptr::null_mut());
            if r < 0 {
                (a.free_context)(&mut ctx);
                if !hw_device.is_null() {
                    (a.buffer_unref)(&mut hw_device);
                }
                bail!("cannot open decoder: {}", err_str(r));
            }
            Ok(Self {
                ctx,
                frame: (a.frame_alloc)(),
                sw_frame: (a.frame_alloc)(),
                pkt: (a.packet_alloc)(),
                out: I420Frame::new(2, 2),
                yuv420p: fmt("yuv420p"),
                yuvj420p: fmt("yuvj420p"),
                yuv444p: fmt("yuv444p"),
                yuvj444p: fmt("yuvj444p"),
                yuv420p10: fmt("yuv420p10le"),
                p010: fmt("p010le"),
                out16: crate::hdr::Yuv16Frame::new(2, 2),
                last16: false,
                nv12: fmt("nv12"),
                va_display: if hw.is_some_and(|(k, _)| k == "vaapi") { vaapi_display(a, hw_device) } else { std::ptr::null_mut() },
                hw_device,
                hw_fmt,
                hw_active: false,
                zero_copy: None,
                gpu_out: None,
            })
        }
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<&I420Frame>> {
        Ok(if self.decode_ready(data)? { Some(&self.out) } else { None })
    }

    /// The last decoded picture.
    pub fn picture(&self) -> &I420Frame {
        &self.out
    }

    /// The last picture if it is 10-bit.
    pub fn picture16(&self) -> Option<&crate::hdr::Yuv16Frame> {
        self.last16.then_some(&self.out16)
    }

    /// The last picture if it was kept on the GPU (zero-copy), else `None`.
    pub fn take_gpu_picture(&mut self) -> Option<VaapiFrame> {
        self.gpu_out.take()
    }

    /// Decode one access unit; true if a new picture is available in [`Self::picture`].
    pub fn decode_ready(&mut self, data: &[u8]) -> Result<bool> {
        let a = api()?;
        unsafe {
            *((self.pkt as *mut u8).add(PKT_DATA) as *mut *const u8) = data.as_ptr();
            wr_i32(self.pkt, PKT_SIZE, data.len() as i32);
            let r = (a.send_packet)(self.ctx, self.pkt);
            *((self.pkt as *mut u8).add(PKT_DATA) as *mut *const u8) = std::ptr::null();
            wr_i32(self.pkt, PKT_SIZE, 0);
            if r < 0 && r != AVERROR_EAGAIN {
                bail!("avcodec_send_packet: {}", err_str(r));
            }
            let mut got = false;
            loop {
                let r = (a.receive_frame)(self.ctx, self.frame);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    bail!("avcodec_receive_frame: {}", err_str(r));
                }
                let mut src = self.frame;
                self.hw_active = self.hw_fmt >= 0 && rd_i32(self.frame, 116) == self.hw_fmt;
                // Zero-copy: hand the VAAPI surface itself to the renderer.
                if self.hw_active && !self.va_display.is_null() && self.zero_copy.as_ref().is_some_and(|z| z.load(Ordering::Relaxed)) {
                    let f = (a.frame_clone)(self.frame);
                    if !f.is_null() {
                        let surface = *((f as *const u8).add(FRAME_DATA + 3 * 8) as *const usize) as u32;
                        self.gpu_out = Some(VaapiFrame { frame: f, display: self.va_display, surface, width: rd_i32(f, 104) as usize, height: rd_i32(f, 108) as usize, csc: self.out.csc });
                        got = true;
                        continue;
                    }
                }
                self.gpu_out = None;
                if self.hw_active {
                    // GPU surface -> system memory (NV12 for 8-bit 4:2:0).
                    (a.frame_unref)(self.sw_frame);
                    let r = (a.hwframe_transfer_data)(self.sw_frame, self.frame, 0);
                    if r < 0 {
                        bail!("av_hwframe_transfer_data: {}", err_str(r));
                    }
                    src = self.sw_frame;
                }
                let (w, h, fmt) = (rd_i32(src, 104) as usize, rd_i32(src, 108) as usize, rd_i32(src, 116));
                let plane = |i: usize, rows: usize| -> (&[u8], usize) {
                    let p = *((src as *const u8).add(FRAME_DATA + i * 8) as *const *const u8);
                    let ls = rd_i32(src, FRAME_LINESIZE + i * 4) as usize;
                    (std::slice::from_raw_parts(p, ls * rows), ls)
                };
                let ch = h.div_ceil(2);
                self.last16 = fmt == self.yuv420p10 || fmt == self.p010;
                let plane16 = |i: usize, rows: usize| -> (&[u16], usize) {
                    let p = *((src as *const u8).add(FRAME_DATA + i * 8) as *const *const u16);
                    let ls = rd_i32(src, FRAME_LINESIZE + i * 4) as usize / 2;
                    (std::slice::from_raw_parts(p, ls * rows), ls)
                };
                if fmt == self.yuv420p10 {
                    let (y, sy) = plane16(0, h);
                    let (u, su) = plane16(1, ch);
                    let (v, sv) = plane16(2, ch);
                    self.out16.copy_from_planar10(w, h, [y, u, v], [sy, su, sv]);
                } else if fmt == self.p010 {
                    let (y, sy) = plane16(0, h);
                    let (uv, suv) = plane16(1, ch);
                    self.out16.copy_from_p010(w, h, y, sy, uv, suv);
                } else if fmt == self.yuv420p || fmt == self.yuvj420p {
                    let (y, sy) = plane(0, h);
                    let (u, su) = plane(1, ch);
                    let (v, sv) = plane(2, ch);
                    self.out.copy_from_planes(w, h, [y, u, v], [sy, su, sv]);
                } else if fmt == self.yuv444p || fmt == self.yuvj444p {
                    let (y, sy) = plane(0, h);
                    let (u, su) = plane(1, h);
                    let (v, sv) = plane(2, h);
                    self.out.copy_from_planes_fmt(w, h, true, [y, u, v], [sy, su, sv]);
                } else if fmt == self.nv12 {
                    let (y, sy) = plane(0, h);
                    let (uv, suv) = plane(1, ch);
                    self.out.copy_from_nv12(w, h, y, sy, uv, suv);
                } else {
                    bail!("unsupported decoded pixel format {fmt}");
                }
                got = true;
            }
            Ok(got)
        }
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.packet_free)(&mut self.pkt);
                (a.frame_free)(&mut self.frame);
                (a.frame_free)(&mut self.sw_frame);
                (a.free_context)(&mut self.ctx);
                if !self.hw_device.is_null() {
                    (a.buffer_unref)(&mut self.hw_device);
                }
            }
        }
    }
}

/// Check whether an FFmpeg encoder can actually be opened on this machine.
pub fn probe(codec: &str) -> Result<()> {
    FfmpegEncoder::new(codec, 640, 360, 30, 2000).map(|_| ()).with_context(|| format!("{codec} unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FFmpeg decoding through a hardware accelerator (same code path as VAAPI;
    /// CUDA is used here because it exists wherever NVENC does). Skipped without
    /// NVENC or without FFmpeg's native AV1 decoder.
    #[test]
    fn hwaccel_decode_roundtrip() {
        let (w, h) = (1280, 720);
        let Ok(mut enc) = FfmpegEncoder::new("av1_nvenc", w, h, 60, 8000) else {
            eprintln!("no av1_nvenc, skipping");
            return;
        };
        let mut dec = match FfmpegDecoder::new_hw(&["av1"], "cuda", None) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("no CUDA hwaccel decoder ({e:#}), skipping");
                return;
            }
        };
        let mut f = I420Frame::new(w, h);
        let (mut decoded, mut hw, mut err_sum) = (0, 0, 0u64);
        for i in 0..30usize {
            for (k, p) in f.y.iter_mut().enumerate() {
                *p = (16 + (k % w / 4 + i * 2) % 200) as u8;
            }
            for (k, p) in f.u.iter_mut().enumerate() {
                *p = (64 + k % 97) as u8;
            }
            let data = enc.encode(&f, i as i64 * 1500).unwrap().data.to_vec();
            if let Some(out) = dec.decode(&data).unwrap() {
                assert_eq!((out.width, out.height), (w, h));
                err_sum += out.y.iter().zip(&f.y).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / (w * h) as u64;
                let uerr = out.u.iter().zip(&f.u).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / f.u.len() as u64;
                assert!(uerr <= 8, "chroma mismatch (NV12 deinterleave?) {uerr}");
                decoded += 1;
                hw += dec.hw_active as u32;
            }
        }
        eprintln!("hwaccel: {decoded}/30 decoded, {hw} on the GPU, mean luma error {}", err_sum / decoded.max(1) as u64);
        assert!(decoded >= 28, "decoded only {decoded}");
        assert_eq!(hw, decoded, "frames did not come from the GPU");
        assert!(err_sum / decoded as u64 <= 6);
    }
    use crate::h264::H264Decoder;

    #[test]
    fn dav1d_decodes_nvenc_av1_if_available() {
        let (Ok(mut enc), Ok(mut dec)) = (FfmpegEncoder::new("av1_nvenc", 640, 360, 30, 2000), FfmpegDecoder::new(&["libdav1d"])) else {
            eprintln!("av1_nvenc or libdav1d unavailable, skipping");
            return;
        };
        let f = I420Frame::new(640, 360);
        let mut n = 0;
        for i in 0..30 {
            let d = enc.encode(&f, i * 3000).unwrap().data.to_vec();
            if dec.decode(&d).unwrap().is_some() {
                n += 1;
            }
        }
        assert!(n >= 25, "dav1d decoded {n}/30");
    }

    /// NVENC must spend roughly the requested bitrate on detailed content.
    #[test]
    fn nvenc_honours_bitrate_if_available() {
        let Ok(mut enc) = FfmpegEncoder::new("hevc_nvenc", 1280, 720, 60, 10_000) else { return };
        let mut f = I420Frame::new(1280, 720);
        let mut seed = 1u32;
        let mut total = 0usize;
        for i in 0..120i64 {
            for p in f.y.iter_mut() {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *p = 16 + (seed >> 24) as u8 % 200;
            }
            total += enc.encode(&f, i * 1500).unwrap().data.len();
        }
        let mbps = total as f64 * 8.0 / 2.0 / 1e6;
        eprintln!("hevc_nvenc produced {mbps:.1} Mbit/s for 10 requested");
        assert!(mbps > 6.0 && mbps < 14.0, "bitrate not honoured: {mbps:.1} Mbit/s");
    }

    /// Runs only where NVENC is present.
    #[test]
    fn nvenc_roundtrip_if_available() {
        if probe("h264_nvenc").is_err() {
            eprintln!("h264_nvenc not available, skipping: {:#}", probe("h264_nvenc").unwrap_err());
            return;
        }
        let (w, h) = (1280, 720);
        let mut enc = FfmpegEncoder::new("h264_nvenc", w, h, 60, 10_000).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let mut f = I420Frame::new(w, h);
        let mut decoded = 0;
        let mut idrs = vec![];
        let t = std::time::Instant::now();
        for i in 0..120usize {
            for (k, p) in f.y.iter_mut().enumerate() {
                *p = (16 + (k % w + i * 3) % 200) as u8;
            }
            if i == 60 {
                enc.request_idr();
            }
            let e = enc.encode(&f, (i as i64) * 1500).unwrap();
            if e.idr {
                idrs.push(i);
            }
            let data = e.data.to_vec();
            if let Some(p) = dec.decode(&data).unwrap() {
                assert_eq!((p.width, p.height), (w, h));
                decoded += 1;
            }
        }
        eprintln!("nvenc: 120 frames in {:?}, IDR at {idrs:?}, decoded {decoded}", t.elapsed());
        assert_eq!(idrs, vec![0, 60], "IDR only on first frame and on request");
        assert!(decoded >= 118);
    }
}

#[cfg(test)]
mod idr_size_probe {
    use super::*;

    /// Diagnostic: size of forced IDR frames on busy content (run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn idr_sizes() {
        for (codec, kbps) in [("hevc_nvenc", 20_000u32), ("hevc_nvenc", 50_000), ("h264_nvenc", 20_000)] {
            let Ok(mut enc) = FfmpegEncoder::new(codec, 1920, 1080, 60, kbps) else { continue };
            let mut f = I420Frame::new(1920, 1080);
            let mut seed = 7u32;
            let (mut p_max, mut idr_sizes) = (0usize, vec![]);
            for i in 0..120i64 {
                for p in f.y.iter_mut().step_by(3) {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    *p = 16 + (seed >> 24) as u8 % 200;
                }
                if i % 20 == 10 {
                    enc.request_idr();
                }
                let e = enc.encode(&f, i * 1500).unwrap();
                if e.idr { idr_sizes.push(e.data.len()) } else { p_max = p_max.max(e.data.len()) }
            }
            let shards: Vec<usize> = idr_sizes.iter().map(|s| (s + 8).div_ceil(1376)).collect();
            eprintln!("{codec} {kbps} kbps: IDR sizes {idr_sizes:?} -> data shards {shards:?}; max P {p_max} B");
        }
    }
}
