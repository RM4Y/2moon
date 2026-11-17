//! Opus multistream encoder/decoder, loaded at runtime from the system
//! `libopus.so.0` (no build-time headers needed).

use std::ffi::{c_int, c_uchar, c_void};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use libloading::Library;

const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;

type EncCreate = unsafe extern "C" fn(i32, c_int, c_int, c_int, *const c_uchar, c_int, *mut c_int) -> *mut c_void;
type Encode = unsafe extern "C" fn(*mut c_void, *const i16, c_int, *mut c_uchar, i32) -> i32;
type EncCtl = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type DecCreate = unsafe extern "C" fn(i32, c_int, c_int, c_int, *const c_uchar, *mut c_int) -> *mut c_void;
type Decode = unsafe extern "C" fn(*mut c_void, *const c_uchar, i32, *mut i16, c_int, c_int) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);

struct Api {
    _lib: Library,
    enc_create: EncCreate,
    encode: Encode,
    enc_ctl: EncCtl,
    enc_destroy: Destroy,
    dec_create: DecCreate,
    decode: Decode,
    dec_destroy: Destroy,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = Library::new("libopus.so.0").or_else(|_| Library::new("libopus.so")).map_err(|e| format!("cannot load libopus: {e}"))?;
        macro_rules! sym {
            ($n:literal) => {
                *lib.get($n).map_err(|e| format!("libopus symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        Ok(Api {
            enc_create: sym!(b"opus_multistream_encoder_create\0"),
            encode: sym!(b"opus_multistream_encode\0"),
            enc_ctl: sym!(b"opus_multistream_encoder_ctl\0"),
            enc_destroy: sym!(b"opus_multistream_encoder_destroy\0"),
            dec_create: sym!(b"opus_multistream_decoder_create\0"),
            decode: sym!(b"opus_multistream_decode\0"),
            dec_destroy: sym!(b"opus_multistream_decoder_destroy\0"),
            _lib: lib,
        })
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// Multistream layout, as advertised in the `surround-params` SDP attribute:
/// `<channels><streams><coupled><mapping digits...>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLayout {
    pub channels: u8,
    pub streams: u8,
    pub coupled: u8,
    pub mapping: Vec<u8>,
}

impl StreamLayout {
    pub fn stereo() -> Self {
        Self { channels: 2, streams: 1, coupled: 1, mapping: vec![0, 1] }
    }

    pub fn surround51() -> Self {
        Self { channels: 6, streams: 4, coupled: 2, mapping: vec![0, 1, 2, 4, 5, 3] }
    }

    pub fn surround71() -> Self {
        Self { channels: 8, streams: 5, coupled: 3, mapping: vec![0, 1, 2, 4, 5, 3, 6, 7] }
    }

    pub fn for_channels(ch: u32) -> Self {
        match ch {
            6 => Self::surround51(),
            8 => Self::surround71(),
            _ => Self::stereo(),
        }
    }

    pub fn to_sdp(&self) -> String {
        let mut s = format!("{}{}{}", self.channels, self.streams, self.coupled);
        for m in &self.mapping {
            s.push_str(&m.to_string());
        }
        s
    }

    pub fn from_sdp(s: &str) -> Option<Self> {
        let d: Vec<u8> = s.bytes().map(|b| b.wrapping_sub(b'0')).collect();
        if d.len() < 3 || d.iter().any(|x| *x > 9) {
            return None;
        }
        let ch = d[0] as usize;
        if d.len() != 3 + ch {
            return None;
        }
        Some(Self { channels: d[0], streams: d[1], coupled: d[2], mapping: d[3..].to_vec() })
    }
}

pub struct OpusEncoder {
    st: *mut c_void,
    pub layout: StreamLayout,
    pub frame_size: usize,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    /// `bitrate`: bits per second; `cbr` makes every packet the same size (needed for audio FEC).
    pub fn new(layout: StreamLayout, sample_rate: u32, frame_ms: u32, bitrate: i32, cbr: bool) -> Result<Self> {
        let a = api()?;
        let mut err = 0;
        let st = unsafe {
            (a.enc_create)(sample_rate as i32, layout.channels as c_int, layout.streams as c_int, layout.coupled as c_int, layout.mapping.as_ptr(), OPUS_APPLICATION_RESTRICTED_LOWDELAY, &mut err)
        };
        if st.is_null() || err != 0 {
            bail!("opus_multistream_encoder_create failed ({err})");
        }
        unsafe {
            (a.enc_ctl)(st, OPUS_SET_BITRATE_REQUEST, bitrate as c_int);
            (a.enc_ctl)(st, OPUS_SET_VBR_REQUEST, (!cbr) as c_int);
        }
        Ok(Self { st, frame_size: (sample_rate * frame_ms / 1000) as usize, layout })
    }

    /// Encode exactly `frame_size` interleaved samples per channel.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        assert_eq!(pcm.len(), self.frame_size * self.layout.channels as usize);
        out.resize(4000, 0);
        let n = unsafe { (api()?.encode)(self.st, pcm.as_ptr(), self.frame_size as c_int, out.as_mut_ptr(), out.len() as i32) };
        if n < 0 {
            bail!("opus encode error {n}");
        }
        out.truncate(n as usize);
        Ok(())
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.enc_destroy)(self.st) }
        }
    }
}

