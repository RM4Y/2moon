//! Interleaved f32 audio: channel mapping and linear resampling, used where
//! the audio device does not run at the stream's rate / channel count.

/// Linear resampler + channel mapper from `in_ch`@`in_rate` to `out_ch`@`out_rate`.
pub struct Converter {
    in_ch: usize,
    out_ch: usize,
    step: f64,
    pos: f64,
    prev: Vec<f32>,
}

impl Converter {
    pub fn new(in_ch: usize, in_rate: u32, out_ch: usize, out_rate: u32) -> Self {
        Self { in_ch, out_ch, step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: vec![0.0; in_ch] }
    }

    /// Map one input frame to the output channel layout (stereo pairs kept,
    /// surround folded to the front pair, mono duplicated / averaged).
    fn map(&self, frame: &[f32], out: &mut Vec<f32>) {
        match (self.in_ch, self.out_ch) {
            (a, b) if a == b => out.extend_from_slice(frame),
            (1, n) => out.extend(std::iter::repeat_n(frame[0], n)),
            (a, 1) => out.push(frame.iter().take(a.min(2)).sum::<f32>() / a.min(2) as f32),
            // 7.1 to 5.1: the first six channels are the same speakers.
            (a, b) if a > b && b > 2 => out.extend_from_slice(&frame[..b]),
            (_, n) => {
                out.push(frame[0]);
                out.push(frame[1]);
                out.extend(std::iter::repeat_n(0.0, n - 2));
            }
        }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let frames = input.len() / self.in_ch;
        if (self.step - 1.0).abs() < 1e-9 {
            for f in input.chunks_exact(self.in_ch) {
                self.map(f, out);
            }
            return;
        }
        let mut tmp = vec![0.0; self.in_ch];
        while self.pos < frames as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..self.in_ch {
                let a = if i == 0 { self.prev[c] } else { input[(i - 1) * self.in_ch + c] };
                let b = input[i * self.in_ch + c];
                tmp[c] = a + (b - a) * t;
            }
            self.map(&tmp, out);
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.prev.copy_from_slice(&input[(frames - 1) * self.in_ch..frames * self.in_ch]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_duration_and_shape() {
        // 48 kHz stereo -> 44.1 kHz stereo: 480 frames become ~441.
        let mut c = Converter::new(2, 48_000, 2, 44_100);
        let input: Vec<f32> = (0..480).flat_map(|i| [i as f32 / 480.0, -(i as f32) / 480.0]).collect();
        let mut out = Vec::new();
        c.push(&input, &mut out);
        let frames = out.len() / 2;
        assert!((440..=442).contains(&frames), "{frames}");
        assert!(out.windows(4).step_by(2).all(|w| w[2] >= w[0]), "ramp must stay monotonic");
    }
}
