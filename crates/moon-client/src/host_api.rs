//! Host mode link to this computer's host: its admin API over local HTTP, and its service.
//! Auth: the token from `local-admin.json` (loopback only), else the panel's login.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpStream;

/// Default admin port of the host (GameStream port 47989 + 1).
pub const DEFAULT_ADMIN_PORT: u16 = 47990;

/// Where the host keeps its identity and settings (same rules as moon-server).
/// `MOON_SERVER_STATE_DIR` points elsewhere (tests, a host started with --state-dir).
pub fn server_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MOON_SERVER_STATE_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(target_os = "macos") {
        return PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/2moon-server");
    }
    if cfg!(windows) {
        return PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("2moon");
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("2moon-server")
}

#[derive(Clone)]
pub struct HostApi {
    port: u16,
    token: Option<String>,
    /// Admin session cookie after a login (when there is no local token).
    cookie: Arc<Mutex<Option<String>>>,
}

/// An API error with the host's message (in French, from the admin panel).
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl HostApi {
    /// The local host, through its `local-admin.json`; `None` if it cannot be read
    /// (host never started, or its folder is not ours: use [`Self::with_login`]).
    pub fn local() -> Option<Self> {
        let dir = server_state_dir();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("local-admin.json")).ok()?).ok()?;
        let port = v.get("admin_port")?.as_u64()? as u16;
        let token = v.get("token")?.as_str()?.to_string();
        Some(Self { port, token: Some(token), cookie: Default::default() })
    }

    /// The local host on `port`, authenticated by logging in.
    pub fn with_login(port: u16) -> Self {
        Self { port, token: None, cookie: Default::default() }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Needs a login (no local token) and has none yet.
    pub fn needs_login(&self) -> bool {
        self.token.is_none() && self.cookie.lock().unwrap().is_none()
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let (_, body) = self.request("GET", path, None, "application/json").await?;
        Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let (_, body) = self.request("POST", path, Some(Bytes::from(body.to_string())), "application/json").await?;
        Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// Raw GET (app covers); `None` on 404.
    pub async fn get_bytes(&self, path: &str) -> Result<Option<Bytes>> {
        match self.request("GET", path, None, "application/octet-stream").await {
            Ok((_, b)) => Ok(Some(b)),
            Err(e) if e.downcast_ref::<ApiError>().is_some_and(|a| a.status == 404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn login(&self, user: &str, password: &str) -> Result<()> {
        self.post("/api/login", serde_json::json!({"user": user, "password": password})).await.map(|_| ())
    }

    async fn request(&self, method: &str, path: &str, body: Option<Bytes>, ctype: &str) -> Result<(u16, Bytes)> {
        let tcp = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(("127.0.0.1", self.port))).await.context("the host does not answer")??;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = hyper::Request::builder().method(method).uri(path).header("Host", format!("127.0.0.1:{}", self.port)).header("X-2moon", "1").header("Content-Type", ctype);
        if let Some(t) = &self.token {
            req = req.header("X-2moon-Token", t);
        }
        if let Some(c) = self.cookie.lock().unwrap().clone() {
            req = req.header("Cookie", c);
        }
        let req = req.body(Full::new(body.unwrap_or_default()))?;
        let fut = async {
            let res = sender.send_request(req).await?;
            let status = res.status().as_u16();
            if let Some(c) = res.headers().get(hyper::header::SET_COOKIE).and_then(|v| v.to_str().ok()) {
                let pair = c.split(';').next().unwrap_or("").to_string();
                *self.cookie.lock().unwrap() = (!pair.ends_with('=')).then_some(pair);
            }
            let body = res.into_body().collect().await?.to_bytes();
            Ok::<_, anyhow::Error>((status, body))
        };
        let (status, body) = tokio::time::timeout(Duration::from_secs(10), fut).await.context("the host did not answer in time")??;
        if !(200..300).contains(&status) {
            let msg = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from)).unwrap_or_else(|| format!("erreur {status}"));
            return Err(ApiError { status, message: msg }.into());
        }
        Ok((status, body))
    }
}

// ---------------------------------------------------------------- service

/// How the host runs on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Installed and running (or at least started).
    Running,
    /// Installed, not running.
    Stopped,
    /// Not installed as a service.
    Missing,
    /// Cannot tell on this system.
    Unknown,
}

/// The moon-server executable shipped next to this app.
pub fn server_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "moon-server.exe" } else { "moon-server" };
    let p = std::env::current_exe().ok()?.parent()?.join(name);
    p.exists().then_some(p)
}

#[cfg(target_os = "macos")]
mod svc {
    use super::*;
    const LABEL: &str = "com.2moon.server";

