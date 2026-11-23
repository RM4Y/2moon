//! Remote access without open ports: client <-> host traffic in one iroh QUIC connection
//! (hole punching, relay as a last resort).
//! TCP: one QUIC stream per connection, prefixed by the port (u16 BE).
//! UDP: QUIC datagrams, prefixed by the port (u16 BE).
//! The host id is a public key, so only the real host can answer for it.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointId, SecretKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

pub use iroh::EndpointId as HostId;

pub const ALPN: &[u8] = b"2moon/gamestream/1";

/// A bidirectional byte stream: a TCP connection carried by the tunnel.
pub type Io = tokio::io::Join<RecvStream, SendStream>;

/// Parses a host code (the endpoint id, 64 hex characters; spaces and dashes are ignored).
pub fn parse_id(s: &str) -> Option<EndpointId> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_lowercase();
    if clean.len() != 64 {
        return None;
    }
    clean.parse().ok()
}

/// Largest video packet (`packetSize`) that fits in one datagram of this connection.
fn packet_size_for(max_datagram: Option<usize>) -> u32 {
    // Datagram = port (2) + RTP header and reserved bytes (16) + packetSize.
    let max = max_datagram.unwrap_or(1100).saturating_sub(2 + 16);
    (max.min(1392) as u32).max(512) & !15
}

/// How the connection currently travels.
fn describe_path(conn: &Connection) -> String {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected());
    match selected {
        Some(p) if p.is_relay() => format!("relais ({} ms)", p.rtt().as_millis()),
        Some(p) => format!("direct ({} ms)", p.rtt().as_millis()),
        None => "en cours".into(),
    }
}

async fn bind(secret: Option<SecretKey>, accept: bool) -> Result<Endpoint> {
    let mut b = Endpoint::builder(presets::N0);
    if let Some(k) = secret {
        b = b.secret_key(k);
    }
    if accept {
        b = b.alpns(vec![ALPN.to_vec()]);
    }
    b.bind().await.map_err(|e| anyhow!("{e}")).context("démarrage du tunnel")
}

// ================================================================ host

/// Serves the TCP connections arriving through the tunnel (in-process, so the
/// host can tell them apart from local connections).
pub trait TcpService: Send + Sync + 'static {
    /// `port` is one of the ports given to [`HostTunnel::start`].
    fn serve(&self, port: u16, io: Io, remote: EndpointId);
}

pub struct HostTunnel {
    endpoint: Endpoint,
}

impl HostTunnel {
    /// `secret`: the host's persistent key (its id must not change). TCP streams for
    /// `tcp_ports` go to `tcp`; datagrams for `udp_ports` go to 127.0.0.1:port.
    pub async fn start(secret: [u8; 32], tcp: Arc<dyn TcpService>, tcp_ports: Vec<u16>, udp_ports: Vec<u16>) -> Result<Self> {
        let endpoint = bind(Some(SecretKey::from_bytes(&secret)), true).await?;
        let ep = endpoint.clone();
        let (tcp_ports, udp_ports) = (Arc::new(tcp_ports), Arc::new(udp_ports));
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let (tcp, tcp_ports, udp_ports) = (tcp.clone(), tcp_ports.clone(), udp_ports.clone());
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(e) => return debug!("tunnel: refused connection: {e}"),
                    };
                    let remote = conn.remote_id();
                    info!("tunnel: client {} connected ({})", remote.fmt_short(), describe_path(&conn));
                    tokio::join!(host_streams(&conn, tcp, &tcp_ports), host_datagrams(&conn, &udp_ports));
                    info!("tunnel: client {} disconnected", remote.fmt_short());
                });
            }
        });
        Ok(Self { endpoint })
    }

    /// The code clients use to reach this host.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Resolves once the host can be reached from outside (relay connected).
    pub async fn online(&self) {
        self.endpoint.online().await
    }

    pub async fn close(self) {
        self.endpoint.close().await
    }
}

/// A new persistent host key.
pub fn generate_secret() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// The code of the host using this key.
pub fn id_of(secret: &[u8; 32]) -> EndpointId {
    SecretKey::from_bytes(secret).public()
}

