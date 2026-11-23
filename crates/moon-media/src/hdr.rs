//! HDR10 helpers (BT.2020, PQ, 10-bit). The SDR desktop is placed in HDR10 with
//! SDR white at 203 nits; the client maps it back to SDR.

use rayon::prelude::*;

/// Reference white of SDR content inside HDR10 (BT.2408).
pub const SDR_WHITE_NITS: f32 = 203.0;

pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

const M1: f32 = 2610.0 / 16384.0;
const M2: f32 = 2523.0 / 4096.0 * 128.0;
const C1: f32 = 3424.0 / 4096.0;
const C2: f32 = 2413.0 / 4096.0 * 32.0;
const C3: f32 = 2392.0 / 4096.0 * 32.0;

/// PQ inverse EOTF: linear light (1.0 = 10000 nits) -> signal.
pub fn pq_oetf(l: f32) -> f32 {
    let p = l.max(0.0).powf(M1);
    ((C1 + C2 * p) / (1.0 + C3 * p)).powf(M2)
}

/// PQ EOTF: signal -> linear light (1.0 = 10000 nits).
pub fn pq_eotf(e: f32) -> f32 {
    let p = e.clamp(0.0, 1.0).powf(1.0 / M2);
    ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

/// Linear RGB, BT.709 primaries -> BT.2020 primaries (row-major).
pub const BT709_TO_BT2020: [[f32; 3]; 3] = [[0.627_404, 0.329_283, 0.043_313], [0.069_097, 0.919_540, 0.011_362], [0.016_391, 0.088_013, 0.895_595]];
/// Linear RGB, BT.2020 primaries -> BT.709 primaries (row-major).
pub const BT2020_TO_BT709: [[f32; 3]; 3] = [[1.660_491, -0.587_641, -0.072_850], [-0.124_550, 1.132_9, -0.008_349], [-0.018_151, -0.100_579, 1.118_73]];

fn mul(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

/// Knee of the HDR -> SDR curve: below it, light is left untouched.
pub const TONE_KNEE: f32 = 0.75;

/// HDR -> SDR tone curve (`x`: linear, 1.0 = SDR white; `peak`: content maximum).
/// Unchanged below [`TONE_KNEE`], highlights roll off towards white. Same curve in the shader.
pub fn tone_map(x: f32, peak: f32) -> f32 {
    if peak <= 1.0 || x <= TONE_KNEE {
        return x.min(1.0);
    }
    1.0 - (1.0 - TONE_KNEE) * (-(x - TONE_KNEE) / (1.0 - TONE_KNEE)).exp()
}

/// HDR10 static metadata sent by the host in HDR mode
/// control message (all little-endian u16; chromaticities in 1/50000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrMetadata {
    pub primaries: [(u16, u16); 3],
    pub white_point: (u16, u16),
    pub max_display_luminance: u16,
    /// 1/10000 nit.
    pub min_display_luminance: u16,
    pub max_content_light_level: u16,
    pub max_frame_average_light_level: u16,
    pub max_full_frame_luminance: u16,
}

impl HdrMetadata {
    /// BT.2020 primaries, D65, for SDR content at 203 nits in a 1000-nit container.
    pub fn sdr_in_hdr10() -> Self {
        Self {
            primaries: [(35400, 14600), (8500, 39850), (6550, 2300)],
            white_point: (15635, 16450),
            max_display_luminance: 1000,
            min_display_luminance: 50,
            max_content_light_level: SDR_WHITE_NITS as u16,
            max_frame_average_light_level: SDR_WHITE_NITS as u16,
            max_full_frame_luminance: SDR_WHITE_NITS as u16,
        }
    }

    /// The HDR mode control message payload: `enabled` byte + metadata.
    pub fn message(enabled: bool, m: Option<&Self>) -> Vec<u8> {
        let mut v = vec![enabled as u8];
        let m = m.copied().unwrap_or(Self { primaries: [(0, 0); 3], white_point: (0, 0), max_display_luminance: 0, min_display_luminance: 0, max_content_light_level: 0, max_frame_average_light_level: 0, max_full_frame_luminance: 0 });
        for (x, y) in m.primaries.iter().chain([&m.white_point]) {
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
        }
        for n in [m.max_display_luminance, m.min_display_luminance, m.max_content_light_level, m.max_frame_average_light_level, m.max_full_frame_luminance] {
            v.extend_from_slice(&n.to_le_bytes());
        }
        v
    }

    /// Parse the HDR mode message: (enabled, metadata if present).
    pub fn parse(p: &[u8]) -> Option<(bool, Option<Self>)> {
        let enabled = *p.first()? != 0;
        if p.len() < 27 {
            return Some((enabled, None));
        }
        let u = |i: usize| u16::from_le_bytes([p[1 + i * 2], p[2 + i * 2]]);
        Some((
            enabled,
            Some(Self {
                primaries: [(u(0), u(1)), (u(2), u(3)), (u(4), u(5))],
                white_point: (u(6), u(7)),
                max_display_luminance: u(8),
                min_display_luminance: u(9),
                max_content_light_level: u(10),
                max_frame_average_light_level: u(11),
                max_full_frame_luminance: u(12),
            }),
        ))
    }

    /// Content peak relative to SDR white, for [`tone_map`].
    pub fn peak(&self) -> f32 {
        let nits = if self.max_content_light_level > 0 { self.max_content_light_level } else { self.max_display_luminance };
        if nits == 0 { 1000.0 / SDR_WHITE_NITS } else { nits as f32 / SDR_WHITE_NITS }
    }
}

/// 10-bit 4:2:0 picture in the P010 layout NVENC takes: 16-bit samples with the
/// value in the 10 most significant bits; luma plane, then interleaved U/V.
#[derive(Clone, Debug)]
pub struct P010Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u16>,
    pub uv: Vec<u16>,
}

