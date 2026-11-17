//! Direct NVENC encoder (through `csrc/nvenc_shim.c`), used by the host when
//! available: unlike FFmpeg's wrapper it can invalidate reference frames, so a
//! client that lost frames recovers with a small P-frame instead of an IDR.

use std::ffi::{c_char, c_int, c_void, CStr};

use anyhow::{bail, Result};

use crate::encoder::Codec;
use crate::frame::{I420Frame, Matrix};
use crate::h264::EncodedFrame;

#[repr(C)]
struct Config {
    codec: c_int,
    width: c_int,
    height: c_int,
    fps: c_int,
    bitrate_kbps: c_int,
    preset: c_int,
    dpb: c_int,
    yuv444: c_int,
    matrix: c_int,
    full_range: c_int,
}

extern "C" {
    fn moon_nvenc_open(c: *const Config, err: *mut c_char, errlen: c_int) -> *mut c_void;
    #[allow(clippy::too_many_arguments)]
    fn moon_nvenc_encode(
        e: *mut c_void,
        y: *const u8,
        u: *const u8,
        v: *const u8,
        ys: c_int,
        us: c_int,
        vs: c_int,
        ts: u64,
        force_idr: c_int,
        out: *mut *const u8,
        out_len: *mut u32,
        is_idr: *mut c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> c_int;
    fn moon_nvenc_invalidate(e: *mut c_void, ts: u64) -> c_int;
    fn moon_nvenc_close(e: *mut c_void);
}

/// Reference frames kept when the client uses RFI: enough to cover the loss
/// detection + request round trip (~130 ms at 60 fps).
pub const RFI_DPB: u32 = 8;

pub struct NvencEncoder {
    e: *mut c_void,
    codec: Codec,
    width: usize,
    height: usize,
    dpb: u32,
    force_idr: bool,
    pub codec_name: String,
    pub yuv444: bool,
}

unsafe impl Send for NvencEncoder {}

fn err_msg(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() }
}

impl NvencEncoder {
    /// `dpb` > 1 keeps that many reference frames so they can be invalidated.
    pub fn new(codec: Codec, width: usize, height: usize, fps: u32, bitrate_kbps: u32, opts: &crate::ffenc::EncoderOptions, dpb: u32) -> Result<Self> {
        let preset = opts.preset.trim_start_matches('p').parse::<c_int>().unwrap_or(4).clamp(1, 7);
        let cfg = Config {
            codec: codec as c_int,
            width: width as c_int,
            height: height as c_int,
            fps: fps.max(1) as c_int,
            bitrate_kbps: bitrate_kbps as c_int,
            preset,
            dpb: dpb.max(1) as c_int,
            yuv444: opts.yuv444 as c_int,
            matrix: match opts.csc.matrix {
                Matrix::Bt601 => 0,
                Matrix::Bt709 => 1,
                Matrix::Bt2020 => 2,
            },
            full_range: opts.csc.full_range as c_int,
        };
        let mut err = [0 as c_char; 256];
        let e = unsafe { moon_nvenc_open(&cfg, err.as_mut_ptr(), err.len() as c_int) };
        if e.is_null() {
            bail!("NVENC: {}", err_msg(&err));
        }
        let codec_name = format!("nvenc-{}", match codec {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        });
        Ok(Self { e, codec, width, height, dpb: dpb.max(1), force_idr: true, codec_name, yuv444: opts.yuv444 })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// Encode `f`; `ts` identifies the picture for [`Self::invalidate`] (the frame index).
    pub fn encode(&mut self, f: &I420Frame, ts: u64) -> Result<EncodedFrame<'_>> {
        if f.yuv444 != self.yuv444 {
            bail!("NVENC: frame and encoder chroma formats differ");
        }
        if f.width != self.width || f.height != self.height {
            bail!("NVENC: frame is {}x{}, encoder is {}x{}", f.width, f.height, self.width, self.height);
        }
        let cw = f.chroma_width() as c_int;
        let (mut out, mut len, mut idr) = (std::ptr::null(), 0u32, 0);
        let mut err = [0 as c_char; 256];
        let r = unsafe {
            moon_nvenc_encode(self.e, f.y.as_ptr(), f.u.as_ptr(), f.v.as_ptr(), f.width as c_int, cw, cw, ts, self.force_idr as c_int, &mut out, &mut len, &mut idr, err.as_mut_ptr(), err.len() as c_int)
        };
        if r != 0 {
            bail!("{}", err_msg(&err));
        }
        self.force_idr = false;
        let data = if out.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(out, len as usize) } };
        Ok(EncodedFrame { data, idr: idr != 0 })
    }

    /// Stop referencing the pictures `first..=last` (frame indexes / timestamps).
    /// False if that is impossible (range larger than the reference buffer, or
    /// the driver refused): the caller must then send an IDR.
    pub fn invalidate(&mut self, first: u32, last: u32) -> bool {
        let n = last.wrapping_sub(first);
        // NVENC answers an AV1 invalidation with a key frame anyway: say so, and
        // let the caller send a proper IDR.
        if self.dpb < 2 || n >= self.dpb - 1 || !self.supports_rfi() {
            return false;
        }
        (0..=n).all(|i| unsafe { moon_nvenc_invalidate(self.e, first.wrapping_add(i) as u64) } == 0)
    }

    /// Real RFI (recovery with a P-frame): H.264 and HEVC; AV1 falls back to IDR.
    pub fn supports_rfi(&self) -> bool {
        self.dpb > 1 && self.codec != Codec::Av1
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe { moon_nvenc_close(self.e) }
    }
}

