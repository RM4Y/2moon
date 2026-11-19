//! Planar I420 frames and fast colour conversion (BT.601 limited range,
//! the default colourimetry of GameStream H.264 streams).

use rayon::prelude::*;

/// YUV matrix of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Matrix {
    #[default]
    Bt601,
    Bt709,
    Bt2020,
}

/// Colour space conversion parameters (matrix + range).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Csc {
    pub matrix: Matrix,
    pub full_range: bool,
}

impl Csc {
    /// GameStream `encoderCscMode`: bit 0 = full range, bits 1.. = 0 BT.601, 1 BT.709, 2 BT.2020.
    pub fn from_mode(mode: u32) -> Self {
        let matrix = match mode >> 1 {
            1 => Matrix::Bt709,
            2 => Matrix::Bt2020,
            _ => Matrix::Bt601,
        };
        Self { matrix, full_range: mode & 1 != 0 }
    }

    pub fn mode(&self) -> u32 {
        (match self.matrix {
            Matrix::Bt601 => 0,
            Matrix::Bt709 => 1,
            Matrix::Bt2020 => 2,
        } << 1)
            | self.full_range as u32
    }

    fn kr_kb(&self) -> (f64, f64) {
        match self.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// RGB -> YUV fixed-point (16.16) coefficients: rows Y, U, V; plus Y offset.
    fn forward(&self) -> ([[i32; 3]; 3], i32) {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if self.full_range { (255.0, 255.0, 0) } else { (219.0, 224.0, 16) };
        let f = |x: f64| (x * 65536.0 / 255.0).round() as i32;
        let y = [f(kr * ys), f(kg * ys), f(kb * ys)];
        let u = [f(-kr / (2.0 * (1.0 - kb)) * cs), f(-kg / (2.0 * (1.0 - kb)) * cs), f(0.5 * cs)];
        let v = [f(0.5 * cs), f(-kg / (2.0 * (1.0 - kr)) * cs), f(-kb / (2.0 * (1.0 - kr)) * cs)];
        ([y, u, v], yo)
    }

    /// YUV -> RGB fixed-point (16.16): (y scale, r_v, g_u, g_v, b_u, y offset).
    fn inverse(&self) -> (i32, i32, i32, i32, i32, i32) {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if self.full_range { (255.0, 255.0, 0) } else { (219.0, 224.0, 16) };
        let f = |x: f64| (x * 65536.0).round() as i32;
        let y = f(255.0 / ys);
        let c = 255.0 / cs;
        (y, f(2.0 * (1.0 - kr) * c), f(-2.0 * (1.0 - kb) * kb / kg * c), f(-2.0 * (1.0 - kr) * kr / kg * c), f(2.0 * (1.0 - kb) * c), yo)
    }

    /// RGB -> YUV for normalised (0..1) samples, for GPU shaders: rows Y, U, V,
    /// each `[r, g, b, offset]` (`out = dot(rgb, row.rgb) + row.a`).
    pub fn yuv_rows(&self) -> [[f32; 4]; 3] {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if self.full_range { (255.0, 255.0, 0.0) } else { (219.0, 224.0, 16.0) };
        let (ys, cs) = (ys / 255.0, cs / 255.0);
        let half = 128.0 / 255.0;
        [
            [(kr * ys) as f32, (kg * ys) as f32, (kb * ys) as f32, (yo / 255.0) as f32],
            [(-kr / (2.0 * (1.0 - kb)) * cs) as f32, (-kg / (2.0 * (1.0 - kb)) * cs) as f32, (0.5 * cs) as f32, half as f32],
            [(0.5 * cs) as f32, (-kg / (2.0 * (1.0 - kr)) * cs) as f32, (-kb / (2.0 * (1.0 - kr)) * cs) as f32, half as f32],
        ]
    }

    /// YUV -> RGB for normalised (0..1) samples, for GPU shaders:
    /// `rgb = M * (yuv - offset)`, `M` given column-major (GLSL `mat3` order).
    pub fn rgb_matrix(&self) -> ([f32; 9], [f32; 3]) {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yo) = if self.full_range { (255.0, 255.0, 0.0) } else { (219.0, 224.0, 16.0) };
        let y = 255.0 / ys;
        let c = 255.0 / cs;
        let (rv, gu, gv, bu) = (2.0 * (1.0 - kr) * c, -2.0 * (1.0 - kb) * kb / kg * c, -2.0 * (1.0 - kr) * kr / kg * c, 2.0 * (1.0 - kb) * c);
        // columns: Y, U, V contributions to (R, G, B)
        let m = [y, y, y, 0.0, gu, bu, rv, gv, 0.0];
        (m.map(|v: f64| v as f32), [yo / 255.0, 128.0 / 255.0, 128.0 / 255.0])
    }
}

