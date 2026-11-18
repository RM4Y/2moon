//! Control stream client: an ENet connection carrying encrypted control
//! messages (start, keep-alive, IDR requests, input) to the host, and
//! receiving host events (termination, rumble, HDR...).

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use moon_proto::control::{input, msg, ControlCipher, Origin};
use rusty_enet as enet;
use tracing::{debug, warn};

pub const CHANNEL_COUNT: usize = 0x20;
pub const CHANNEL_GENERIC: u8 = 0;
pub const CHANNEL_URGENT: u8 = 1;
pub const CHANNEL_KEYBOARD: u8 = 2;
pub const CHANNEL_MOUSE: u8 = 3;
pub const CHANNEL_UTF8: u8 = 6;
pub const CHANNEL_GAMEPAD_BASE: u8 = 0x10;

#[derive(Debug)]
pub enum ControlEvent {
    Connected,
    Message(u16, Vec<u8>),
    /// Host ended the session (reason code).
    Terminated(u32),
    Disconnected,
}

enum Cmd {
    Send { channel: u8, msg_type: u16, payload: Vec<u8>, reliable: bool },
    /// Relative mouse motion: moves still queued are added up into one packet.
    MouseRel(i32, i32),
    Disconnect,
}

#[derive(Clone)]
pub struct ControlSender(Sender<Cmd>);

impl ControlSender {
    pub fn send(&self, msg_type: u16, payload: Vec<u8>) {
        let _ = self.0.send(Cmd::Send { channel: CHANNEL_GENERIC, msg_type, payload, reliable: true });
    }

    pub fn request_idr(&self) {
        let _ = self.0.send(Cmd::Send { channel: CHANNEL_URGENT, msg_type: msg::REQUEST_IDR_FRAME, payload: vec![0; 16], reliable: true });
    }

    /// Ask the host to stop referencing frames `first..=last` (lost on the
    /// network) and to send a frame decodable without them (Sunshine RFI).
    pub fn invalidate_ref_frames(&self, first: u32, last: u32) {
        let mut p = vec![0u8; 24];
        p[0..4].copy_from_slice(&first.to_le_bytes());
        p[8..12].copy_from_slice(&last.to_le_bytes());
        let _ = self.0.send(Cmd::Send { channel: CHANNEL_URGENT, msg_type: msg::INVALIDATE_REF_FRAMES, payload: p, reliable: true });
    }

    /// Acknowledge a long-term reference frame (needed by hosts using RFI).
    pub fn ack_ltr_frame(&self, index: u32) {
        let mut p = vec![0u8; 8];
        p[0..4].copy_from_slice(&index.to_le_bytes());
        let _ = self.0.send(Cmd::Send { channel: CHANNEL_URGENT, msg_type: msg::LTR_FRAME_ACK, payload: p, reliable: true });
    }

    pub fn input(&self, ev: &input::Event) {
        if let input::Event::MouseRel { dx, dy } = ev {
            let _ = self.0.send(Cmd::MouseRel(*dx as i32, *dy as i32));
            return;
        }
        let channel = match ev {
            input::Event::Key { .. } => CHANNEL_KEYBOARD,
            input::Event::Text(_) => CHANNEL_UTF8,
            input::Event::Gamepad(g) => CHANNEL_GAMEPAD_BASE + (g.index as u8 & 0xf),
            _ => CHANNEL_MOUSE,
        };
        // Relative mouse motion is high-rate and loss tolerant; everything else is reliable.
        let reliable = !matches!(ev, input::Event::MouseRel { .. } | input::Event::MouseAbs { .. });
        let _ = self.0.send(Cmd::Send { channel, msg_type: msg::INPUT_DATA, payload: ev.encode(), reliable });
    }

    pub fn disconnect(&self) {
        let _ = self.0.send(Cmd::Disconnect);
    }
}

pub struct ControlClient {
    pub sender: ControlSender,
    pub events: Receiver<ControlEvent>,
}

/// `rumble`: where to send the host's controller rumble requests.
pub fn connect(addr: SocketAddr, connect_data: u32, key: [u8; 16], v2: bool, rumble: Sender<crate::gamepad::Rumble>) -> Result<ControlClient> {
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let socket = UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ev_tx, ev_rx) = mpsc::channel::<ControlEvent>();
    std::thread::Builder::new().name("control".into()).spawn(move || {
        if let Err(e) = run(socket, addr, connect_data, key, v2, cmd_rx, ev_tx.clone(), rumble) {
            warn!("control stream error: {e:#}");
        }
        let _ = ev_tx.send(ControlEvent::Disconnected);
    })?;
    Ok(ControlClient { sender: ControlSender(cmd_tx), events: ev_rx })
}