/// True if the direct NVENC path works for `codec` on this machine.
pub fn available(codec: Codec) -> bool {
    NvencEncoder::new(codec, 640, 360, 30, 2000, &Default::default(), RFI_DPB).is_ok()
}

/// True if NVENC can encode `codec` in 4:4:4 here.
pub fn available_444(codec: Codec) -> bool {
    codec != Codec::Av1 && NvencEncoder::new(codec, 640, 360, 30, 2000, &crate::ffenc::EncoderOptions { yuv444: true, ..Default::default() }, 1).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4:4:4 HEVC: NVENC -> NVDEC keeps full-resolution chroma (one-pixel colour stripes).
    #[test]
    fn nvenc_nvdec_444_roundtrip() {
        if !crate::nvdec::available() || !available_444(Codec::Hevc) || !crate::nvdec::supports_444(Codec::Hevc) {
            eprintln!("no HEVC 4:4:4 on this GPU, skipping");
            return;
        }
        let (w, h) = (1280, 720);
        let opts = crate::ffenc::EncoderOptions { yuv444: true, ..Default::default() };
        let mut enc = NvencEncoder::new(Codec::Hevc, w, h, 60, 20000, &opts, 1).unwrap();
        let mut dec = crate::nvdec::NvDecoder::new(Codec::Hevc).unwrap();
        let mut f = I420Frame::with_format(w, h, true);
        for (k, (u, v)) in f.u.iter_mut().zip(f.v.iter_mut()).enumerate() {
            // alternate chroma every pixel: impossible to represent in 4:2:0
            (*u, *v) = if k % 2 == 0 { (60, 200) } else { (200, 60) };
        }
        let mut checked = 0;
        for i in 1..=10u64 {
            let data = enc.encode(&f, i).unwrap().data.to_vec();
            if let Some(out) = dec.decode(&data).unwrap() {
                assert!(out.yuv444, "decoder output is not 4:4:4");
                let eu = out.u.iter().zip(&f.u).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / f.u.len() as u64;
                let ev = out.v.iter().zip(&f.v).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / f.v.len() as u64;
                assert!(eu <= 8 && ev <= 8, "chroma detail lost: mean error U {eu}, V {ev}");
                checked += 1;
            }
        }
        eprintln!("4:4:4 HEVC: {checked} pictures with full chroma");
        assert!(checked >= 8);
    }

    /// Encode with NVENC, decode with NVDEC; invalidate references mid-stream and
    /// check that the decoder, which never sees the "lost" frames, still decodes
    /// the recovery frame correctly. Skipped without an NVIDIA GPU.
    #[test]
    fn nvenc_rfi_roundtrip() {
        if !crate::nvdec::available() {
            eprintln!("no NVDEC, skipping");
            return;
        }
        let (w, h) = (1280, 720);
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let mut enc = match NvencEncoder::new(codec, w, h, 60, 8000, &Default::default(), RFI_DPB) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("{codec:?}: {e:#}, skipping");
                    continue;
                }
            };
            let mut dec = crate::nvdec::NvDecoder::new(codec).unwrap();
            let mut f = I420Frame::new(w, h);
            let (mut decoded, mut idr_count, mut err_sum) = (0, 0, 0u64);
            for i in 1..=60u32 {
                for (k, p) in f.y.iter_mut().enumerate() {
                    *p = (16 + (k % w / 4 + i as usize * 3) % 200) as u8;
                }
                // Frames 20..=22 are "lost": the client never decodes them and
                // asks for their invalidation when frame 24 is encoded.
                if i == 24 && !enc.invalidate(20, 23) {
                    assert_eq!(codec, Codec::Av1, "{codec:?}: invalidation refused");
                    enc.request_idr();
                }
                let e = enc.encode(&f, i as u64).unwrap();
                idr_count += e.idr as u32;
                let data = e.data.to_vec();
                if (20..=23).contains(&i) {
                    continue;
                }
                if let Some(out) = dec.decode(&data).unwrap() {
                    let err = out.y.iter().zip(&f.y).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum::<u64>() / (w * h) as u64;
                    if i >= 24 {
                        assert!(err <= 6, "{codec:?}: frame {i} decoded wrongly after RFI (error {err})");
                    }
                    err_sum += err;
                    decoded += 1;
                }
            }
            eprintln!("{codec:?}: {decoded} decoded, {idr_count} IDR, mean error {}", err_sum / decoded.max(1) as u64);
            // H.264/HEVC recover with a P-frame; AV1 with an IDR.
            assert_eq!(idr_count, if codec == Codec::Av1 { 2 } else { 1 }, "{codec:?}: unexpected IDR count");
            assert!(decoded >= 54);
        }
    }
}
