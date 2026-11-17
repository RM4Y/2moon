//! Video stream wire format: RTP header + NV video header, frame
//! packetization with Reed-Solomon FEC (sender) and frame reassembly with
//! loss recovery (receiver).
//!
//! Packet layout (all packets of a FEC block share the same size):
//! ```text
//!  0..12  RTP header (BE): flags(0x90) type seq ts ssrc
//! 12..16  reserved
//! 16..32  NV header (LE): streamPacketIndex, frameIndex, flags, extraFlags,
//!         multiFecFlags, multiFecBlocks, fecInfo
//! 32..    shard payload (`packet_size - 16` bytes, i.e. packet_size counts
//!         the NV header but not the RTP header/reserved bytes)
//! ```
//! The first shard of a frame starts with an 8 byte frame header:
//! `01 00 00 <frameType> <lastPayloadLen u16 LE> 00 00`.

use std::collections::BTreeMap;

use crate::fec::ReedSolomon;

pub const RTP_HEADER_LEN: usize = 12;
pub const RESERVED_LEN: usize = 4;
pub const NV_HEADER_LEN: usize = 16;
pub const PACKET_HEADER_LEN: usize = RTP_HEADER_LEN + RESERVED_LEN + NV_HEADER_LEN;
pub const FRAME_HEADER_LEN: usize = 8;

pub const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;

pub const FRAME_TYPE_P: u8 = 1;
pub const FRAME_TYPE_IDR: u8 = 2;
/// P-frame with intra-refresh blocks (Sunshine).
pub const FRAME_TYPE_INTRA_REFRESH: u8 = 4;
/// First P-frame after a reference frame invalidation (Sunshine).
pub const FRAME_TYPE_RFI_RECOVERY: u8 = 5;
/// NV header extra flag: this frame is a long-term reference (Sunshine RFI).
pub const EXTRA_FLAG_LTR_FRAME: u8 = 0x1;

/// Maximum shards per FEC block and number of blocks per frame.
pub const MAX_SHARDS: usize = 255;
pub const MAX_BLOCKS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacketHeader {
    pub rtp_flags: u8,
    pub packet_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub stream_packet_index: u32,
    pub frame_index: u32,
    pub flags: u8,
    pub extra_flags: u8,
    pub multi_fec_flags: u8,
    pub multi_fec_blocks: u8,
    pub fec_info: u32,
}

impl PacketHeader {
    pub fn shard_index(&self) -> usize {
        ((self.fec_info >> 12) & 0x3ff) as usize
    }
    pub fn data_shards(&self) -> usize {
        ((self.fec_info >> 22) & 0x3ff) as usize
    }
    pub fn fec_percent(&self) -> u32 {
        (self.fec_info >> 4) & 0xff
    }
    pub fn block_index(&self) -> usize {
        ((self.multi_fec_blocks >> 4) & 0x3) as usize
    }
    pub fn last_block_index(&self) -> usize {
        ((self.multi_fec_blocks >> 6) & 0x3) as usize
    }

    pub fn make_fec_info(shard: usize, data_shards: usize, percent: u32) -> u32 {
        ((data_shards as u32) << 22) | ((shard as u32) << 12) | (percent << 4)
    }

    pub fn write(&self, out: &mut [u8]) {
        out[0] = self.rtp_flags;
        out[1] = self.packet_type;
        out[2..4].copy_from_slice(&self.seq.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        out[12..16].fill(0);
        out[16..20].copy_from_slice(&self.stream_packet_index.to_le_bytes());
        out[20..24].copy_from_slice(&self.frame_index.to_le_bytes());
        out[24] = self.flags;
        out[25] = self.extra_flags;
        out[26] = self.multi_fec_flags;
        out[27] = self.multi_fec_blocks;
        out[28..32].copy_from_slice(&self.fec_info.to_le_bytes());
    }

    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < PACKET_HEADER_LEN {
            return None;
        }
        let u32le = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        Some(Self {
            rtp_flags: p[0],
            packet_type: p[1],
            seq: u16::from_be_bytes([p[2], p[3]]),
            timestamp: u32::from_be_bytes(p[4..8].try_into().unwrap()),
            ssrc: u32::from_be_bytes(p[8..12].try_into().unwrap()),
            stream_packet_index: u32le(16),
            frame_index: u32le(20),
            flags: p[24],
            extra_flags: p[25],
            multi_fec_flags: p[26],
            multi_fec_blocks: p[27],
            fec_info: u32le(28),
        })
    }
}