/// Planar YUV picture: 4:2:0 (chroma planes half size in both directions, the
/// GameStream default) or 4:4:4 (full-size chroma, `yuv444`).
#[derive(Clone, Debug)]
pub struct I420Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Colour space used by the conversions to/from RGB.
    pub csc: Csc,
    /// Full-resolution chroma (4:4:4) instead of 4:2:0.
    pub yuv444: bool,
}

impl I420Frame {
    pub fn new(width: usize, height: usize) -> Self {
        Self::with_format(width, height, false)
    }

    pub fn with_format(width: usize, height: usize, yuv444: bool) -> Self {
        let (cw, ch) = if yuv444 { (width, height) } else { (width.div_ceil(2), height.div_ceil(2)) };
        Self { width, height, y: vec![16; width * height], u: vec![128; cw * ch], v: vec![128; cw * ch], csc: Csc::default(), yuv444 }
    }

    pub fn chroma_width(&self) -> usize {
        if self.yuv444 { self.width } else { self.width.div_ceil(2) }
    }

    pub fn chroma_height(&self) -> usize {
        if self.yuv444 { self.height } else { self.height.div_ceil(2) }
    }

    /// Reallocate for another size / chroma format, keeping the colour space.
    fn reshape(&mut self, width: usize, height: usize, yuv444: bool) {
        if self.width != width || self.height != height || self.yuv444 != yuv444 {
            let csc = self.csc;
            *self = Self::with_format(width, height, yuv444);
            self.csc = csc;
        }
    }