/// sRGB 8-bit -> HDR10 PQ signal, through BT.709 -> BT.2020 and SDR white at 203 nits.
struct Hdr10Lut {
    lin: [f32; 256],
    /// PQ signal of BT.2020 linear values 0..=1 (scaled to SDR white).
    pq: Vec<f32>,
}

const PQ_LUT: usize = 4096;

fn hdr10_lut() -> &'static Hdr10Lut {
    static L: std::sync::OnceLock<Hdr10Lut> = std::sync::OnceLock::new();
    L.get_or_init(|| Hdr10Lut {
        lin: std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0)),
        pq: (0..=PQ_LUT).map(|i| pq_oetf(i as f32 / PQ_LUT as f32 * SDR_WHITE_NITS / 10000.0)).collect(),
    })
}

impl Hdr10Lut {
    fn pq(&self, l: f32) -> f32 {
        let x = l.clamp(0.0, 1.0) * PQ_LUT as f32;
        let i = (x as usize).min(PQ_LUT - 1);
        let f = x - i as f32;
        self.pq[i] * (1.0 - f) + self.pq[i + 1] * f
    }

    /// (R', G', B') PQ signal of an sRGB pixel.
    fn pixel(&self, r: u8, g: u8, b: u8) -> [f32; 3] {
        let l = mul(&BT709_TO_BT2020, [self.lin[r as usize], self.lin[g as usize], self.lin[b as usize]]);
        l.map(|c| self.pq(c))
    }
}

/// BT.2020 non-constant-luminance Y'CbCr of a PQ R'G'B' pixel (Y in 0..1, Cb/Cr in -0.5..0.5).
fn ycbcr2020(p: [f32; 3]) -> [f32; 3] {
    let y = 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
    [y, (p[2] - y) / 1.8814, (p[0] - y) / 1.4746]
}

fn q10(v: f32, offset: f32, scale: f32) -> u16 {
    ((offset + scale * v).round().clamp(0.0, 1023.0) as u16) << 6
}

impl P010Frame {
    pub fn new(width: usize, height: usize) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        Self { width, height, y: vec![64 << 6; width * height], uv: vec![512 << 6; cw * ch * 2] }
    }

    /// Convert packed 32-bit sRGB (B,G,R,X or R,G,B,X with `rgb_order`) of any size
    /// to HDR10 at this frame's `dw x dh` (nearest-sample scaling, 2x2 chroma average).
    #[allow(clippy::too_many_arguments)]
    pub fn from_rgb32_scaled(&mut self, src: &[u8], sw: usize, sh: usize, stride: usize, rgb_order: bool, dw: usize, dh: usize) {
        if self.width != dw || self.height != dh {
            *self = Self::new(dw, dh);
        }
        let lut = hdr10_lut();
        let (ri, bi) = if rgb_order { (0, 2) } else { (2, 0) };
        let xmap: Vec<usize> = (0..dw).map(|x| (x * sw / dw.max(1)).min(sw - 1)).collect();
        let ymap: Vec<usize> = (0..dh).map(|y| (y * sh / dh.max(1)).min(sh - 1)).collect();
        let cw = dw.div_ceil(2);
        self.y.par_chunks_mut(dw * 2).zip(self.uv.par_chunks_mut(cw * 2)).enumerate().for_each(|(cy, (yrows, uvrow))| {
            let rows = if cy * 2 + 1 < dh { 2 } else { 1 };
            let mut acc = vec![[0f32; 4]; cw];
            for dy in 0..rows {
                let sy = ymap[cy * 2 + dy];
                for x in 0..dw {
                    let o = sy * stride + xmap[x] * 4;
                    let p = lut.pixel(src[o + ri], src[o + 1], src[o + bi]);
                    yrows[dy * dw + x] = q10(0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2], 64.0, 876.0);
                    let a = &mut acc[x / 2];
                    a[0] += p[0];
                    a[1] += p[1];
                    a[2] += p[2];
                    a[3] += 1.0;
                }
            }
            for (cx, a) in acc.iter().enumerate() {
                let n = a[3].max(1.0);
                let [_, cb, cr] = ycbcr2020([a[0] / n, a[1] / n, a[2] / n]);
                uvrow[cx * 2] = q10(cb, 512.0, 896.0);
                uvrow[cx * 2 + 1] = q10(cr, 512.0, 896.0);
            }
        });
    }
}

