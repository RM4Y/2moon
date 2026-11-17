//! Advertise the host on the LAN as `_nvstream._tcp` so clients discover it.
//! Only physical interfaces are used (container bridges and VPN/virtual links are skipped).

use anyhow::Result;
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use tracing::info;

fn is_virtual(name: &str) -> bool {
    ["lo", "docker", "br-", "veth", "virbr", "vnet", "tun", "tap", "wg", "podman", "cni", "flannel", "tailscale", "zt"].iter().any(|p| name.starts_with(p))
}

pub fn advertise(instance: &str, http_port: u16) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let mut addrs = Vec::new();
    for itf in if_addrs::get_if_addrs()? {
        if is_virtual(&itf.name) {
            let _ = daemon.disable_interface(IfKind::Name(itf.name.clone()));
            continue;
        }
        let ip = itf.ip();
        if ip.is_ipv4() || !matches!(ip, std::net::IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80) {
            addrs.push(ip);
        }
    }
    let host = format!("{}.local.", instance.replace(['.', ' '], "-"));
    let info = ServiceInfo::new("_nvstream._tcp.local.", instance, &host, &addrs[..], http_port, None::<std::collections::HashMap<String, String>>)?;
    daemon.register(info)?;
    info!("advertising '{instance}' via mDNS on {addrs:?} (port {http_port})");
    Ok(daemon)
}