/// Sender side: split encoded frames into FEC-protected packets.
pub struct Packetizer {
    /// Bytes of shard payload per packet (`packetSize` from the client minus the NV header).
    pub shard_payload: usize,
    pub fec_percent: u32,
    pub min_parity: usize,
    seq: u16,
    stream_index: u32,
    pub ssrc: u32,
    /// Host processing latency of the next frame in 0.1 ms units (frame header bytes 1..3).
    pub processing_latency: u16,
}

impl Packetizer {
    /// `packet_size` is the client's `x-nv-video[0].packetSize`.
    pub fn new(packet_size: usize, fec_percent: u32, min_parity: usize) -> Self {
        Self { shard_payload: packet_size.saturating_sub(NV_HEADER_LEN).max(64), fec_percent, min_parity, seq: 0, stream_index: 0, ssrc: 0, processing_latency: 0 }
    }

    /// `streamPacketIndex` is a 24-bit packet counter (stored << 8). It must NOT
    /// wrap with the 16-bit RTP sequence number: clients compare it modulo 2^24
    /// and would discard every packet after a 16-bit wrap (~65k packets).
    fn seq_index(&self) -> u32 {
        (self.stream_index & 0x00ff_ffff) << 8
    }

    fn advance(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.stream_index = self.stream_index.wrapping_add(1) & 0x00ff_ffff;
    }

    fn parity_for(&self, data: usize) -> usize {
        let p = (data * self.fec_percent as usize).div_ceil(100);
        p.max(self.min_parity).min(MAX_SHARDS - data)
    }

