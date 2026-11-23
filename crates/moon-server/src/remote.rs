//! Remote access: the host is reachable by its code, no port to open (see `moon-tunnel`).
//! The code is stable (`remote.key`); paired clients get it via `MoonRemoteId`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::state::Host;

#[derive(Debug, Clone, Default)]
pub enum Status {
    #[default]
    Off,
    Starting,
    Online,
    Failed(String),
}

impl Status {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Status::Off => serde_json::json!({"state": "off"}),
            Status::Starting => serde_json::json!({"state": "starting"}),
            Status::Online => serde_json::json!({"state": "online"}),
            Status::Failed(e) => serde_json::json!({"state": "failed", "error": e}),
        }
    }
}

/// The host's persistent remote access key (created on first use).
pub fn load_or_create_key(state_dir: &Path) -> Result<[u8; 32]> {
    let path = state_dir.join("remote.key");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(k) = hex::decode(s.trim()).map_err(anyhow::Error::from).and_then(|v| <[u8; 32]>::try_from(v).map_err(|_| anyhow::anyhow!("size"))) {
            return Ok(k);
        }
        warn!("remote access: {} is damaged, a new code is created", path.display());
    }
    let k = moon_tunnel::generate_secret();
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(&path, hex::encode(k)).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(k)
}

/// Standard GameStream HTTP port (HTTPS and RTSP derive from it).
const STD_HTTP: u16 = 47989;

/// A new code: the old one stops working at once (its connections are closed and
/// nobody can reach the host with it any more). Returns the new code.
pub fn regenerate(host: &Host) -> Result<String> {
    let k = moon_tunnel::generate_secret();
    let path = host.cfg.state_dir.join("remote.key");
    let tmp = path.with_extension("tmp");
    std::fs::create_dir_all(&host.cfg.state_dir)?;
    std::fs::write(&tmp, hex::encode(k))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    host.reset_remote_code();
    // Wakes the tunnel task, which sees the new key and restarts with it.
    host.settings.changed.send_modify(|g| *g += 1);
    let id = moon_tunnel::id_of(&k).to_string();
    info!("remote access: new code {id} (the old code no longer works)");
    Ok(id)
}

/// The GameStream services, served in-process for tunnel connections.
struct Services {
    host: Arc<Host>,
    tls: tokio_rustls::TlsAcceptor,
}

impl moon_tunnel::TcpService for Services {
    fn serve(&self, port: u16, io: moon_tunnel::Io, _remote: moon_tunnel::HostId) {
        let host = self.host.clone();
        // Seen as 127.0.0.1 so that the UDP flows (relayed from 127.0.0.1) match the
        // session; `tunnel` keeps it from counting as a local connection.
        let via = crate::http::Via { peer: ([127, 0, 0, 1], 0).into(), local: None, tunnel: true };
        let c = &host.cfg;
        // A client that only knows the code asks for the standard ports.
        if port == c.http_port || port == STD_HTTP {
            tokio::spawn(crate::http::serve_plain(host, io, via));
        } else if port == c.https_port || port == STD_HTTP - 5 {
            tokio::spawn(crate::http::serve_tls(host, self.tls.clone(), io, via));
        } else if port == c.rtsp_port || port == STD_HTTP + 21 {
            tokio::spawn(async move {
                if let Err(e) = crate::rtsp::connection(host, io).await {
                    tracing::debug!("RTSP (remote access): {e:#}");
                }
            });
        }
    }
}

/// Follows the "remote" setting: opens the tunnel endpoint when on, closes it when off.
pub fn spawn(host: Arc<Host>) {
    let mut changed = host.settings.changed.subscribe();
    tokio::spawn(async move {
        let mut running: Option<moon_tunnel::HostTunnel> = None;
        loop {
            let want = host.settings.get().remote;
            // New code: drop the tunnel of the old one (its clients are disconnected).
            let key_id = load_or_create_key(&host.cfg.state_dir).ok().map(|k| moon_tunnel::id_of(&k));
            if let Some(t) = running.take_if(|t| Some(t.id()) != key_id) {
                t.close().await;
                info!("remote access: old code revoked");
            }
            if want && running.is_none() {
                set(&host, Status::Starting);
                match start(&host).await {
                    Ok(t) => {
                        info!("remote access on: code {}", t.id());
                        running = Some(t);
                        let h = host.clone();
                        // "Online" once the relay answers (the host can then be reached from anywhere).
                        let online = running.as_ref().map(|t| t.online());
                        if let Some(f) = online {
                            tokio::select! {
                                _ = f => set(&h, Status::Online),
                                _ = tokio::time::sleep(std::time::Duration::from_secs(20)) => set(&h, Status::Failed("pas de connexion au serveur relais (Internet coupé ?)".into())),
                            }
                        }
                    }
                    Err(e) => {
                        warn!("remote access: {e:#}");
                        set(&host, Status::Failed(format!("{e:#}")));
                    }
                }
            } else if !want {
                if let Some(t) = running.take() {
                    t.close().await;
                    info!("remote access disabled in the settings");
                }
                set(&host, Status::Off);
            }
            let retry = matches!(*host.remote_status.lock().unwrap(), Status::Failed(_));
            tokio::select! {
                r = changed.changed() => if r.is_err() { break },
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)), if retry => {
                    // Not online yet: maybe Internet came back.
                    if let Some(t) = &running {
                        if tokio::time::timeout(std::time::Duration::from_secs(5), t.online()).await.is_ok() {
                            set(&host, Status::Online);
                        }
                    }
                }
            }
        }
    });
}

async fn start(host: &Arc<Host>) -> Result<moon_tunnel::HostTunnel> {
    let key = load_or_create_key(&host.cfg.state_dir)?;
    let c = &host.cfg;
    let tcp = vec![c.http_port, c.https_port, c.rtsp_port, STD_HTTP, STD_HTTP - 5, STD_HTTP + 21];
    let mut udp = vec![c.video_port, c.control_port, c.audio_port];
    udp.extend(c.mic_port);
    let services = Arc::new(Services { host: host.clone(), tls: crate::http::tls_acceptor(host)? });
    moon_tunnel::HostTunnel::start(key, services, tcp, udp).await
}

fn set(host: &Host, s: Status) {
    *host.remote_status.lock().unwrap() = s;
}
