//! Software H.264 encode/decode backed by OpenH264.

use anyhow::{anyhow, Result};
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, SpsPpsStrategy, UsageType};
use openh264::formats::{YUVSlices, YUVSource};
use openh264::OpenH264API;

use crate::frame::I420Frame;

pub struct H264Encoder {
    enc: Encoder,
    force_idr: bool,
    out: Vec<u8>,
}

pub struct EncodedFrame<'a> {
    pub data: &'a [u8],
    pub idr: bool,
}

impl H264Encoder {
    pub fn new(fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4) as u16;
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .complexity(Complexity::Low)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .num_threads(threads);
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| anyhow!("openh264 encoder: {e}"))?;
        Ok(Self { enc, force_idr: true, out: Vec::with_capacity(1 << 20) })
    }

    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    pub fn encode(&mut self, f: &I420Frame) -> Result<EncodedFrame<'_>> {
        if self.force_idr {
            self.enc.force_intra_frame();
            self.force_idr = false;
        }
        if f.yuv444 {
            return Err(anyhow!("OpenH264 cannot encode 4:4:4"));
        }
        let cw = f.chroma_width();
        let src = YUVSlices::new((&f.y, &f.u, &f.v), (f.width, f.height), (f.width, cw, cw));
        let bs = self.enc.encode(&src).map_err(|e| anyhow!("encode: {e}"))?;
        let idr = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
        self.out.clear();
        bs.write_vec(&mut self.out);
        Ok(EncodedFrame { data: &self.out, idr })
    }
}

pub struct H264Decoder {
    dec: Decoder,
    frame: I420Frame,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        let cfg = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let dec = Decoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| anyhow!("openh264 decoder: {e}"))?;
        Ok(Self { dec, frame: I420Frame::new(2, 2) })
    }

    /// Decode one access unit (Annex-B). Returns the picture if one is ready.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<&I420Frame>> {
        let pic = self.dec.decode(au).map_err(|e| anyhow!("decode: {e}"))?;
        let Some(pic) = pic else { return Ok(None) };
        let (w, h) = pic.dimensions();
        let (sy, su, sv) = pic.strides();
        self.frame.copy_from_planes(w, h, [pic.y(), pic.u(), pic.v()], [sy, su, sv]);
        Ok(Some(&self.frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (320, 240);
        let mut enc = H264Encoder::new(60, 4000).unwrap();
        let mut dec = H264Decoder::new().unwrap();
        let mut decoded = 0;
        for i in 0..30usize {
            let mut bgrx = vec![0u8; w * h * 4];
            for (p, px) in bgrx.chunks_exact_mut(4).enumerate() {
                let x = p % w;
                let y = p / w;
                px.copy_from_slice(&[((x + i * 4) % 256) as u8, (y % 256) as u8, 128, 255]);
            }
            let mut f = I420Frame::new(w, h);
            f.from_bgrx(&bgrx, w, h, w * 4);
            let e = enc.encode(&f).unwrap();
            assert_eq!(e.idr, i == 0, "frame {i}");
            let data = e.data.to_vec();
            assert!(data.starts_with(&[0, 0, 0, 1]));
            if let Some(out) = dec.decode(&data).unwrap() {
                assert_eq!((out.width, out.height), (w, h));
                // luma should be close to the source
                let err: u64 = out.y.iter().zip(&f.y).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64).sum();
                assert!(err / (w * h) as u64 <= 6, "mean luma error too high at frame {i}");
                decoded += 1;
            }
        }
        assert!(decoded >= 28);
    }
}
