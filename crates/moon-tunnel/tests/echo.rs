//! End-to-end check through the real iroh network (needs Internet access):
//! `cargo test -p moon-tunnel -- --ignored`.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use moon_tunnel::{HostTunnel, Io, TcpService};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Echo;

impl TcpService for Echo {
    fn serve(&self, _port: u16, io: Io, _remote: moon_tunnel::HostId) {
        tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(io);
            let _ = tokio::io::copy(&mut r, &mut w).await;
            let _ = w.shutdown().await;
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tcp_and_udp_echo() {
    // UDP echo service on the "host".
    let udp = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = udp.recv_from(&mut buf).await {
            let _ = udp.send_to(&buf[..n], from).await;
        }
    });

    let host = HostTunnel::start(moon_tunnel::generate_secret(), Arc::new(Echo), vec![47989], vec![udp_port]).await.unwrap();
    host.online().await;
    let t = tokio::time::timeout(Duration::from_secs(30), moon_tunnel::tunnel(host.id())).await.unwrap().unwrap();
    println!("path: {}, packet size {}", t.path(), t.packet_size());

    let mut io = t.open_tcp(47989).await.unwrap();
    io.write_all(b"hello over the tunnel").await.unwrap();
    let mut got = [0u8; 21];
    io.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"hello over the tunnel");

    let relay = t.udp(udp_port).await.unwrap();
    let client = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let payload = vec![7u8; t.packet_size() as usize + 16];
    let mut ok = false;
    for _ in 0..20 {
        client.send_to(&payload, relay).await.unwrap();
        let mut buf = [0u8; 2048];
        if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buf)).await {
            assert_eq!(&buf[..n], &payload[..]);
            ok = true;
            break;
        }
    }
    assert!(ok, "no UDP echo through the tunnel");
    host.close().await;
}
