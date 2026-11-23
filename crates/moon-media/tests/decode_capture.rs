//! Decode a real H.264 capture (see moon-proto/tests/real_capture.rs).
//! Skipped unless MOON_VIDEO_CAPTURE is set.

use moon_media::h264::H264Decoder;
use moon_proto::video::{Event, Reassembler};

#[test]
fn decode_real_stream() {
    let Ok(path) = std::env::var("MOON_VIDEO_CAPTURE") else { return };
    let d = std::fs::read(path).unwrap();
    let mut r = Reassembler::new();
    let mut ev = Vec::new();
    let mut i = 0;
    while i + 4 <= d.len() {
        let n = u32::from_le_bytes(d[i..i + 4].try_into().unwrap()) as usize;
        r.push(&d[i + 4..i + 4 + n], &mut ev);
        i += 4 + n;
    }
    let mut dec = H264Decoder::new().unwrap();
    let mut pics = 0;
    let mut dims = (0, 0);
    let mut errors = 0;
    for e in ev {
        if let Event::Frame(f) = e {
            match dec.decode(&f.data) {
                Ok(Some(p)) => {
                    pics += 1;
                    dims = (p.width, p.height);
                }
                Ok(None) => {}
                Err(_) => errors += 1,
            }
        }
    }
    eprintln!("decoded {pics} pictures at {dims:?}, {errors} errors");
    assert!(pics > 100);
    assert_eq!(errors, 0);
}