async fn host_streams(conn: &Connection, tcp: Arc<dyn TcpService>, ports: &[u16]) {
    while let Ok((send, mut recv)) = conn.accept_bi().await {
        let Ok(port) = recv.read_u16().await else { continue };
        if !ports.contains(&port) {
            warn!("tunnel: TCP port {port} refused");
            continue;
        }
        tcp.serve(port, tokio::io::join(recv, send), conn.remote_id());
    }
}

async fn host_datagrams(conn: &Connection, ports: &[u16]) {
    let mut sockets: HashMap<u16, Arc<UdpSocket>> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    while let Ok(d) = conn.read_datagram().await {
        if d.len() < 2 {
            continue;
        }
        let port = u16::from_be_bytes([d[0], d[1]]);
        if !ports.contains(&port) {
            continue;
        }
        let sock = match sockets.get(&port) {
            Some(s) => s.clone(),
            None => {
                // One local socket per flow: the host service sees it as the client.
                let s = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("tunnel: UDP socket: {e}");
                        continue;
                    }
                };
                if s.connect((Ipv4Addr::LOCALHOST, port)).await.is_err() {
                    continue;
                }
                let s = Arc::new(s);
                sockets.insert(port, s.clone());
                let (s2, c2) = (s.clone(), conn.clone());
                tasks.spawn(async move { udp_to_tunnel(&s2, &c2, port, None).await });
                s
            }
        };
        let _ = sock.send(&d[2..]).await;
    }
    tasks.abort_all();
}

/// Datagrams from a local socket into the tunnel, tagged with `port`. `last_peer`
/// (client side) remembers who sent them, to deliver the answers.
async fn udp_to_tunnel(sock: &UdpSocket, conn: &Connection, port: u16, last_peer: Option<&Mutex<Option<SocketAddr>>>) {
    let mut buf = vec![0u8; 2 + 65536];
    buf[..2].copy_from_slice(&port.to_be_bytes());
    loop {
        let (n, from) = tokio::select! {
            r = sock.recv_from(&mut buf[2..]) => match r {
                Ok(r) => r,
                Err(_) => continue, // ICMP "port unreachable" from a previous send
            },
            _ = conn.closed() => return,
        };
        if let Some(p) = last_peer {
            *p.lock().unwrap() = Some(from);
        }
        match conn.send_datagram(bytes::Bytes::copy_from_slice(&buf[..2 + n])) {
            Ok(()) => {}
            Err(iroh::endpoint::SendDatagramError::TooLarge) => debug!("tunnel: {n}-byte datagram too large (port {port})"),
            Err(_) => return,
        }
    }
}

// ================================================================ client

type UdpRelay = (SocketAddr, Arc<Mutex<Option<SocketAddr>>>, Arc<UdpSocket>);

/// A connection to one host. Cheap to share; it stays open while referenced.
pub struct ClientTunnel {
    endpoint: Endpoint,
    conn: Connection,
    /// Remote UDP port -> local relay socket address, the last local sender, the socket.
    udp: Arc<Mutex<HashMap<u16, UdpRelay>>>,
}

impl ClientTunnel {
    async fn connect(host: EndpointId) -> Result<Arc<Self>> {
        let endpoint = bind(None, false).await?;
        let conn = tokio::time::timeout(Duration::from_secs(20), endpoint.connect(host, ALPN))
            .await
            .context("l'hôte ne répond pas par l'accès à distance")?
            .map_err(|e| anyhow!("accès à distance : {e}"))?;
        let udp: Arc<Mutex<HashMap<u16, UdpRelay>>> = Default::default();
        // Datagrams from the host go back to the local socket that sent the last one.
        let (u2, c2) = (udp.clone(), conn.clone());
        tokio::spawn(async move {
            while let Ok(d) = c2.read_datagram().await {
                if d.len() < 2 {
                    continue;
                }
                let port = u16::from_be_bytes([d[0], d[1]]);
                let entry = u2.lock().unwrap().get(&port).map(|(_, p, s)| (*p.lock().unwrap(), s.clone()));
                if let Some((Some(to), sock)) = entry {
                    let _ = sock.send_to(&d[2..], to).await;
                }
            }
        });
        let t = Arc::new(Self { endpoint, conn, udp });
        info!("tunnel to {} open ({})", host.fmt_short(), t.path());
        Ok(t)
    }

    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    /// A TCP connection to `port` on the host.
    pub async fn open_tcp(&self, port: u16) -> Result<Io> {
        let (mut send, recv) = self.conn.open_bi().await.map_err(|e| anyhow!("accès à distance : {e}"))?;
        send.write_u16(port).await?;
        Ok(tokio::io::join(recv, send))
    }

