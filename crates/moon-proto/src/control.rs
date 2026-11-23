//! Control stream messages (carried over ENet) and input event encoding.
//!
//! Generation-7 hosts wrap every control message in an AES-128-GCM envelope:
//! ```text
//! u16 LE 0x0001 (encrypted marker) | u16 LE length | u32 LE seq | tag[16] | ciphertext
//! ciphertext = u16 LE type | u16 LE payload length | payload
//! ```
//! `length` counts seq + tag + ciphertext. With the "control v2" extension the
//! 12-byte IV is `seq (LE) | 0.. | origin | 'C'` where origin is 'C' for
//! client-originated and 'H' for host-originated messages.

use crate::crypto;

pub const ENCRYPTED_HEADER_TYPE: u16 = 0x0001;

pub mod msg {
    pub const PERIODIC_PING: u16 = 0x0200;
    pub const LOSS_STATS: u16 = 0x0201;
    pub const FRAME_STATS: u16 = 0x0204;
    pub const INPUT_DATA: u16 = 0x0206;
    pub const INVALIDATE_REF_FRAMES: u16 = 0x0301;
    pub const REQUEST_IDR_FRAME: u16 = 0x0302;
    /// Client acknowledges a long-term reference frame.
    pub const LTR_FRAME_ACK: u16 = 0x0350;
    pub const START_A: u16 = 0x0305;
    pub const START_B: u16 = 0x0307;
    pub const TERMINATION: u16 = 0x0109;
    pub const RUMBLE: u16 = 0x010b;
    pub const HDR_MODE: u16 = 0x010e;
    pub const RUMBLE_TRIGGERS: u16 = 0x5500;
    pub const SET_MOTION_EVENT: u16 = 0x5501;
    pub const SET_RGB_LED: u16 = 0x5502;
}

/// Termination reasons sent by the host (big endian u32 payload).
pub mod termination {
    pub const GRACEFUL: u32 = 0x80030023;
    pub const PROTECTED_CONTENT: u32 = 0x800e9302;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Client,
    Host,
}

#[derive(Debug, Clone)]
pub struct ControlCipher {
    key: [u8; 16],
    pub v2: bool,
    pub send_seq: u32,
    pub local: Origin,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("packet too short")]
    Short,
    #[error("not an encrypted control packet")]
    NotEncrypted,
    #[error("authentication failed")]
    Auth,
}

fn iv(seq: u32, v2: bool, origin: Origin) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    if v2 {
        iv[10] = match origin {
            Origin::Client => b'C',
            Origin::Host => b'H',
        };
        iv[11] = b'C';
    }
    iv
}

impl ControlCipher {
    pub fn new(key: [u8; 16], v2: bool, local: Origin) -> Self {
        Self { key, v2, send_seq: 0, local }
    }

    /// Encode + encrypt one control message.
    pub fn seal(&mut self, msg_type: u16, payload: &[u8]) -> Vec<u8> {
        let mut plain = Vec::with_capacity(4 + payload.len());
        plain.extend_from_slice(&msg_type.to_le_bytes());
        plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        plain.extend_from_slice(payload);
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        let (tag, ct) = crypto::gcm_encrypt(&self.key, &iv(seq, self.v2, self.local), &plain);
        let mut out = Vec::with_capacity(8 + 16 + ct.len());
        out.extend_from_slice(&ENCRYPTED_HEADER_TYPE.to_le_bytes());
        out.extend_from_slice(&((4 + 16 + ct.len()) as u16).to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&ct);
        out
    }

    /// Decrypt a message sent by the peer. Returns (type, payload).
    pub fn open(&self, pkt: &[u8]) -> Result<(u16, Vec<u8>), ControlError> {
        if pkt.len() < 8 + 16 + 4 {
            return Err(ControlError::Short);
        }
        if u16::from_le_bytes([pkt[0], pkt[1]]) != ENCRYPTED_HEADER_TYPE {
            return Err(ControlError::NotEncrypted);
        }
        let seq = u32::from_le_bytes(pkt[4..8].try_into().unwrap());
        let peer = match self.local {
            Origin::Client => Origin::Host,
            Origin::Host => Origin::Client,
        };
        let plain = crypto::gcm_decrypt(&self.key, &iv(seq, self.v2, peer), &pkt[8..24], &pkt[24..]).map_err(|_| ControlError::Auth)?;
        if plain.len() < 4 {
            return Err(ControlError::Short);
        }
        let t = u16::from_le_bytes([plain[0], plain[1]]);
        let len = (u16::from_le_bytes([plain[2], plain[3]]) as usize).min(plain.len() - 4);
        Ok((t, plain[4..4 + len].to_vec()))
    }
}

