//! Hardware video encoding through the system FFmpeg (e.g. `h264_nvenc`),
//! loaded at runtime. Encoder parameters are set through the AVOptions API
//! (by name), so no FFmpeg headers or struct layouts are needed except a few
//! AVFrame/AVPacket fields whose offsets are discovered at runtime.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

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
        let avutil = open(&["libavutil.so.60", "libavutil.so.59", "libavutil.so"])?;
        let avcodec = open(&["libavcodec.so.62", "libavcodec.so.61", "libavcodec.so"])?;
        macro_rules! sym {
            ($l:expr, $n:literal) => {
                *$l.get($n).map_err(|e| format!("ffmpeg symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        Ok(Api {
            find_encoder_by_name: sym!(avcodec, b"avcodec_find_encoder_by_name\0"),
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
                ("time_base", "1/90000".into()),
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
            let tuning: &[(&str, &str)] = if codec.contains("nvenc") {
                &[("preset", "p1"), ("tune", "ull"), ("zerolatency", "1"), ("rc", "cbr"), ("delay", "0"), ("forced-idr", "1"), ("aud", "0")]
            } else if codec.contains("vaapi") {
                &[("rc_mode", "CBR"), ("async_depth", "1")]
            } else {
                &[("preset", "ultrafast"), ("tune", "zerolatency")]
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
            Ok(Self { ctx, frame, pkt, layout, width, height, force_idr: true, pts: 0, out: Vec::with_capacity(1 << 20), codec_name: codec.to_string() })
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
            let cw = f.chroma_width();
            let ch = f.height.div_ceil(2);
            for (plane, src, w, h) in [(0usize, &f.y, f.width, f.height), (1, &f.u, cw, ch), (2, &f.v, cw, ch)] {
                let dst = *((self.frame as *const u8).add(FRAME_DATA + plane * 8) as *const *mut u8);
                let ls = rd_i32(self.frame, FRAME_LINESIZE + plane * 4) as usize;
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(src.as_ptr().add(row * w), dst.add(row * ls), w);
                }
            }
            self.pts = self.pts.max(pts90k).max(self.pts + 1);
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

/// Check whether an FFmpeg encoder can actually be opened on this machine.
pub fn probe(codec: &str) -> Result<()> {
    FfmpegEncoder::new(codec, 640, 360, 30, 2000).map(|_| ()).with_context(|| format!("{codec} unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::H264Decoder;

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
