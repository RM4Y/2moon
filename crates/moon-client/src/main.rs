use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use moon_client::http::GsClient;
use moon_client::store::{KnownHost, Store};

#[derive(Parser)]
#[command(name = "moon-client", about = "2moon: GameStream/Moonlight-compatible streaming client written in Rust")]
struct Args {
    /// Directory holding the client identity and known hosts.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair with a host using a PIN (type the same PIN on the host).
    Pair {
        host: String,
        #[arg(long)]
        pin: Option<String>,
    },
    /// List the applications of a paired host.
    List { host: String },
    /// Show host information.
    Info { host: String },
    /// Quit the running application on the host.
    Quit { host: String },
    /// Debug: launch, negotiate RTSP and dump raw UDP packets for a few seconds.
    Probe {
        host: String,
        #[arg(long, default_value = "probe-dump")]
        out: PathBuf,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
    },
}

/// Split `host[:port]` (IPv6 in brackets) into address + HTTP port.
pub fn split_host(s: &str) -> (String, u16) {
    if let Some(rest) = s.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            return (h.to_string(), p.trim_start_matches(':').parse().unwrap_or(47989));
        }
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().unwrap_or(47989)),
        _ => (s.to_string(), 47989),
    }
}

fn client_for(store: &Store, host: &str) -> GsClient {
    let (addr, port) = split_host(host);
    let mut c = GsClient::new(&addr, port, store.identity.clone());
    if let Some(k) = store.find(&addr, port) {
        c.host = k.address.clone();
        c.http_port = k.http_port;
        c.server_cert = moon_proto::crypto::pem_decode_cert(&k.server_cert_pem).ok();
    }
    c
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let mut store = Store::load(args.state_dir.unwrap_or_else(Store::default_dir))?;

    match args.cmd {
        Cmd::Pair { host, pin } => {
            let mut c = client_for(&store, &host);
            c.server_cert = None;
            let pin = pin.unwrap_or_else(|| format!("{:04}", rand::random::<u16>() % 10000));
            println!("Enter PIN {pin} on the host to pair...");
            let device = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "2moon".into());
            c.pair(&pin, &device).await?;
            let info = c.server_info().await?;
            store.upsert(KnownHost {
                name: info.hostname.clone(),
                uuid: info.uuid.clone(),
                address: c.host.clone(),
                http_port: c.http_port,
                server_cert_pem: moon_proto::crypto::pem_encode_cert(c.server_cert.as_ref().unwrap()),
            })?;
            println!("Paired with {} (paired status reported by host: {})", info.hostname, info.paired);
        }
        Cmd::List { host } => {
            let c = client_for(&store, &host);
            c.server_cert.as_ref().context("not paired with this host")?;
            for a in c.app_list().await? {
                println!("{}\t{}", a.id, a.title);
            }
        }
        Cmd::Info { host } => {
            let c = client_for(&store, &host);
            println!("{:#?}", c.server_info().await?);
        }
        Cmd::Probe { host, out, seconds } => {
            let c = client_for(&store, &host);
            moon_client::probe::run(&c, &out, seconds).await?;
        }
        Cmd::Quit { host } => {
            client_for(&store, &host).cancel().await?;
            println!("Session ended");
        }
    }
    Ok(())
}