/// Input events (payload of `msg::INPUT_DATA`).
pub mod input {
    pub const KEY_DOWN: u32 = 0x0000_0003;
    pub const KEY_UP: u32 = 0x0000_0004;
    pub const MOUSE_MOVE_ABS: u32 = 0x0000_0005;
    pub const MOUSE_MOVE_REL: u32 = 0x0000_0007;
    pub const MOUSE_BUTTON_DOWN: u32 = 0x0000_0008;
    pub const MOUSE_BUTTON_UP: u32 = 0x0000_0009;
    pub const SCROLL: u32 = 0x0000_000A;
    pub const MULTI_CONTROLLER: u32 = 0x0000_000C;
    pub const UTF8_TEXT: u32 = 0x0000_0017;
    pub const HSCROLL: u32 = 0x5500_0001;

    pub const BUTTON_LEFT: u8 = 1;
    pub const BUTTON_MIDDLE: u8 = 2;
    pub const BUTTON_RIGHT: u8 = 3;
    pub const BUTTON_X1: u8 = 4;
    pub const BUTTON_X2: u8 = 5;

    pub const MOD_SHIFT: u8 = 0x01;
    pub const MOD_CTRL: u8 = 0x02;
    pub const MOD_ALT: u8 = 0x04;
    pub const MOD_META: u8 = 0x08;

    /// Gamepad button flags (low 16 bits + extended high bits).
    pub mod pad {
        pub const UP: u32 = 0x0001;
        pub const DOWN: u32 = 0x0002;
        pub const LEFT: u32 = 0x0004;
        pub const RIGHT: u32 = 0x0008;
        pub const START: u32 = 0x0010;
        pub const BACK: u32 = 0x0020;
        pub const LS: u32 = 0x0040;
        pub const RS: u32 = 0x0080;
        pub const LB: u32 = 0x0100;
        pub const RB: u32 = 0x0200;
        pub const HOME: u32 = 0x0400;
        pub const A: u32 = 0x1000;
        pub const B: u32 = 0x2000;
        pub const X: u32 = 0x4000;
        pub const Y: u32 = 0x8000;
    }

    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        /// Windows virtual-key code (without the 0x8000 flag).
        Key { down: bool, vk: u16, modifiers: u8 },
        MouseRel { dx: i16, dy: i16 },
        MouseAbs { x: i16, y: i16, width: i16, height: i16 },
        MouseButton { down: bool, button: u8 },
        /// Vertical scroll; 120 = one notch.
        Scroll { amount: i16 },
        HScroll { amount: i16 },
        Text(String),
        Gamepad(Gamepad),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct Gamepad {
        pub index: u16,
        pub active_mask: u16,
        pub buttons: u32,
        pub left_trigger: u8,
        pub right_trigger: u8,
        pub ls_x: i16,
        pub ls_y: i16,
        pub rs_x: i16,
        pub rs_y: i16,
    }

    fn header(out: &mut Vec<u8>, magic: u32) {
        out.extend_from_slice(&[0, 0, 0, 0]); // size, patched later
        out.extend_from_slice(&magic.to_le_bytes());
    }

    fn finish(mut out: Vec<u8>) -> Vec<u8> {
        let size = (out.len() - 4) as u32;
        out[..4].copy_from_slice(&size.to_be_bytes());
        out
    }

    impl Event {
        pub fn encode(&self) -> Vec<u8> {
            let mut o = Vec::with_capacity(32);
            match self {
                Event::Key { down, vk, modifiers } => {
                    header(&mut o, if *down { KEY_DOWN } else { KEY_UP });
                    o.push(0);
                    o.extend_from_slice(&(0x8000u16 | vk).to_le_bytes());
                    o.push(*modifiers);
                    o.extend_from_slice(&[0, 0]);
                }
                Event::MouseRel { dx, dy } => {
                    header(&mut o, MOUSE_MOVE_REL);
                    o.extend_from_slice(&dx.to_be_bytes());
                    o.extend_from_slice(&dy.to_be_bytes());
                }
                Event::MouseAbs { x, y, width, height } => {
                    header(&mut o, MOUSE_MOVE_ABS);
                    o.extend_from_slice(&x.to_be_bytes());
                    o.extend_from_slice(&y.to_be_bytes());
                    o.extend_from_slice(&[0, 0]);
                    o.extend_from_slice(&(width - 1).to_be_bytes());
                    o.extend_from_slice(&(height - 1).to_be_bytes());
                }
                Event::MouseButton { down, button } => {
                    header(&mut o, if *down { MOUSE_BUTTON_DOWN } else { MOUSE_BUTTON_UP });
                    o.push(*button);
                }
                Event::Scroll { amount } => {
                    header(&mut o, SCROLL);
                    o.extend_from_slice(&amount.to_be_bytes());
                    o.extend_from_slice(&amount.to_be_bytes());
                    o.extend_from_slice(&[0, 0]);
                }
                Event::HScroll { amount } => {
                    header(&mut o, HSCROLL);
                    o.extend_from_slice(&amount.to_be_bytes());
                }
                Event::Text(t) => {
                    header(&mut o, UTF8_TEXT);
                    o.extend_from_slice(t.as_bytes());
                }
                Event::Gamepad(g) => {
                    header(&mut o, MULTI_CONTROLLER);
                    o.extend_from_slice(&0x1Au16.to_le_bytes()); // header A
                    o.extend_from_slice(&g.index.to_le_bytes());
                    o.extend_from_slice(&g.active_mask.to_le_bytes());
                    o.extend_from_slice(&0x14u16.to_le_bytes()); // mid B
                    o.extend_from_slice(&(g.buttons as u16).to_le_bytes());
                    o.push(g.left_trigger);
                    o.push(g.right_trigger);
                    o.extend_from_slice(&g.ls_x.to_le_bytes());
                    o.extend_from_slice(&g.ls_y.to_le_bytes());
                    o.extend_from_slice(&g.rs_x.to_le_bytes());
                    o.extend_from_slice(&g.rs_y.to_le_bytes());
                    o.extend_from_slice(&((g.buttons >> 16) as u16).to_le_bytes());
                    o.extend_from_slice(&0x55u16.to_le_bytes()); // tail
                    o.extend_from_slice(&[0, 0]);
                }
            }
            finish(o)
        }