    /// Packetize one encoded frame. `timestamp` is in 90 kHz units.
    pub fn packetize(&mut self, frame_index: u32, timestamp: u32, idr: bool, bitstream: &[u8]) -> Vec<Vec<u8>> {
        // Prefix the frame header; lastPayloadLen is patched below.
        let mut payload = Vec::with_capacity(FRAME_HEADER_LEN + bitstream.len());
        let lat = self.processing_latency.to_le_bytes();
        payload.extend_from_slice(&[0x01, lat[0], lat[1], if idr { FRAME_TYPE_IDR } else { FRAME_TYPE_P }, 0, 0, 0, 0]);
        payload.extend_from_slice(bitstream);
        let sp = self.shard_payload;
        let total_data = payload.len().div_ceil(sp);
        let last_len = payload.len() - (total_data - 1) * sp;
        payload[4..6].copy_from_slice(&(last_len as u16).to_le_bytes());

        // Split into up to 4 FEC blocks so that each block fits in 255 shards with parity.
        let max_data_per_block = {
            let mut d = MAX_SHARDS;
            while d > 1 && d + self.parity_for(d) > MAX_SHARDS {
                d -= 1;
            }
            d
        };
        let mut blocks = total_data.div_ceil(max_data_per_block).max(1);
        let fec_percent = if blocks > MAX_BLOCKS {
            // Oversized frame: send without parity in 4 blocks of up to 255 shards.
            blocks = total_data.div_ceil(MAX_SHARDS).min(MAX_BLOCKS);
            0
        } else {
            self.fec_percent
        };
        let per_block = total_data.div_ceil(blocks);

        let mut packets = Vec::new();
        let mut shard_start = 0usize;
        for b in 0..blocks {
            let data_n = per_block.min(total_data - shard_start);
            let parity_n = if fec_percent == 0 && blocks > 1 && total_data > MAX_SHARDS * MAX_BLOCKS { 0 } else { self.parity_for(data_n).min(MAX_SHARDS - data_n) };
            // The percentage field is what the receiver uses to derive the parity count.
            let percent_field = if data_n > 0 { (parity_n * 100 / data_n) as u32 } else { 0 };
            let percent_field = (1..=255u32).find(|p| (data_n * *p as usize).div_ceil(100) == parity_n).unwrap_or(percent_field);
            let multi_fec_blocks = ((b as u8) << 4) | (((blocks - 1) as u8) << 6);
            let first_packet = packets.len();
            for i in 0..data_n {
                let global = shard_start + i;
                let mut flags = FLAG_CONTAINS_PIC_DATA;
                if global == 0 {
                    flags |= FLAG_SOF;
                }
                if global == total_data - 1 {
                    flags |= FLAG_EOF;
                }
                let h = PacketHeader {
                    rtp_flags: 0x90,
                    packet_type: 0,
                    seq: self.seq,
                    timestamp,
                    ssrc: self.ssrc,
                    stream_packet_index: self.seq_index(),
                    frame_index,
                    flags,
                    extra_flags: 0,
                    multi_fec_flags: 0x10,
                    multi_fec_blocks,
                    fec_info: PacketHeader::make_fec_info(i, data_n, percent_field),
                };
                let mut pkt = vec![0u8; PACKET_HEADER_LEN + sp];
                h.write(&mut pkt);
                let off = global * sp;
                if off < payload.len() {
                    let end = (off + sp).min(payload.len());
                    pkt[PACKET_HEADER_LEN..PACKET_HEADER_LEN + end - off].copy_from_slice(&payload[off..end]);
                }
                packets.push(pkt);
                self.advance();
            }
            if parity_n > 0 {
                // FEC shards span the NV header and the payload (everything after
                // the RTP header + reserved bytes).
                let shard_off = RTP_HEADER_LEN + RESERVED_LEN;
                let rs = ReedSolomon::new(data_n, parity_n).expect("valid shard counts");
                let mut parity = vec![vec![0u8; PACKET_HEADER_LEN + sp]; parity_n];
                {
                    let d: Vec<&[u8]> = packets[first_packet..].iter().map(|p| &p[shard_off..]).collect();
                    let mut p: Vec<&mut [u8]> = parity.iter_mut().map(|s| &mut s[shard_off..]).collect();
                    rs.encode(&d, &mut p).expect("encode");
                }
                for (i, mut pkt) in parity.into_iter().enumerate() {
                    pkt[0] = 0x90;
                    pkt[1] = 0;
                    pkt[2..4].copy_from_slice(&self.seq.to_be_bytes());
                    pkt[4..8].copy_from_slice(&timestamp.to_be_bytes());
                    pkt[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
                    pkt[20..24].copy_from_slice(&frame_index.to_le_bytes());
                    pkt[27] = multi_fec_blocks;
                    pkt[28..32].copy_from_slice(&PacketHeader::make_fec_info(data_n + i, data_n, percent_field).to_le_bytes());
                    packets.push(pkt);
                    self.advance();
                }
            }
            shard_start += data_n;
        }
        packets
    }
}

/// A fully reassembled frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub index: u32,
    pub timestamp: u32,
    pub idr: bool,
    /// Frame type from the frame header (`FRAME_TYPE_*`).
    pub frame_type: u8,
    /// Long-term reference frame: the client acknowledges it when using RFI.
    pub ltr: bool,
    pub data: Vec<u8>,
    /// Number of data shards that had to be rebuilt with FEC.
    pub recovered_shards: usize,
}

impl Frame {
    /// Decoding can restart on this frame after a reference frame invalidation.
    pub fn is_rfi_recovery_point(&self) -> bool {
        self.idr || matches!(self.frame_type, FRAME_TYPE_IDR | FRAME_TYPE_INTRA_REFRESH | FRAME_TYPE_RFI_RECOVERY)
    }
}

struct Block {
    data_shards: usize,
    total: usize,
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    recovered: usize,
    done: bool,
}

struct PendingFrame {
    timestamp: u32,
    ltr: bool,
    last_block: usize,
    blocks: Vec<Option<Block>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ReassemblyStats {
    pub frames_ok: u64,
    pub frames_lost: u64,
    pub shards_recovered: u64,
    pub packets: u64,
}

/// Receiver side: collect packets per frame / FEC block and emit complete frames in order.
pub struct Reassembler {
    pending: BTreeMap<u32, PendingFrame>,
    next_frame: Option<u32>,
    pub stats: ReassemblyStats,
    /// Frames older than `newest - window` are given up on.
    pub window: u32,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of feeding a packet.
#[derive(Debug)]
pub enum Event {
    Frame(Frame),
    /// Frames in this inclusive range could not be recovered.
    Lost { first: u32, last: u32 },
}

impl Reassembler {
    pub fn new() -> Self {
        Self { pending: BTreeMap::new(), next_frame: None, stats: ReassemblyStats::default(), window: 8 }
    }