pub struct OpusDecoder {
    st: *mut c_void,
    pub layout: StreamLayout,
    pub max_frame: usize,
}

unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new(layout: StreamLayout, sample_rate: u32) -> Result<Self> {
        let a = api()?;
        let mut err = 0;
        let st = unsafe { (a.dec_create)(sample_rate as i32, layout.channels as c_int, layout.streams as c_int, layout.coupled as c_int, layout.mapping.as_ptr(), &mut err) };
        if st.is_null() || err != 0 {
            bail!("opus_multistream_decoder_create failed ({err})");
        }
        Ok(Self { st, max_frame: (sample_rate / 50) as usize * 6, layout })
    }

    /// Decode a packet (or `None` for packet-loss concealment of `conceal_samples`).
    /// Returns interleaved samples.
    pub fn decode(&mut self, packet: Option<&[u8]>, conceal_samples: usize, out: &mut Vec<i16>) -> Result<usize> {
        let ch = self.layout.channels as usize;
        out.resize(self.max_frame * ch, 0);
        let (ptr, len, frames) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32, self.max_frame),
            None => (std::ptr::null(), 0, conceal_samples.min(self.max_frame)),
        };
        let n = unsafe { (api()?.decode)(self.st, ptr, len, out.as_mut_ptr(), frames as c_int, 0) };
        if n < 0 {
            bail!("opus decode error {n}");
        }
        out.truncate(n as usize * ch);
        Ok(n as usize)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.dec_destroy)(self.st) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_sdp() {
        assert_eq!(StreamLayout::stereo().to_sdp(), "21101");
        assert_eq!(StreamLayout::from_sdp("21101"), Some(StreamLayout::stereo()));
        assert_eq!(StreamLayout::from_sdp("642012453"), Some(StreamLayout::surround51()));
        assert_eq!(StreamLayout::surround71().to_sdp(), "85301245367");
    }

    #[test]
    fn surround_roundtrip_keeps_channels_apart() {
        for layout in [StreamLayout::surround51(), StreamLayout::surround71()] {
            let ch = layout.channels as usize;
            let mut enc = OpusEncoder::new(layout.clone(), 48000, 5, 96_000 * ch as i32 / 2, true).unwrap();
            let mut dec = OpusDecoder::new(layout.clone(), 48000).unwrap();
            let (mut pkt, mut out) = (Vec::new(), Vec::new());
            let mut energy = vec![0f64; ch];
            for f in 0..200 {
                // Tone only on channel 2 (front centre).
                let pcm: Vec<i16> = (0..240 * ch).map(|i| if i % ch == 2 { (((f * 240 + i / ch) as f64 / 48000.0 * 880.0 * std::f64::consts::TAU).sin() * 8000.0) as i16 } else { 0 }).collect();
                enc.encode(&pcm, &mut pkt).unwrap();
                dec.decode(Some(&pkt), 0, &mut out).unwrap();
                if f > 20 {
                    for (i, s) in out.iter().enumerate() {
                        energy[i % ch] += (*s as f64).powi(2);
                    }
                }
            }
            let loudest = (0..ch).max_by(|a, b| energy[*a].total_cmp(&energy[*b])).unwrap();
            assert_eq!(loudest, 2, "{ch}ch energy {energy:?}");
        }
    }

    #[test]
    fn encode_decode_sine() {
        let mut enc = OpusEncoder::new(StreamLayout::stereo(), 48000, 5, 256_000, true).unwrap();
        let mut dec = OpusDecoder::new(StreamLayout::stereo(), 48000).unwrap();
        let mut sizes = std::collections::HashSet::new();
        let mut energy = 0f64;
        let mut pkt = Vec::new();
        let mut out = Vec::new();
        for f in 0..100 {
            let pcm: Vec<i16> = (0..240).flat_map(|i| {
                let t = (f * 240 + i) as f64 / 48000.0;
                let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
                [s, s]
            }).collect();
            enc.encode(&pcm, &mut pkt).unwrap();
            sizes.insert(pkt.len());
            let n = dec.decode(Some(&pkt), 0, &mut out).unwrap();
            assert_eq!(n, 240);
            if f > 10 {
                energy += out.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
            }
        }
        assert_eq!(sizes.len(), 1, "CBR packets must be constant size: {sizes:?}");
        assert!(energy > 1e9, "decoded audio is silent");
        // concealment
        assert_eq!(dec.decode(None, 240, &mut out).unwrap(), 240);
    }
}