        pub fn decode(p: &[u8]) -> Option<Self> {
            if p.len() < 8 {
                return None;
            }
            let magic = u32::from_le_bytes(p[4..8].try_into().unwrap());
            let b = &p[8..];
            let be16 = |o: usize| b.get(o..o + 2).map(|s| i16::from_be_bytes([s[0], s[1]]));
            let le16 = |o: usize| b.get(o..o + 2).map(|s| i16::from_le_bytes([s[0], s[1]]));
            Some(match magic {
                KEY_DOWN | KEY_UP => {
                    let vk = (le16(1)? as u16) & 0x7fff;
                    Event::Key { down: magic == KEY_DOWN, vk, modifiers: *b.get(3)? }
                }
                MOUSE_MOVE_REL => Event::MouseRel { dx: be16(0)?, dy: be16(2)? },
                MOUSE_MOVE_ABS => Event::MouseAbs { x: be16(0)?, y: be16(2)?, width: be16(6)?.saturating_add(1), height: be16(8)?.saturating_add(1) },
                MOUSE_BUTTON_DOWN | MOUSE_BUTTON_UP => Event::MouseButton { down: magic == MOUSE_BUTTON_DOWN, button: *b.first()? },
                SCROLL => Event::Scroll { amount: be16(0)? },
                HSCROLL => Event::HScroll { amount: be16(0)? },
                UTF8_TEXT => Event::Text(String::from_utf8_lossy(b).into_owned()),
                MULTI_CONTROLLER => Event::Gamepad(Gamepad {
                    index: le16(2)? as u16,
                    active_mask: le16(4)? as u16,
                    buttons: (le16(8)? as u16 as u32) | ((le16(20)? as u16 as u32) << 16),
                    left_trigger: *b.get(10)?,
                    right_trigger: *b.get(11)?,
                    ls_x: le16(12)?,
                    ls_y: le16(14)?,
                    rs_x: le16(16)?,
                    rs_y: le16(18)?,
                }),
                _ => return None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::input::*;
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = [5u8; 16];
        let mut c = ControlCipher::new(key, true, Origin::Client);
        let h = ControlCipher::new(key, true, Origin::Host);
        let p = c.seal(msg::PERIODIC_PING, &[1, 2, 3]);
        assert_eq!(h.open(&p).unwrap(), (msg::PERIODIC_PING, vec![1, 2, 3]));
        // A client must not be able to decrypt its own messages as host messages (IV domain separation).
        let c2 = ControlCipher::new(key, true, Origin::Client);
        assert_eq!(c2.open(&p), Err(ControlError::Auth));
        let p2 = c.seal(msg::START_A, &[]);
        assert_eq!(u32::from_le_bytes(p2[4..8].try_into().unwrap()), 1);
    }

    #[test]
    fn input_roundtrip() {
        let evs = vec![
            Event::Key { down: true, vk: 0x41, modifiers: MOD_SHIFT },
            Event::Key { down: false, vk: 0x0D, modifiers: 0 },
            Event::MouseRel { dx: -5, dy: 300 },
            Event::MouseAbs { x: 100, y: 200, width: 1920, height: 1080 },
            Event::MouseButton { down: true, button: BUTTON_RIGHT },
            Event::Scroll { amount: -120 },
            Event::HScroll { amount: 120 },
            Event::Text("héllo".into()),
            Event::Gamepad(Gamepad { index: 1, active_mask: 3, buttons: pad::A | pad::HOME, left_trigger: 10, right_trigger: 255, ls_x: -32768, ls_y: 5, rs_x: 32767, rs_y: -1 }),
        ];
        for e in evs {
            let enc = e.encode();
            assert_eq!(u32::from_be_bytes(enc[..4].try_into().unwrap()) as usize, enc.len() - 4);
            assert_eq!(Event::decode(&enc), Some(e));
        }
    }
}