    pub fn push(&mut self, pkt: &[u8], out: &mut Vec<Event>) {
        let Some(h) = PacketHeader::parse(pkt) else { return };
        self.stats.packets += 1;
        // FEC shards span the NV header + payload.
        let payload = &pkt[RTP_HEADER_LEN + RESERVED_LEN..];
        let frame = h.frame_index;
        if let Some(n) = self.next_frame {
            if (frame.wrapping_sub(n) as i32) < 0 {
                return; // already delivered or abandoned
            }
        } else {
            self.next_frame = Some(frame);
        }
        let data_n = h.data_shards();
        let shard = h.shard_index();
        let block_idx = h.block_index();
        let total = if h.fec_percent() == 0 && shard < data_n { data_n } else { data_n + (data_n * h.fec_percent() as usize).div_ceil(100) };
        if data_n == 0 || shard >= MAX_SHARDS || block_idx > h.last_block_index() || payload.len() <= NV_HEADER_LEN {
            return;
        }
        let pf = self.pending.entry(frame).or_insert_with(|| PendingFrame { timestamp: h.timestamp, ltr: h.extra_flags & EXTRA_FLAG_LTR_FRAME != 0, last_block: h.last_block_index(), blocks: (0..=h.last_block_index()).map(|_| None).collect() });
        let Some(slot) = pf.blocks.get_mut(block_idx) else { return };
        let blk = slot.get_or_insert_with(|| Block { data_shards: data_n, total: total.max(data_n), shards: vec![None; MAX_SHARDS], received: 0, recovered: 0, done: false });
        if blk.done || shard >= blk.shards.len() || blk.shards[shard].is_some() {
            return;
        }
        blk.total = blk.total.max(shard + 1);
        blk.shards[shard] = Some(payload.to_vec());
        blk.received += 1;
        if blk.received >= blk.data_shards {
            let data_present = blk.shards[..blk.data_shards].iter().all(|s| s.is_some());
            if data_present {
                blk.done = true;
            } else {
                let total = blk.total;
                let parity = total - blk.data_shards;
                if let Ok(rs) = ReedSolomon::new(blk.data_shards, parity) {
                    let len = payload.len();
                    let present: Vec<bool> = blk.shards[..total].iter().map(|s| s.is_some()).collect();
                    let mut shards: Vec<Vec<u8>> = blk.shards[..total].iter().map(|s| s.clone().unwrap_or_else(|| vec![0; len])).collect();
                    if rs.reconstruct_data(&mut shards, &present).is_ok() {
                        let missing = present[..blk.data_shards].iter().filter(|p| !**p).count();
                        self.stats.shards_recovered += missing as u64;
                        blk.recovered = missing;
                        for (i, s) in shards.into_iter().take(blk.data_shards).enumerate() {
                            blk.shards[i] = Some(s);
                        }
                        blk.done = true;
                    }
                }
            }
        }
        self.drain(frame, out);
    }

    fn try_complete(pf: &PendingFrame, index: u32) -> Option<Frame> {
        if pf.blocks.len() != pf.last_block + 1 || pf.blocks.iter().any(|b| !b.as_ref().map(|b| b.done).unwrap_or(false)) {
            return None;
        }
        let mut data = Vec::new();
        let mut recovered = 0;
        for b in pf.blocks.iter().flatten() {
            for s in &b.shards[..b.data_shards] {
                data.extend_from_slice(&s.as_ref().unwrap()[NV_HEADER_LEN..]);
            }
            recovered += b.recovered;
        }
        if data.len() < FRAME_HEADER_LEN {
            return None;
        }
        let shard_len = pf.blocks[0].as_ref().unwrap().shards[0].as_ref().unwrap().len() - NV_HEADER_LEN;
        let frame_type = data[3];
        let idr = frame_type == FRAME_TYPE_IDR;
        let last_len = u16::from_le_bytes([data[4], data[5]]) as usize;
        if last_len > 0 && last_len <= shard_len {
            let trimmed = data.len() - (shard_len - last_len);
            data.truncate(trimmed);
        }
        data.drain(..FRAME_HEADER_LEN);
        Some(Frame { index, timestamp: pf.timestamp, idr, frame_type, ltr: pf.ltr, data, recovered_shards: recovered })
    }