#[allow(clippy::too_many_arguments)]
fn run(socket: UdpSocket, addr: SocketAddr, connect_data: u32, key: [u8; 16], v2: bool, cmds: Receiver<Cmd>, events: Sender<ControlEvent>, rumble: Sender<crate::gamepad::Rumble>) -> Result<()> {
    let mut host = enet::Host::new(socket, enet::HostSettings { peer_limit: 1, channel_limit: CHANNEL_COUNT, ..Default::default() })
        .map_err(|e| anyhow::anyhow!("enet host: {e:?}"))?;
    let peer_id = host.connect(addr, CHANNEL_COUNT, connect_data).map_err(|e| anyhow::anyhow!("enet connect: {e:?}"))?.id();
    let mut cipher = ControlCipher::new(key, v2, Origin::Client);
    let started = Instant::now();
    let mut connected = false;
    let mut last_ping = Instant::now();
    loop {
        while let Some(ev) = host.service().map_err(|e| anyhow::anyhow!("enet service: {e:?}"))? {
            match ev {
                enet::Event::Connect { peer, .. } => {
                    debug!("control connected");
                    connected = true;
                    for (t, p) in [(msg::START_A, vec![0u8, 0]), (msg::START_B, vec![0, 0, 0, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])] {
                        let _ = peer.send(CHANNEL_GENERIC, &enet::Packet::reliable(cipher.seal(t, &p).as_slice()));
                    }
                    let _ = events.send(ControlEvent::Connected);
                }
                enet::Event::Disconnect { data, .. } => {
                    debug!("control disconnected ({data})");
                    return Ok(());
                }
                enet::Event::Receive { packet, .. } => match cipher.open(packet.data()) {
                    Ok((t, payload)) => {
                        if t == msg::TERMINATION {
                            let code = payload.get(..4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).unwrap_or(0);
                            let _ = events.send(ControlEvent::Terminated(code));
                        } else if t == msg::RUMBLE {
                            if let Some(r) = crate::gamepad::parse_rumble(&payload) {
                                let _ = rumble.send(r);
                            }
                        } else {
                            let _ = events.send(ControlEvent::Message(t, payload));
                        }
                    }
                    Err(e) => warn!("undecryptable control packet ({} bytes): {e}", packet.data().len()),
                },
            }
        }
        if !connected && started.elapsed() > Duration::from_secs(10) {
            anyhow::bail!("control stream connection timed out");
        }
        if connected && last_ping.elapsed() > Duration::from_millis(100) {
            let ping = cipher.seal(msg::PERIODIC_PING, &[0x04, 0, 0, 0, 0, 0, 0, 0]);
            let _ = host.peer_mut(peer_id).send(CHANNEL_GENERIC, &enet::Packet::unreliable(ping.as_slice()));
            last_ping = Instant::now();
        }
        host.flush();
        // Wait for the next input (sent as soon as it arrives) or ~1 ms to service ENet.
        let first = match cmds.recv_timeout(Duration::from_millis(1)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Cmd::Disconnect),
        };
        let mut next = first;
        // Mouse motion waiting to be sent: sent before any other input, so order is kept.
        let mut motion: Option<(i32, i32)> = None;
        let send_motion = |host: &mut enet::Host<UdpSocket>, cipher: &mut ControlCipher, m: &mut Option<(i32, i32)>| {
            if let Some((dx, dy)) = m.take() {
                let ev = input::Event::MouseRel { dx: dx.clamp(-32768, 32767) as i16, dy: dy.clamp(-32768, 32767) as i16 };
                let data = cipher.seal(msg::INPUT_DATA, &ev.encode());
                let _ = host.peer_mut(peer_id).send(CHANNEL_MOUSE, &enet::Packet::unreliable(data.as_slice()));
            }
        };
        while let Some(cmd) = next.take() {
            if !matches!(cmd, Cmd::MouseRel(..)) {
                send_motion(&mut host, &mut cipher, &mut motion);
            }
            match cmd {
                Cmd::MouseRel(dx, dy) if connected => {
                    let m = motion.get_or_insert((0, 0));
                    m.0 += dx;
                    m.1 += dy;
                }
                Cmd::MouseRel(..) => {}
                Cmd::Send { channel, msg_type, payload, reliable } if connected => {
                    let data = cipher.seal(msg_type, &payload);
                    let pkt = if reliable { enet::Packet::reliable(data.as_slice()) } else { enet::Packet::unreliable(data.as_slice()) };
                    let _ = host.peer_mut(peer_id).send(channel, &pkt);
                }
                Cmd::Send { .. } => {}
                Cmd::Disconnect => {
                    host.peer_mut(peer_id).disconnect(0);
                    let deadline = Instant::now() + Duration::from_millis(300);
                    while Instant::now() < deadline {
                        let _ = host.service();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    return Ok(());
                }
            }
            next = match cmds.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Cmd::Disconnect),
            };
        }
        send_motion(&mut host, &mut cipher, &mut motion);
        // Push queued inputs onto the wire now rather than at the next service.
        host.flush();
    }
}
