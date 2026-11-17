mod http;
mod state;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "moon-server", about = "2moon: GameStream/Moonlight-compatible streaming host written in Rust")]
struct Args {
    /// Base HTTP port (GameStream default 47989); other ports are derived from it.
    #[arg(long, default_value_t = 47989)]
    port: u16,
    /// Directory holding the host identity and paired clients.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Host name advertised to clients.
    #[arg(long)]
    name: Option<String>,
    /// Accept this PIN for every pairing request (for automation/tests).
    #[arg(long)]
    pin: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let state_dir = args.state_dir.unwrap_or_else(|| {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
        base.join("2moon-server")
    });
    let hostname = args.name.unwrap_or_else(|| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "2moon".into()));
    let mut cfg = state::Config::with_base_port(args.port, state_dir, hostname);
    cfg.preset_pin = args.pin;
    let host = state::Host::load(cfg)?;
    info!("2moon host '{}' uuid {}", host.cfg.hostname, host.uuid);

    // PINs typed in the console are delivered to waiting pairing requests.
    let h = host.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let pin = line.trim();
            if !pin.is_empty() {
                let n = h.submit_pin(pin);
                info!("PIN delivered to {n} pending pairing request(s)");
            }
            line.clear();
        }
    });

    http::run(host).await
}
