//! Advertise the host on the LAN as `_nvstream._tcp` so clients discover it.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tracing::info;

pub fn advertise(instance: &str, http_port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("{}.local.", instance.replace(['.', ' '], "-"));
    let info = ServiceInfo::new("_nvstream._tcp.local.", instance, &host, "", http_port, None::<std::collections::HashMap<String, String>>)?.enable_addr_auto();
    daemon.register(info)?;
    info!("advertising '{instance}' via mDNS (_nvstream._tcp, port {http_port})");
    Ok(daemon)
}