/// Planar 16-bit YUV 4:2:0 picture decoded from a 10-bit stream, samples
/// MSB-aligned (a 10-bit value v is stored as v << 6), as the GL renderer
/// samples them from 16-bit normalised textures.
#[derive(Clone, Debug)]
pub struct Yuv16Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u16>,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
}

impl Yuv16Frame {
    pub fn new(width: usize, height: usize) -> Self {
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        Self { width, height, y: vec![64 << 6; width * height], u: vec![512 << 6; cw * ch], v: vec![512 << 6; cw * ch] }
    }

    pub fn chroma_width(&self) -> usize {
        self.width.div_ceil(2)
    }

    pub fn chroma_height(&self) -> usize {
        self.height.div_ceil(2)
    }

    fn reshape(&mut self, w: usize, h: usize) {
        if self.width != w || self.height != h {
            *self = Self::new(w, h);
        }
    }

    pub fn copy_from(&mut self, o: &Yuv16Frame) {
        self.width = o.width;
        self.height = o.height;
        self.y.clone_from(&o.y);
        self.u.clone_from(&o.u);
        self.v.clone_from(&o.v);
    }

    /// From P010/P016 (MSB-aligned luma + interleaved chroma), strides in samples.
    pub fn copy_from_p010(&mut self, w: usize, h: usize, y: &[u16], ys: usize, uv: &[u16], uvs: usize) {
        self.reshape(w, h);
        let cw = self.chroma_width();
        for r in 0..h {
            self.y[r * w..r * w + w].copy_from_slice(&y[r * ys..r * ys + w]);
        }
        for (r, (du, dv)) in self.u.chunks_exact_mut(cw).zip(self.v.chunks_exact_mut(cw)).enumerate() {
            for (x, (u, v)) in du.iter_mut().zip(dv.iter_mut()).enumerate() {
                *u = uv[r * uvs + x * 2];
                *v = uv[r * uvs + x * 2 + 1];
            }
        }
    }

    /// From planar 10-bit little-endian samples in the low bits (FFmpeg
    /// yuv420p10le), strides in samples.
    pub fn copy_from_planar10(&mut self, w: usize, h: usize, planes: [&[u16]; 3], strides: [usize; 3]) {
        self.reshape(w, h);
        let (cw, ch) = (self.chroma_width(), self.chroma_height());
        for (dst, src, s, pw, ph) in [(&mut self.y, planes[0], strides[0], w, h), (&mut self.u, planes[1], strides[1], cw, ch), (&mut self.v, planes[2], strides[2], cw, ch)] {
            for r in 0..ph {
                for x in 0..pw {
                    dst[r * pw + x] = src[r * s + x] << 6;
                }
            }
        }
    }

    /// HDR10 -> SDR `0x00RRGGBB` (CPU path; the GL renderer does the same in its shader).
    pub fn to_xrgb_hdr(&self, dst: &mut [u32], peak: f32) {
        let (w, h) = (self.width, self.height);
        assert!(dst.len() >= w * h);
        let cw = self.chroma_width();
        dst[..w * h].par_chunks_mut(w).enumerate().for_each(|(row, out)| {
            for (x, o) in out.iter_mut().enumerate() {
                let c = (row / 2) * cw + x / 2;
                let rgb = hdr10_to_sdr([self.y[row * w + x], self.u[c], self.v[c]], peak);
                *o = ((rgb[0] as u32) << 16) | ((rgb[1] as u32) << 8) | rgb[2] as u32;
            }
        });
    }
}