    fn drain(&mut self, newest: u32, out: &mut Vec<Event>) {
        loop {
            let Some(next) = self.next_frame else { return };
            if let Some(pf) = self.pending.get(&next) {
                if let Some(f) = Self::try_complete(pf, next) {
                    self.pending.remove(&next);
                    self.stats.frames_ok += 1;
                    out.push(Event::Frame(f));
                    self.next_frame = Some(next.wrapping_add(1));
                    continue;
                }
            }
            // Give up on `next` if we are too far ahead.
            if newest.wrapping_sub(next) as i32 > self.window as i32 {
                let first = next;
                let mut last = next;
                // Skip to the oldest frame we still hold that is within the window.
                let target = self.pending.keys().copied().find(|&k| (newest.wrapping_sub(k) as i32) <= self.window as i32).unwrap_or(newest);
                while last.wrapping_add(1) != target {
                    last = last.wrapping_add(1);
                }
                self.pending.retain(|&k, _| (k.wrapping_sub(target) as i32) >= 0);
                self.stats.frames_lost += (last.wrapping_sub(first) + 1) as u64;
                out.push(Event::Lost { first, last });
                self.next_frame = Some(target);
                continue;
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn collect(r: &mut Reassembler, pkts: &[Vec<u8>]) -> Vec<Event> {
        let mut ev = Vec::new();
        for p in pkts {
            r.push(p, &mut ev);
        }
        ev
    }

    #[test]
    fn roundtrip_no_loss() {
        let mut p = Packetizer::new(1024, 20, 2);
        let mut r = Reassembler::new();
        for (i, size) in [10usize, 1000, 5000, 100_000, 400_000].iter().enumerate() {
            let f = frame_bytes(*size, i as u8);
            let pkts = p.packetize(i as u32 + 1, 1000, i == 0, &f);
            assert!(pkts.iter().all(|x| x.len() == PACKET_HEADER_LEN + 1008));
            let ev = collect(&mut r, &pkts);
            match &ev[..] {
                [Event::Frame(fr)] => {
                    assert_eq!(fr.data, f, "frame {i} size {size}");
                    assert_eq!(fr.idr, i == 0);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn stream_packet_index_does_not_wrap_with_rtp_seq() {
        let mut p = Packetizer::new(1024, 0, 0);
        let mut last_idx = None;
        let mut last_seq = 0u16;
        let mut wrapped = false;
        for i in 0..2000u32 {
            for pkt in p.packetize(i, 0, false, &frame_bytes(40 * 1008, 1)) {
                let h = PacketHeader::parse(&pkt).unwrap();
                if h.seq < last_seq {
                    wrapped = true;
                }
                last_seq = h.seq;
                let idx = h.stream_packet_index >> 8;
                if let Some(prev) = last_idx {
                    assert_eq!(idx, (prev + 1) & 0xff_ffff, "streamPacketIndex must increase across the RTP wrap");
                }
                last_idx = Some(idx);
                assert_eq!(h.stream_packet_index & 0xff, 0);
            }
        }
        assert!(wrapped, "test must cross the 16-bit RTP sequence wrap");
    }

    #[test]
    fn recovers_with_fec() {
        let mut p = Packetizer::new(1024, 20, 2);
        let mut r = Reassembler::new();
        let f = frame_bytes(50_000, 7);
        let mut pkts = p.packetize(1, 0, true, &f);
        // Drop 2 data packets and reorder the rest.
        pkts.remove(3);
        pkts.remove(10);
        pkts.reverse();
        let ev = collect(&mut r, &pkts);
        match &ev[..] {
            [Event::Frame(fr)] => assert_eq!(fr.data, f),
            other => panic!("unexpected {other:?}"),
        }
        assert!(r.stats.shards_recovered >= 2); // reversed order: recovery kicks in before late data shards arrive
    }

    #[test]
    fn reports_lost_frames() {
        let mut p = Packetizer::new(1024, 0, 0);
        let mut r = Reassembler::new();
        let mut ev = Vec::new();
        for i in 1..=20u32 {
            let pkts = p.packetize(i, 0, false, &frame_bytes(3000, i as u8));
            for (k, pkt) in pkts.iter().enumerate() {
                if i == 5 && k == 0 {
                    continue; // frame 5 is unrecoverable without parity
                }
                r.push(pkt, &mut ev);
            }
        }
        let frames: Vec<u32> = ev.iter().filter_map(|e| if let Event::Frame(f) = e { Some(f.index) } else { None }).collect();
        assert!(ev.iter().any(|e| matches!(e, Event::Lost { first: 5, last: 5 })), "{ev:?}");
        assert_eq!(frames.len(), 19);
        assert!(!frames.contains(&5));
    }
}