    fn plist() -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
    }
    fn target() -> String {
        format!("gui/{}", unsafe { libc_getuid() })
    }
    extern "C" {
        #[link_name = "getuid"]
        fn libc_getuid() -> u32;
    }
    fn launchctl(args: &[&str]) -> Result<String> {
        let out = std::process::Command::new("launchctl").args(args).output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim().to_string() + &String::from_utf8_lossy(&out.stdout));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn state() -> ServiceState {
        if !plist().exists() {
            return ServiceState::Missing;
        }
        match launchctl(&["print", &format!("{}/{LABEL}", target())]) {
            Ok(s) if s.contains("state = running") => ServiceState::Running,
            Ok(_) | Err(_) => ServiceState::Stopped,
        }
    }

    /// Write the LaunchAgent (starts at login, restarts if it stops) and start it.
    pub fn install() -> Result<()> {
        let exe = server_exe().ok_or_else(|| anyhow::anyhow!("moon-server is not next to this app"))?;
        let logs = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Logs/2moon-server.log");
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--video</string><string>screen</string>
    <string>--audio</string><string>desktop</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{logs}</string>
  <key>StandardErrorPath</key><string>{logs}</string>
</dict>
</plist>
"#,
            exe = xml_escape(&exe.to_string_lossy()),
            logs = xml_escape(&logs.to_string_lossy())
        );
        if let Some(d) = plist().parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(plist(), xml)?;
        let _ = launchctl(&["bootout", &format!("{}/{LABEL}", target())]);
        launchctl(&["bootstrap", &target(), &plist().to_string_lossy()])?;
        Ok(())
    }

    pub fn start() -> Result<()> {
        if launchctl(&["kickstart", &format!("{}/{LABEL}", target())]).is_err() {
            launchctl(&["bootstrap", &target(), &plist().to_string_lossy()])?;
        }
        Ok(())
    }

    pub fn stop() -> Result<()> {
        launchctl(&["bootout", &format!("{}/{LABEL}", target())]).map(|_| ())
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
}

#[cfg(target_os = "linux")]
mod svc {
    use super::*;
    const UNIT: &str = "2moon-server";

    fn systemctl(args: &[&str]) -> Result<String> {
        let out = std::process::Command::new("systemctl").arg("--user").args(args).output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn unit_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
        base.join("systemd/user").join(format!("{UNIT}.service"))
    }

    pub fn state() -> ServiceState {
        match std::process::Command::new("systemctl").args(["--user", "is-active", UNIT]).output() {
            Ok(o) if o.status.success() => ServiceState::Running,
            Ok(_) if systemctl(&["cat", UNIT]).is_ok() => ServiceState::Stopped,
            Ok(_) => ServiceState::Missing,
            Err(_) => ServiceState::Unknown,
        }
    }

    /// Like scripts/install-service.sh: KWin capture on KDE Plasma (no dialog),
    /// the XDG ScreenCast portal elsewhere (GNOME, Hyprland, Sway...: asks once).
    pub fn install() -> Result<()> {
        let exe = server_exe().ok_or_else(|| anyhow::anyhow!("moon-server is not next to this app"))?;
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.to_ascii_uppercase().contains("KDE"));
        let video = if kde { "kwin" } else { "portal" };
        let wayland = std::env::var("WAYLAND_DISPLAY").map(|w| format!("Environment=WAYLAND_DISPLAY={w}\n")).unwrap_or_default();
        let unit = format!(
            "[Unit]\nDescription=2moon game streaming host\nAfter=graphical-session.target pipewire.service\nPartOf=graphical-session.target\n\n[Service]\nExecStart=\"{}\" --video {video} --audio desktop\n{wayland}Environment=RUST_LOG=info\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=graphical-session.target\n",
            exe.display()
        );
        if let Some(d) = unit_path().parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(unit_path(), unit)?;
        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", UNIT]).map(|_| ())
    }

    pub fn start() -> Result<()> {
        systemctl(&["start", UNIT]).map(|_| ())
    }

    pub fn stop() -> Result<()> {
        systemctl(&["stop", UNIT]).map(|_| ())
    }
}

#[cfg(windows)]
mod svc {
    use super::*;
    const NAME: &str = "2moon";

    fn sc(args: &[&str]) -> Result<String> {
        let out = std::process::Command::new("sc.exe").args(args).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            bail!("{}", text.trim());
        }
        Ok(text)
    }

    pub fn state() -> ServiceState {
        match sc(&["query", NAME]) {
            Ok(s) if s.contains("RUNNING") || s.contains("START_PENDING") => ServiceState::Running,
            Ok(_) => ServiceState::Stopped,
            Err(_) => ServiceState::Missing,
        }
    }

    /// The Windows host is installed by its own installer (service running as SYSTEM).
    pub fn install() -> Result<()> {
        bail!("installez l'hôte avec l'installateur 2moon pour Windows (2moon-…-windows-x86_64-setup.exe)")
    }

    pub fn start() -> Result<()> {
        sc(&["start", NAME]).map(|_| ()).context("démarrer le service demande les droits d'administrateur")
    }

    pub fn stop() -> Result<()> {
        sc(&["stop", NAME]).map(|_| ()).context("arrêter le service demande les droits d'administrateur")
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod svc {
    use super::*;
    pub fn state() -> ServiceState {
        ServiceState::Unknown
    }
    pub fn install() -> Result<()> {
        bail!("not supported on this system")
    }
    pub fn start() -> Result<()> {
        bail!("not supported on this system")
    }
    pub fn stop() -> Result<()> {
        bail!("not supported on this system")
    }
}

pub use svc::{install as install_service, start as start_service, state as service_state, stop as stop_service};