/// One HDR10 sample (MSB-aligned 16-bit Y, Cb, Cr; BT.2020 limited range, PQ)
/// to 8-bit sRGB for an SDR display.
pub fn hdr10_to_sdr(yuv: [u16; 3], peak: f32) -> [u8; 3] {
    let n = |v: u16| (v >> 6) as f32;
    let y = (n(yuv[0]) - 64.0) / 876.0;
    let cb = (n(yuv[1]) - 512.0) / 896.0;
    let cr = (n(yuv[2]) - 512.0) / 896.0;
    let r = y + 1.4746 * cr;
    let b = y + 1.8814 * cb;
    let g = (y - 0.2627 * r - 0.0593 * b) / 0.6780;
    let lin = [r, g, b].map(|e| pq_eotf(e) * 10000.0 / SDR_WHITE_NITS);
    let l709 = mul(&BT2020_TO_BT709, lin);
    l709.map(|c| (linear_to_srgb(tone_map(c.max(0.0), peak)) * 255.0).round().clamp(0.0, 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_roundtrip() {
        for l in [0.0, 0.0001, 0.0203, 0.1, 0.5, 1.0] {
            assert!((pq_eotf(pq_oetf(l)) - l).abs() < 1e-4 * (1.0 + l * 100.0), "{l}");
        }
        // 203 nits is about 58% PQ signal
        assert!((pq_oetf(0.0203) - 0.58).abs() < 0.01);
    }

    #[test]
    fn tone_curve_is_monotonic_and_bounded() {
        let mut prev = 0.0;
        for i in 0..2000 {
            let x = i as f32 / 100.0;
            let y = tone_map(x, 5.0);
            assert!(y >= prev && y <= 1.0, "{x} -> {y}");
            prev = y;
        }
        assert_eq!(tone_map(0.5, 5.0), 0.5);
        assert_eq!(tone_map(1.4, 1.0), 1.0);
    }

    /// The host converts every captured picture: it must stay well within a frame.
    #[test]
    fn hdr10_conversion_is_fast_enough() {
        let (w, h) = (1920, 1080);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut p = P010Frame::new(w, h);
        p.from_rgb32_scaled(&src, w, h, w * 4, false, w, h); // warm-up (LUT)
        let t = std::time::Instant::now();
        for _ in 0..10 {
            p.from_rgb32_scaled(&src, w, h, w * 4, false, w, h);
        }
        let ms = t.elapsed().as_secs_f64() * 100.0;
        eprintln!("1080p sRGB -> HDR10 P010: {ms:.2} ms per picture");
        assert!(ms < 16.0, "too slow for 60 fps: {ms:.2} ms");
    }

    #[test]
    fn metadata_message_roundtrip() {
        let m = HdrMetadata::sdr_in_hdr10();
        let msg = HdrMetadata::message(true, Some(&m));
        assert_eq!(msg.len(), 27);
        assert_eq!(HdrMetadata::parse(&msg), Some((true, Some(m))));
        assert_eq!(HdrMetadata::parse(&[0]), Some((false, None)));
    }

    /// Host conversion (sRGB -> HDR10) then client conversion (HDR10 -> SDR)
    /// gives the original colours back: SDR content survives the HDR path.
    #[test]
    fn sdr_through_hdr10_and_back() {
        let (w, h) = (8, 2);
        let colours: [[u8; 3]; 8] = [[255, 255, 255], [0, 0, 0], [255, 0, 0], [0, 255, 0], [0, 0, 255], [128, 128, 128], [30, 200, 120], [250, 180, 20]];
        let mut src = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let c = colours[x];
                // BGRX; two identical columns per chroma sample would be needed for exact chroma,
                // so compare each 2x2 block's average colour instead.
                src[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
            }
        }
        // Make chroma blocks uniform: duplicate each colour over 2 columns.
        let mut src2 = vec![0u8; w * 2 * h * 4];
        for y in 0..h {
            for x in 0..w * 2 {
                let o = (y * w + x / 2) * 4;
                src2[(y * w * 2 + x) * 4..(y * w * 2 + x) * 4 + 4].copy_from_slice(&src[o..o + 4]);
            }
        }
        let mut p = P010Frame::new(w * 2, h);
        p.from_rgb32_scaled(&src2, w * 2, h, w * 2 * 4, false, w * 2, h);
        let mut f = Yuv16Frame::new(1, 1);
        f.copy_from_p010(w * 2, h, &p.y, w * 2, &p.uv, w * 2);
        let mut out = vec![0u32; w * 2 * h];
        f.to_xrgb_hdr(&mut out, 1.0);
        for (x, c) in colours.iter().enumerate() {
            let o = out[x * 2];
            let got = [(o >> 16) as u8, (o >> 8) as u8, o as u8];
            // Compare in linear light: 10-bit quantisation in the wider BT.2020 gamut
            // leaves a tiny amount of light (< 1 % of white) in zero channels.
            for i in 0..3 {
                let (g, e) = (srgb_to_linear(got[i] as f32 / 255.0), srgb_to_linear(c[i] as f32 / 255.0));
                assert!((g - e).abs() <= 0.01, "colour {c:?} came back as {got:?}");
            }
        }
    }
}