    /// Convert from packed 32-bit BGRX/BGRA (little-endian `0xAARRGGBB` words) with the given stride in bytes.
    pub fn from_bgrx(&mut self, src: &[u8], width: usize, height: usize, stride: usize) {
        assert!(src.len() >= stride * (height - 1) + width * 4);
        self.reshape(width, height, self.yuv444);
        let ([cy_, cu_, cv_], yo) = self.csc.forward();
        if self.yuv444 {
            self.y.par_chunks_mut(width).zip(self.u.par_chunks_mut(width).zip(self.v.par_chunks_mut(width))).enumerate().for_each(|(row, (yrow, (urow, vrow)))| {
                let s = &src[row * stride..row * stride + width * 4];
                for (x, px) in s.chunks_exact(4).enumerate() {
                    let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
                    yrow[x] = (((cy_[0] * r + cy_[1] * g + cy_[2] * b + 32768) >> 16) + yo).clamp(0, 255) as u8;
                    urow[x] = (((cu_[0] * r + cu_[1] * g + cu_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                    vrow[x] = (((cv_[0] * r + cv_[1] * g + cv_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                }
            });
            return;
        }
        let cw = self.chroma_width();
        // Process two source rows at a time: one chroma row.
        self.y
            .par_chunks_mut(width * 2)
            .zip(self.u.par_chunks_mut(cw).zip(self.v.par_chunks_mut(cw)))
            .enumerate()
            .for_each(|(cy, (yrows, (urow, vrow)))| {
                let r0 = cy * 2;
                let rows = if r0 + 1 < height { 2 } else { 1 };
                for dy in 0..rows {
                    let s = &src[(r0 + dy) * stride..(r0 + dy) * stride + width * 4];
                    let yrow = &mut yrows[dy * width..dy * width + width];
                    for (x, px) in s.chunks_exact(4).enumerate() {
                        let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
                        yrow[x] = (((cy_[0] * r + cy_[1] * g + cy_[2] * b + 32768) >> 16) + yo).clamp(0, 255) as u8;
                    }
                }
                for cx in 0..cw {
                    let mut acc = [0i32; 3];
                    let mut n = 0;
                    for dy in 0..rows {
                        for dx in 0..2 {
                            let x = cx * 2 + dx;
                            if x < width {
                                let o = (r0 + dy) * stride + x * 4;
                                acc[0] += src[o] as i32;
                                acc[1] += src[o + 1] as i32;
                                acc[2] += src[o + 2] as i32;
                                n += 1;
                            }
                        }
                    }
                    let (b, g, r) = (acc[0] / n, acc[1] / n, acc[2] / n);
                    urow[cx] = (((cu_[0] * r + cu_[1] * g + cu_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                    vrow[cx] = (((cv_[0] * r + cv_[1] * g + cv_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                }
            });
    }

    /// Convert a packed 32-bit RGB image of any size to this frame's `dw x dh`,
    /// resampling with a 2x2 box filter at the mapped position (cheap and
    /// reasonably alias-free for typical downscales). `rgb_order` is true for
    /// R,G,B,X byte order and false for B,G,R,X.
    #[allow(clippy::too_many_arguments)]
    pub fn from_rgb32_scaled(&mut self, src: &[u8], sw: usize, sh: usize, stride: usize, rgb_order: bool, dw: usize, dh: usize) {
        if sw == dw && sh == dh && !rgb_order {
            return self.from_bgrx(src, sw, sh, stride);
        }
        self.reshape(dw, dh, self.yuv444);
        let ([cy_, cu_, cv_], yo) = self.csc.forward();
        let (ri, bi) = if rgb_order { (0, 2) } else { (2, 0) };
        let xmap: Vec<usize> = (0..dw).map(|x| ((x * 2 + 1) * sw / (dw * 2)).min(sw - 1)).collect();
        let ymap: Vec<usize> = (0..dh).map(|y| ((y * 2 + 1) * sh / (dh * 2)).min(sh - 1)).collect();
        let px = |x: usize, y: usize| -> (i32, i32, i32) {
            // average the pixel with its right/bottom neighbours
            let x1 = (x + 1).min(sw - 1);
            let y1 = (y + 1).min(sh - 1);
            let mut acc = (0i32, 0i32, 0i32);
            for (xx, yy) in [(x, y), (x1, y), (x, y1), (x1, y1)] {
                let o = yy * stride + xx * 4;
                acc.0 += src[o + ri] as i32;
                acc.1 += src[o + 1] as i32;
                acc.2 += src[o + bi] as i32;
            }
            (acc.0 >> 2, acc.1 >> 2, acc.2 >> 2)
        };
        if self.yuv444 {
            self.y.par_chunks_mut(dw).zip(self.u.par_chunks_mut(dw).zip(self.v.par_chunks_mut(dw))).enumerate().for_each(|(row, (yrow, (urow, vrow)))| {
                let sy = ymap[row];
                for x in 0..dw {
                    let (r, g, b) = px(xmap[x], sy);
                    yrow[x] = (((cy_[0] * r + cy_[1] * g + cy_[2] * b + 32768) >> 16) + yo).clamp(0, 255) as u8;
                    urow[x] = (((cu_[0] * r + cu_[1] * g + cu_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                    vrow[x] = (((cv_[0] * r + cv_[1] * g + cv_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                }
            });
            return;
        }
        let cw = self.chroma_width();
        self.y
            .par_chunks_mut(dw * 2)
            .zip(self.u.par_chunks_mut(cw).zip(self.v.par_chunks_mut(cw)))
            .enumerate()
            .for_each(|(cy, (yrows, (urow, vrow)))| {
                let rows = if cy * 2 + 1 < dh { 2 } else { 1 };
                let mut csum = vec![(0i32, 0i32, 0i32, 0i32); cw];
                for dy in 0..rows {
                    let sy = ymap[cy * 2 + dy];
                    let yrow = &mut yrows[dy * dw..dy * dw + dw];
                    for x in 0..dw {
                        let (r, g, b) = px(xmap[x], sy);
                        yrow[x] = (((cy_[0] * r + cy_[1] * g + cy_[2] * b + 32768) >> 16) + yo).clamp(0, 255) as u8;
                        let c = &mut csum[x / 2];
                        c.0 += r;
                        c.1 += g;
                        c.2 += b;
                        c.3 += 1;
                    }
                }
                for (cx, c) in csum.iter().enumerate() {
                    let n = c.3.max(1);
                    let (r, g, b) = (c.0 / n, c.1 / n, c.2 / n);
                    urow[cx] = (((cu_[0] * r + cu_[1] * g + cu_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                    vrow[cx] = (((cv_[0] * r + cv_[1] * g + cv_[2] * b + 32768) >> 16) + 128).clamp(0, 255) as u8;
                }
            });
    }

    /// Convert to packed `0x00RRGGBB` pixels (the format softbuffer expects).
    pub fn to_xrgb(&self, dst: &mut [u32]) {
        let (w, h) = (self.width, self.height);
        assert!(dst.len() >= w * h);
        let cw = self.chroma_width();
        let shift = if self.yuv444 { 0 } else { 1 };
        let (ky, krv, kgu, kgv, kbu, yo) = self.csc.inverse();
        dst[..w * h].par_chunks_mut(w).enumerate().for_each(|(row, out)| {
            let yrow = &self.y[row * w..row * w + w];
            let cr = row >> shift;
            let urow = &self.u[cr * cw..cr * cw + cw];
            let vrow = &self.v[cr * cw..cr * cw + cw];
            for x in 0..w {
                let c = ky * (yrow[x] as i32 - yo);
                let d = urow[x >> shift] as i32 - 128;
                let e = vrow[x >> shift] as i32 - 128;
                let r = ((c + krv * e + 32768) >> 16).clamp(0, 255) as u32;
                let g = ((c + kgu * d + kgv * e + 32768) >> 16).clamp(0, 255) as u32;
                let b = ((c + kbu * d + 32768) >> 16).clamp(0, 255) as u32;
                out[x] = (r << 16) | (g << 8) | b;
            }
        });
    }

    /// Copy another frame's content, reusing this frame's allocations.
    pub fn copy_from(&mut self, o: &I420Frame) {
        self.width = o.width;
        self.height = o.height;
        self.csc = o.csc;
        self.yuv444 = o.yuv444;
        self.y.clone_from(&o.y);
        self.u.clone_from(&o.u);
        self.v.clone_from(&o.v);
    }

    /// Copy from NV12 (luma plane + interleaved U/V plane), e.g. GPU decoder output.
    pub fn copy_from_nv12(&mut self, w: usize, h: usize, y: &[u8], y_stride: usize, uv: &[u8], uv_stride: usize) {
        self.reshape(w, h, false);
        let cw = self.chroma_width();
        for r in 0..h {
            self.y[r * w..r * w + w].copy_from_slice(&y[r * y_stride..r * y_stride + w]);
        }
        for (r, (du, dv)) in self.u.chunks_exact_mut(cw).zip(self.v.chunks_exact_mut(cw)).enumerate() {
            let src = &uv[r * uv_stride..r * uv_stride + cw * 2];
            for ((pair, u), v) in src.chunks_exact(2).zip(du.iter_mut()).zip(dv.iter_mut()) {
                *u = pair[0];
                *v = pair[1];
            }
        }
    }

    /// Copy from strided 4:2:0 planes (e.g. decoder output).
    pub fn copy_from_planes(&mut self, w: usize, h: usize, planes: [&[u8]; 3], strides: [usize; 3]) {
        self.copy_from_planes_fmt(w, h, false, planes, strides)
    }

    /// Copy from strided planes, 4:2:0 or 4:4:4.
    pub fn copy_from_planes_fmt(&mut self, w: usize, h: usize, yuv444: bool, planes: [&[u8]; 3], strides: [usize; 3]) {
        self.reshape(w, h, yuv444);
        let (cw, ch) = (self.chroma_width(), self.chroma_height());
        for r in 0..h {
            self.y[r * w..r * w + w].copy_from_slice(&planes[0][r * strides[0]..r * strides[0] + w]);
        }
        for r in 0..ch {
            self.u[r * cw..r * cw + cw].copy_from_slice(&planes[1][r * strides[1]..r * strides[1] + cw]);
            self.v[r * cw..r * cw + cw].copy_from_slice(&planes[2][r * strides[2]..r * strides[2] + cw]);
        }
    }

    /// Copy from packed 8-bit 4:4:4 with bytes V, U, Y, A/X per pixel (FFmpeg VUYX /
    /// VUYA, DXGI AYUV): what Intel GPUs output for HEVC 4:4:4 (VAAPI, D3D11VA).
    pub fn copy_from_vuyx(&mut self, w: usize, h: usize, src: &[u8], stride: usize) {
        self.reshape(w, h, true);
        for r in 0..h {
            let row = &src[r * stride..r * stride + w * 4];
            let (y, u, v) = (&mut self.y[r * w..r * w + w], &mut self.u[r * w..r * w + w], &mut self.v[r * w..r * w + w]);
            for (i, px) in row.chunks_exact(4).enumerate() {
                v[i] = px[0];
                u[i] = px[1];
                y[i] = px[2];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_roundtrip() {
        for mode in 0..6 {
            roundtrip(Csc::from_mode(mode));
        }
        // BT.601 limited white/black reference levels
        let mut f = I420Frame::new(2, 2);
        f.from_bgrx(&[255u8; 16], 2, 2, 8);
        assert_eq!((f.y[0], f.u[0], f.v[0]), (235, 128, 128));
        f.csc = Csc::from_mode(3); // BT.709 full
        f.from_bgrx(&[255u8; 16], 2, 2, 8);
        assert_eq!((f.y[0], f.u[0], f.v[0]), (255, 128, 128));
    }

    /// The GPU shader matrix must agree with the CPU conversion.
    #[test]
    fn shader_matrix_matches_cpu() {
        for mode in 0..6 {
            let csc = Csc::from_mode(mode);
            let (m, off) = csc.rgb_matrix();
            for (y, u, v) in [(16u8, 128u8, 128u8), (235, 128, 128), (81, 90, 240), (145, 54, 34), (41, 240, 110), (128, 60, 200)] {
                let mut f = I420Frame::new(2, 2);
                f.csc = csc;
                f.y.fill(y);
                f.u.fill(u);
                f.v.fill(v);
                let mut out = [0u32; 4];
                f.to_xrgb(&mut out);
                let d = [y as f32 / 255.0 - off[0], u as f32 / 255.0 - off[1], v as f32 / 255.0 - off[2]];
                for (c, shift) in [(0, 16), (1, 8), (2, 0)] {
                    let gpu = ((m[c] * d[0] + m[3 + c] * d[1] + m[6 + c] * d[2]).clamp(0.0, 1.0) * 255.0).round() as i32;
                    let cpu = ((out[0] >> shift) & 0xff) as i32;
                    assert!((gpu - cpu).abs() <= 1, "mode {mode} yuv ({y},{u},{v}) channel {c}: gpu {gpu} cpu {cpu}");
                }
            }
        }
    }

    /// 4:4:4 keeps full-resolution chroma: a one-pixel colour pattern survives exactly.
    #[test]
    fn yuv444_keeps_chroma_detail() {
        let (w, h) = (16, 8);
        let mut src = vec![0u8; w * h * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            // alternating red / blue columns: lost entirely by 4:2:0 subsampling
            px.copy_from_slice(if i % 2 == 0 { &[0, 0, 220, 255] } else { &[220, 0, 0, 255] });
        }
        let mut f = I420Frame::with_format(w, h, true);
        f.from_bgrx(&src, w, h, w * 4);
        assert_eq!((f.u.len(), f.chroma_width(), f.chroma_height()), (w * h, w, h));
        let mut out = vec![0u32; w * h];
        f.to_xrgb(&mut out);
        for (i, o) in out.iter().enumerate() {
            let (r, b) = ((o >> 16) & 0xff, o & 0xff);
            if i % 2 == 0 {
                assert!(r > 180 && b < 40, "pixel {i}: {o:06x}");
            } else {
                assert!(b > 180 && r < 40, "pixel {i}: {o:06x}");
            }
        }
    }

    fn roundtrip(csc: Csc) {
        let (w, h) = (64, 34);
        let mut src = vec![0u8; w * h * 4];
        for (i, px) in src.chunks_exact_mut(4).enumerate() {
            // smooth gradient so 2x2 chroma subsampling is near-lossless
            let x = (i % w) as u8;
            let y = (i / w) as u8;
            px.copy_from_slice(&[x * 3, 100, y * 5, 255]);
        }
        let mut f = I420Frame::new(1, 1);
        f.csc = csc;
        f.from_bgrx(&src, w, h, w * 4);
        let mut out = vec![0u32; w * h];
        f.to_xrgb(&mut out);
        let mut max_err = 0;
        for (i, px) in src.chunks_exact(4).enumerate() {
            let o = out[i];
            let (r, g, b) = ((o >> 16) as u8, (o >> 8) as u8, o as u8);
            max_err = max_err.max((r as i32 - px[2] as i32).abs()).max((g as i32 - px[1] as i32).abs()).max((b as i32 - px[0] as i32).abs());
        }
        assert!(max_err <= 12, "{csc:?}: max error {max_err}");
    }
}

#[cfg(test)]
mod vuyx_tests {
    use super::I420Frame;

    #[test]
    fn packed_vuyx_to_planar_444() {
        // 2x2 picture, stride 12 (4 bytes of padding per row): bytes V, U, Y, X.
        let src = [10, 20, 30, 0, 11, 21, 31, 0, 99, 99, 99, 99, 12, 22, 32, 0, 13, 23, 33, 0, 99, 99, 99, 99];
        let mut f = I420Frame::new(2, 2);
        f.copy_from_vuyx(2, 2, &src, 12);
        assert!(f.yuv444);
        assert_eq!((f.y.as_slice(), f.u.as_slice(), f.v.as_slice()), (&[30, 31, 32, 33][..], &[20, 21, 22, 23][..], &[10, 11, 12, 13][..]));
    }
}
