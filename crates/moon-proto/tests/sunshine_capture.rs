//! Replays a raw video capture recorded from a real Sunshine host
//! (`moon-client probe`). Skipped unless MOON_VIDEO_CAPTURE points to it.

use moon_proto::video::{Event, PacketHeader, Reassembler};

fn load() -> Option<Vec<Vec<u8>>> {
    let path = std::env::var("MOON_VIDEO_CAPTURE").ok()?;
    let d = std::fs::read(path).ok()?;
    let mut v = Vec::new();
    let mut i = 0;
    while i + 4 <= d.len() {
        let n = u32::from_le_bytes(d[i..i + 4].try_into().unwrap()) as usize;
        v.push(d[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    Some(v)
}

fn frames(pkts: &[Vec<u8>]) -> (Vec<moon_proto::video::Frame>, Reassembler) {
    let mut r = Reassembler::new();
    let mut ev = Vec::new();
    for p in pkts {
        r.push(p, &mut ev);
    }
    let f = ev.into_iter().filter_map(|e| if let Event::Frame(f) = e { Some(f) } else { None }).collect();
    (f, r)
}

#[test]
fn reassemble_real_sunshine_stream() {
    let Some(pkts) = load() else { return };
    let (clean, r) = frames(&pkts);
    assert!(clean.len() > 10);
    assert_eq!(r.stats.frames_lost, 0);
    assert!(clean[0].idr);
    assert_eq!(&clean[0].data[..5], &[0, 0, 0, 1, 0x67], "first frame must start with an SPS");
    for f in &clean {
        assert!(f.data.starts_with(&[0, 0, 0, 1]) || f.data.starts_with(&[0, 0, 1]), "frame {} not annex-b", f.index);
    }

    // Drop one data packet out of every FEC block (and two where parity allows).
    let lossy: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            let h = PacketHeader::parse(p).unwrap();
            let s = h.shard_index();
            !(s == 1 || (s == 4 && h.data_shards() >= 8))
        })
        .cloned()
        .collect();
    let (recovered, r2) = frames(&lossy);
    assert!(r2.stats.shards_recovered > 0);
    assert_eq!(recovered.len(), clean.len());
    for (a, b) in clean.iter().zip(&recovered) {
        assert_eq!(a.data, b.data, "frame {} differs after FEC recovery", a.index);
    }
    eprintln!("{} frames, {} shards rebuilt with FEC", clean.len(), r2.stats.shards_recovered);
}
