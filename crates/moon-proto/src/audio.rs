//! Audio stream wire format: RTP packets carrying Opus frames
//! (payload type 97, timestamp in milliseconds) protected by a 4+2
//! Reed-Solomon block code sent as payload type 127 packets.
//!
//! FEC packet layout:
//! ```text
//!  0..12  RTP header: 0x80, 127, seq (= base + 4 + shard), ts 0, ssrc
//! 12      shard index (0..2)
//! 13      protected payload type (97)
//! 14..16  base sequence number (BE)
//! 16..20  base timestamp (BE)
//! 20..24  ssrc (BE)
//! 24..    parity over the 4 data payloads
//! ```

use std::collections::BTreeMap;

use crate::fec::ReedSolomon;

pub const RTP_HEADER_LEN: usize = 12;
pub const FEC_HEADER_LEN: usize = 12;
pub const PT_AUDIO: u8 = 97;
pub const PT_FEC: u8 = 127;
pub const DATA_SHARDS: usize = 4;
pub const FEC_SHARDS: usize = 2;

/// Fixed parity rows used by GameStream hosts for audio FEC.
pub const PARITY_ROWS: [u8; 8] = [0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c];

fn rs() -> ReedSolomon {
    ReedSolomon::from_parity_rows(DATA_SHARDS, FEC_SHARDS, &PARITY_ROWS).expect("static matrix")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub flags: u8,
    pub packet_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < RTP_HEADER_LEN {
            return None;
        }
        Some(Self {
            flags: p[0],
            packet_type: p[1],
            seq: u16::from_be_bytes([p[2], p[3]]),
            timestamp: u32::from_be_bytes(p[4..8].try_into().unwrap()),
            ssrc: u32::from_be_bytes(p[8..12].try_into().unwrap()),
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        out.push(self.packet_type);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// Sender: wraps Opus packets in RTP and emits FEC every 4 packets.
pub struct AudioPacketizer {
    seq: u16,
    timestamp_ms: u32,
    pub packet_duration_ms: u32,
    block: Vec<Vec<u8>>,
    block_base_seq: u16,
    block_base_ts: u32,
    pub fec: bool,
}

impl AudioPacketizer {
    pub fn new(packet_duration_ms: u32) -> Self {
        Self { seq: 0, timestamp_ms: 0, packet_duration_ms, block: Vec::new(), block_base_seq: 0, block_base_ts: 0, fec: true }
    }

    /// Returns the RTP data packet followed by any FEC packets completed by it.
    pub fn packetize(&mut self, opus: &[u8]) -> Vec<Vec<u8>> {
        if self.block.is_empty() {
            self.block_base_seq = self.seq;
            self.block_base_ts = self.timestamp_ms;
        }
        let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + opus.len());
        RtpHeader { flags: 0x80, packet_type: PT_AUDIO, seq: self.seq, timestamp: self.timestamp_ms, ssrc: 0 }.write(&mut pkt);
        pkt.extend_from_slice(opus);
        let mut out = vec![pkt];
        self.seq = self.seq.wrapping_add(1);
        self.timestamp_ms = self.timestamp_ms.wrapping_add(self.packet_duration_ms);
        if !self.fec {
            return out;
        }
        self.block.push(opus.to_vec());
        if self.block.len() == DATA_SHARDS {
            let len = self.block.iter().map(|b| b.len()).max().unwrap_or(0);
            let shards: Vec<Vec<u8>> = self.block.drain(..).map(|mut b| {
                b.resize(len, 0);
                b
            }).collect();
            let mut parity = vec![vec![0u8; len]; FEC_SHARDS];
            {
                let d: Vec<&[u8]> = shards.iter().map(|s| s.as_slice()).collect();
                let mut p: Vec<&mut [u8]> = parity.iter_mut().map(|s| s.as_mut_slice()).collect();
                rs().encode(&d, &mut p).expect("audio fec");
            }
            for (i, par) in parity.into_iter().enumerate() {
                let mut f = Vec::with_capacity(RTP_HEADER_LEN + FEC_HEADER_LEN + len);
                RtpHeader { flags: 0x80, packet_type: PT_FEC, seq: self.block_base_seq.wrapping_add((DATA_SHARDS + i) as u16), timestamp: 0, ssrc: 0 }.write(&mut f);
                f.push(i as u8);
                f.push(PT_AUDIO);
                f.extend_from_slice(&self.block_base_seq.to_be_bytes());
                f.extend_from_slice(&self.block_base_ts.to_be_bytes());
                f.extend_from_slice(&0u32.to_be_bytes());
                f.extend_from_slice(&par);
                out.push(f);
            }
        }
        out
    }
}

#[derive(Default)]
struct AudioBlock {
    data: [Option<Vec<u8>>; DATA_SHARDS],
    parity: [Option<Vec<u8>>; FEC_SHARDS],
    base_ts: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u16,
    pub timestamp: u32,
    pub opus: Vec<u8>,
    pub recovered: bool,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AudioStats {
    pub received: u64,
    pub recovered: u64,
    pub lost: u64,
}

/// Receiver: reorders audio packets, repairs losses with FEC and emits
/// packets in sequence order (`None` = lost, play concealment).
pub struct AudioReassembler {
    blocks: BTreeMap<u16, AudioBlock>,
    next_seq: Option<u16>,
    pub stats: AudioStats,
    /// Max number of packets to wait for a gap before declaring it lost.
    pub max_delay: u16,
}

impl Default for AudioReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioReassembler {
    pub fn new() -> Self {
        Self { blocks: BTreeMap::new(), next_seq: None, stats: AudioStats::default(), max_delay: 12 }
    }

    fn block_base(seq: u16) -> u16 {
        seq - seq % DATA_SHARDS as u16
    }

    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Option<AudioPacket>>) {
        let Some(h) = RtpHeader::parse(pkt) else { return };
        let newest;
        match h.packet_type {
            PT_AUDIO => {
                let seq = h.seq;
                if let Some(n) = self.next_seq {
                    if (seq.wrapping_sub(n) as i16) < 0 {
                        return;
                    }
                } else {
                    self.next_seq = Some(seq);
                }
                let base = Self::block_base(seq);
                let b = self.blocks.entry(base).or_default();
                let i = (seq - base) as usize;
                if b.data[i].is_none() {
                    b.data[i] = Some(pkt[RTP_HEADER_LEN..].to_vec());
                    self.stats.received += 1;
                }
                if i == 0 {
                    b.base_ts = Some(h.timestamp);
                }
                newest = seq;
            }
            PT_FEC => {
                if pkt.len() < RTP_HEADER_LEN + FEC_HEADER_LEN {
                    return;
                }
                let idx = pkt[12] as usize;
                let base = u16::from_be_bytes([pkt[14], pkt[15]]);
                let base_ts = u32::from_be_bytes(pkt[16..20].try_into().unwrap());
                if idx >= FEC_SHARDS || base % DATA_SHARDS as u16 != 0 {
                    return;
                }
                if let Some(n) = self.next_seq {
                    if (base.wrapping_add(DATA_SHARDS as u16 - 1).wrapping_sub(n) as i16) < 0 {
                        return;
                    }
                }
                let b = self.blocks.entry(base).or_default();
                b.parity[idx] = Some(pkt[RTP_HEADER_LEN + FEC_HEADER_LEN..].to_vec());
                b.base_ts = Some(base_ts);
                newest = base.wrapping_add(DATA_SHARDS as u16 - 1);
            }
            _ => return,
        }
        self.repair(Self::block_base(newest));
        self.drain(newest, out);
    }

    fn repair(&mut self, base: u16) {
        let Some(b) = self.blocks.get_mut(&base) else { return };
        let have_data = b.data.iter().filter(|d| d.is_some()).count();
        let have_par = b.parity.iter().filter(|d| d.is_some()).count();
        if have_data == DATA_SHARDS || have_data + have_par < DATA_SHARDS {
            return;
        }
        let len = b.data.iter().chain(b.parity.iter()).flatten().map(|v| v.len()).max().unwrap_or(0);
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(DATA_SHARDS + FEC_SHARDS);
        let mut present = Vec::with_capacity(DATA_SHARDS + FEC_SHARDS);
        for s in b.data.iter().chain(b.parity.iter()) {
            present.push(s.is_some());
            let mut v = s.clone().unwrap_or_default();
            v.resize(len, 0);
            shards.push(v);
        }
        if rs().reconstruct_data(&mut shards, &present).is_ok() {
            for i in 0..DATA_SHARDS {
                if b.data[i].is_none() {
                    b.data[i] = Some(std::mem::take(&mut shards[i]));
                    self.stats.recovered += 1;
                }
            }
        }
    }

    fn drain(&mut self, newest: u16, out: &mut Vec<Option<AudioPacket>>) {
        while let Some(n) = self.next_seq {
            let base = Self::block_base(n);
            let i = (n - base) as usize;
            let ready = self.blocks.get(&base).and_then(|b| b.data[i].clone().map(|d| (d, b.base_ts)));
            match ready {
                Some((opus, base_ts)) => {
                    let ts = base_ts.map(|t| t.wrapping_add(i as u32 * 5)).unwrap_or(0);
                    out.push(Some(AudioPacket { seq: n, timestamp: ts, opus, recovered: false }));
                }
                None => {
                    if newest.wrapping_sub(n) < self.max_delay || (newest.wrapping_sub(n) as i16) < 0 {
                        return;
                    }
                    self.stats.lost += 1;
                    out.push(None);
                }
            }
            let next = n.wrapping_add(1);
            if next % DATA_SHARDS as u16 == 0 {
                self.blocks.remove(&base);
            }
            self.next_seq = Some(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(i: usize) -> Vec<u8> {
        (0..120).map(|j| (i * 7 + j * 13) as u8).collect()
    }

    fn run(drop: impl Fn(usize, &[u8]) -> bool) -> (Vec<Option<AudioPacket>>, AudioStats) {
        let mut tx = AudioPacketizer::new(5);
        let mut rx = AudioReassembler::new();
        let mut out = Vec::new();
        let mut n = 0;
        for i in 0..40 {
            for p in tx.packetize(&opus(i)) {
                if !drop(n, &p) {
                    rx.push(&p, &mut out);
                }
                n += 1;
            }
        }
        (out, rx.stats)
    }

    #[test]
    fn no_loss_in_order() {
        let (out, st) = run(|_, _| false);
        assert_eq!(out.len(), 40);
        for (i, p) in out.iter().enumerate() {
            let p = p.as_ref().unwrap();
            assert_eq!(p.opus, opus(i));
            assert_eq!(p.seq as usize, i);
            assert_eq!(p.timestamp as usize, i * 5);
        }
        assert_eq!(st.recovered, 0);
    }

    #[test]
    fn recovers_two_losses_per_block() {
        // Drop data packets with seq 1 and 2 in every block.
        let (out, st) = run(|_, p| p[1] == PT_AUDIO && matches!(u16::from_be_bytes([p[2], p[3]]) % 4, 1 | 2));
        assert_eq!(out.len(), 40);
        for (i, p) in out.iter().enumerate() {
            assert_eq!(p.as_ref().unwrap().opus, opus(i), "packet {i}");
        }
        assert_eq!(st.recovered, 20);
    }

    #[test]
    fn parity_rows_xor_to_one() {
        // Property observed on the wire: identical data shards produce parity equal to the data.
        for r in 0..2 {
            assert_eq!(PARITY_ROWS[r * 4..r * 4 + 4].iter().fold(0, |a, b| a ^ b), 1);
        }
    }
}