    /// Local address standing for UDP `port` on the host: send to it as if it were the host.
    pub async fn udp(&self, port: u16) -> Result<SocketAddr> {
        if let Some((addr, ..)) = self.udp.lock().unwrap().get(&port) {
            return Ok(*addr);
        }
        let sock = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let addr = sock.local_addr()?;
        let peer = Arc::new(Mutex::new(None));
        let (s2, p2, c2) = (sock.clone(), peer.clone(), self.conn.clone());
        tokio::spawn(async move { udp_to_tunnel(&s2, &c2, port, Some(&p2)).await });
        self.udp.lock().unwrap().insert(port, (addr, peer, sock));
        Ok(addr)
    }

    /// Video packet size to ask the host for, so that each packet fits in one datagram.
    pub fn packet_size(&self) -> u32 {
        packet_size_for(self.conn.max_datagram_size())
    }

    /// "direct (12 ms)" or "relais (40 ms)".
    pub fn path(&self) -> String {
        describe_path(&self.conn)
    }
}

impl Drop for ClientTunnel {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"bye");
        let ep = self.endpoint.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { ep.close().await });
        }
    }
}

type Pending = tokio::sync::watch::Receiver<Option<Result<Arc<ClientTunnel>, String>>>;

enum Slot {
    Connecting(Pending),
    Ready(Arc<ClientTunnel>),
}

static POOL: Mutex<Option<HashMap<EndpointId, Slot>>> = Mutex::new(None);

/// The tunnel to `host`, opened on first use and shared afterwards. The connection
/// attempt goes on in the background even if the caller gives up waiting.
pub async fn tunnel(host: EndpointId) -> Result<Arc<ClientTunnel>> {
    let mut rx = {
        let mut pool = POOL.lock().unwrap();
        let pool = pool.get_or_insert_with(HashMap::new);
        match pool.get(&host) {
            Some(Slot::Ready(t)) if !t.is_closed() => return Ok(t.clone()),
            Some(Slot::Connecting(rx)) => rx.clone(),
            _ => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                pool.insert(host, Slot::Connecting(rx.clone()));
                tokio::spawn(async move {
                    let r = ClientTunnel::connect(host).await;
                    {
                        let mut pool = POOL.lock().unwrap();
                        let pool = pool.get_or_insert_with(HashMap::new);
                        match &r {
                            Ok(t) => pool.insert(host, Slot::Ready(t.clone())),
                            Err(_) => pool.remove(&host),
                        };
                    }
                    let _ = tx.send(Some(r.map_err(|e| format!("{e:#}"))));
                });
                rx
            }
        }
    };
    loop {
        if let Some(r) = rx.borrow().as_ref() {
            return r.clone().map_err(|e| anyhow!(e));
        }
        if rx.changed().await.is_err() {
            bail!("accès à distance interrompu");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let id = SecretKey::generate().public();
        let s = id.to_string();
        assert_eq!(parse_id(&s), Some(id));
        assert_eq!(parse_id(&format!(" {}-{} ", &s[..32], &s[32..].to_uppercase())), Some(id));
        assert_eq!(parse_id("192.168.1.10"), None);
    }

    #[test]
    fn packet_size() {
        assert_eq!(packet_size_for(Some(1452)), 1392);
        assert_eq!(packet_size_for(Some(1200)), 1168);
        assert!(packet_size_for(None) + 18 <= 1100);
    }
}
