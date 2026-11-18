pub mod control;
pub mod gamepad;
pub mod gui;
pub mod http;
pub mod keygrab;
pub mod probe;
pub mod rtsp;
pub mod session;
pub mod store;
pub mod theme;
pub mod video_gl;
pub mod window;

/// This computer's name, shown on the host when pairing.
pub fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "2moon".into())
}

/// Split `host[:port]` (IPv6 in brackets) into address + HTTP port (default 47989).
pub fn split_host_port(s: &str) -> (String, u16) {
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
