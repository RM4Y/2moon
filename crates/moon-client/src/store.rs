//! Persistent client data: our identity and the hosts we paired with.

use std::path::PathBuf;

use anyhow::Result;
use moon_proto::crypto::Identity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownHost {
    pub name: String,
    pub uuid: String,
    pub address: String,
    pub http_port: u16,
    pub server_cert_pem: String,
    /// 2moon remote access code, to reach the host from outside its network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

pub struct Store {
    pub dir: PathBuf,
    pub identity: Identity,
    pub hosts: Vec<KnownHost>,
}

impl Store {
    pub fn default_dir() -> PathBuf {
        // Windows: %APPDATA%\2moon-client
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|_| cfg!(windows)) {
            return PathBuf::from(appdata).join("2moon-client");
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
        base.join("2moon-client")
    }

    pub fn load(dir: PathBuf) -> Result<Self> {
        let identity = Identity::load_or_generate(&dir, "client", "NVIDIA GameStream Client")?;
        let hosts = match std::fs::read_to_string(dir.join("hosts.json")) {
            Ok(s) => serde_json::from_str(&s)?,
            Err(_) => Vec::new(),
        };
        Ok(Self { dir, identity, hosts })
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.dir.join("hosts.json"), serde_json::to_string_pretty(&self.hosts)?)?;
        Ok(())
    }

    pub fn upsert(&mut self, h: KnownHost) -> Result<()> {
        self.hosts.retain(|x| !(x.uuid == h.uuid || (x.address == h.address && x.http_port == h.http_port)));
        self.hosts.push(h);
        self.save()
    }

    pub fn find(&self, address: &str, http_port: u16) -> Option<&KnownHost> {
        self.hosts
            .iter()
            .find(|h| (h.address == address && h.http_port == http_port) || h.name.eq_ignore_ascii_case(address))
    }
}
