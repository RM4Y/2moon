//! Control stream server: ENet host on the control port. Decrypts client
//! messages (keep-alives, IDR requests, input) and notifies the client when
//! the session ends.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use moon_proto::control::{input, msg, termination, ControlCipher, Origin};
use rusty_enet as enet;
use tracing::{debug, info, warn};

use crate::input::Injector;
use crate::state::Host;

const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

struct Client {
    peer: enet::PeerID,
    cipher: ControlCipher,
    last_rx: Instant,
    session_gen: u64,
}

pub fn spawn(host: Arc<Host>, make_injector: Box<dyn Fn() -> Box<dyn Injector> + Send>) -> Result<()> {
    let sock = {
        let s = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, host.cfg.control_port)).into())?;
        let u: std::net::UdpSocket = s.into();
        u.set_nonblocking(true)?;
        u
    };
    std::thread::Builder::new().name("control".into()).spawn(move || {
        if let Err(e) = run(host, sock, make_injector) {
            warn!("control server stopped: {e:#}");
        }
    })?;
    Ok(())
}

fn run(host: Arc<Host>, sock: std::net::UdpSocket, make_injector: Box<dyn Fn() -> Box<dyn Injector> + Send>) -> Result<()> {
    let mut enet_host = enet::Host::new(sock, enet::HostSettings { peer_limit: 4, channel_limit: 0x20, ..Default::default() }).map_err(|e| anyhow::anyhow!("enet: {e:?}"))?;
    info!("control stream on udp/{}", host.cfg.control_port);
    let mut client: Option<Client> = None;
    let mut injector: Option<Box<dyn Injector>> = None;
    let gen_rx = host.session_gen.subscribe();
    loop {
        while let Some(ev) = enet_host.service().map_err(|e| anyhow::anyhow!("enet service: {e:?}"))?.map(|e| e.no_ref()) {
            match ev {
                enet::EventNoRef::Connect { peer: pid, data } => {
                    let Some(session) = host.current_session() else {
                        warn!("control connection without a launched session; rejecting");
                        enet_host.peer_mut(pid).disconnect(0);
                        continue;
                    };
                    if data != 0 && data != session.control_connect_data {
                        warn!("control connect data mismatch ({data} != {}); rejecting", session.control_connect_data);
                        enet_host.peer_mut(pid).disconnect(0);
                        continue;
                    }
                    if let Some(old) = client.take() {
                        enet_host.peer_mut(old.peer).disconnect(0);
                    }
                    let v2 = host.stream_params().encryption & 1 != 0;
                    info!("control client connected from {:?} (encryption v{})", enet_host.peer_mut(pid).address(), if v2 { 2 } else { 1 });
                    let id = pid;
                    let mut c = Client { peer: id, cipher: ControlCipher::new(session.rikey, v2, Origin::Host), last_rx: Instant::now(), session_gen: *gen_rx.borrow() };
                    // Tell the client we stream SDR.
                    let hdr = c.cipher.seal(msg::HDR_MODE, &[0u8; 27]);
                    let _ = enet_host.peer_mut(id).send(0, &enet::Packet::reliable(hdr.as_slice()));
                    client = Some(c);
                    if injector.is_none() {
                        injector = Some(make_injector());
                    }
                }
                enet::EventNoRef::Disconnect { peer: pid, .. } => {
                    if client.as_ref().map(|c| c.peer) == Some(pid) {
                        info!("control client disconnected; stopping stream");
                        client = None;
                        host.stop_stream();
                        if let Some(i) = injector.as_mut() {
                            i.reset();
                        }
                    }
                }
                enet::EventNoRef::Receive { peer: pid, packet, .. } => {
                    let Some(c) = client.as_mut().filter(|c| c.peer == pid) else { continue };
                    match c.cipher.open(packet.data()) {
                        Ok((t, payload)) => {
                            c.last_rx = Instant::now();
                            handle_message(&host, t, &payload, &mut injector);
                        }
                        Err(e) => debug!("dropping undecryptable control packet: {e}"),
                    }
                }
            }
        }

        if let Some(c) = client.as_mut() {
            let gen = *gen_rx.borrow();
            let ended = gen != c.session_gen && host.current_session().is_none();
            if ended || c.last_rx.elapsed() > IDLE_TIMEOUT {
                if ended {
                    info!("session ended on the host; notifying client");
                    let term = c.cipher.seal(msg::TERMINATION, &termination::GRACEFUL.to_be_bytes());
                    let _ = enet_host.peer_mut(c.peer).send(0, &enet::Packet::reliable(term.as_slice()));
                    enet_host.flush();
                } else {
                    warn!("control client timed out");
                }
                enet_host.peer_mut(c.peer).disconnect_later(0);
                client = None;
                host.stop_stream();
                if let Some(i) = injector.as_mut() {
                    i.reset();
                }
            } else {
                c.session_gen = gen;
            }
        }
        enet_host.flush();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn handle_message(host: &Host, t: u16, payload: &[u8], injector: &mut Option<Box<dyn Injector>>) {
    match t {
        msg::INPUT_DATA => match input::Event::decode(payload) {
            Some(ev) => {
                host.input_events.fetch_add(1, Ordering::Relaxed);
                if let Some(i) = injector.as_mut() {
                    i.handle(&ev);
                }
            }
            None => debug!("unknown input packet ({} bytes, magic {:02x?})", payload.len(), payload.get(4..8)),
        },
        msg::INVALIDATE_REF_FRAMES if payload.len() >= 12 => {
            // { first u32 LE, reserved, last u32 LE, reserved[3] }
            let first = u32::from_le_bytes(payload[0..4].try_into().unwrap());
            let last = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            debug!("client lost frames {first}..={last}: invalidating references");
            host.invalidate_ref_frames(first, last);
        }
        msg::REQUEST_IDR_FRAME | msg::INVALIDATE_REF_FRAMES => {
            debug!("client requested an IDR frame");
            host.request_idr();
        }
        msg::LTR_FRAME_ACK => {}
        msg::PERIODIC_PING | msg::START_A | msg::START_B | msg::LOSS_STATS | msg::FRAME_STATS => {}
        other => debug!("unhandled control message 0x{other:04x} ({} bytes)", payload.len()),
    }
}
