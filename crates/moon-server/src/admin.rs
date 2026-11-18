//! Web admin panel (HTTPS, cookie session) served on `http_port + 1`.
//!
//! Security model:
//! * TLS with the host certificate (self-signed, like the GameStream API).
//! * Username + password stored as salted PBKDF2-HMAC-SHA256.
//! * The first account can only be created with a one-time setup token that is
//!   printed in the server log, so nobody else on the LAN can claim the panel.
//! * Session cookie: random 256-bit token, HttpOnly, Secure, SameSite=Strict.
//! * Every POST must carry `X-2moon: 1` (a custom header cannot be sent
//!   cross-site without a CORS preflight, which we never allow) => no CSRF.
//! * Failed logins are slowed down and temporarily locked out.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use moon_proto::crypto;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::state::Host;

const PAGE: &str = include_str!("admin.html");

/// Static files of the admin page (same look as the client): fonts, moon, stars.
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("moon.webp", "image/webp", include_bytes!("../assets/moon.webp")),
    ("stars.webp", "image/webp", include_bytes!("../assets/stars.webp")),
    ("inter.ttf", "font/ttf", include_bytes!("../../moon-client/assets/fonts/Inter-Regular.ttf")),
    ("inter-semibold.ttf", "font/ttf", include_bytes!("../../moon-client/assets/fonts/Inter-SemiBold.ttf")),
    ("space-grotesk-bold.ttf", "font/ttf", include_bytes!("../../moon-client/assets/fonts/SpaceGrotesk-Bold.ttf")),
    ("jetbrains-mono.ttf", "font/ttf", include_bytes!("../../moon-client/assets/fonts/JetBrainsMono-Regular.ttf")),
    ("jetbrains-mono-bold.ttf", "font/ttf", include_bytes!("../../moon-client/assets/fonts/JetBrainsMono-Bold.ttf")),
];
const COOKIE: &str = "moon_admin";
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const PBKDF2_ROUNDS: u32 = 200_000;

#[derive(Serialize, Deserialize, Clone)]
struct Credentials {
    user: String,
    salt: String,
    hash: String,
    rounds: u32,
}

fn hash_password(password: &str, salt: &[u8], rounds: u32) -> Vec<u8> {
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, rounds, &mut out);
    out.to_vec()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct Admin {
    host: Arc<Host>,
    path: PathBuf,
    creds: Mutex<Option<Credentials>>,
    setup_token: Mutex<Option<String>>,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<(u32, Instant)>,
}

impl Admin {
    fn new(host: Arc<Host>) -> Arc<Self> {
        let path = host.cfg.state_dir.join("admin.json");
        let creds: Option<Credentials> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok());
        let setup_token = if creds.is_none() {
            let t = hex::encode(crypto::random_bytes::<6>());
            warn!("Admin panel not configured yet. Open it and use this one-time setup token: {t}");
            Some(t)
        } else {
            None
        };
        Arc::new(Self { host, path, creds: Mutex::new(creds), setup_token: Mutex::new(setup_token), sessions: Mutex::new(HashMap::new()), failures: Mutex::new((0, Instant::now())) })
    }

    fn save_creds(&self, user: &str, password: &str) -> Result<()> {
        let salt: [u8; 16] = crypto::random_bytes();
        let c = Credentials { user: user.to_string(), salt: hex::encode(salt), hash: hex::encode(hash_password(password, &salt, PBKDF2_ROUNDS)), rounds: PBKDF2_ROUNDS };
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        let tmp = self.path.with_extension("tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&c)?.as_bytes())?;
        }
        std::fs::rename(&tmp, &self.path)?;
        *self.creds.lock().unwrap() = Some(c);
        *self.setup_token.lock().unwrap() = None;
        Ok(())
    }

    fn check_password(&self, user: &str, password: &str) -> bool {
        let Some(c) = self.creds.lock().unwrap().clone() else { return false };
        let (Ok(salt), Ok(hash)) = (hex::decode(&c.salt), hex::decode(&c.hash)) else { return false };
        let user_ok = ct_eq(c.user.as_bytes(), user.as_bytes());
        let pass_ok = ct_eq(&hash_password(password, &salt, c.rounds), &hash);
        user_ok && pass_ok
    }

    fn new_session(&self) -> String {
        let t = hex::encode(crypto::random_bytes::<32>());
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, exp| *exp > Instant::now());
        s.insert(t.clone(), Instant::now() + SESSION_TTL);
        t
    }

    fn session_of(&self, req: &Request<Incoming>) -> Option<String> {
        let cookies = req.headers().get_all(hyper::header::COOKIE);
        for v in cookies.iter().filter_map(|v| v.to_str().ok()) {
            for part in v.split(';') {
                if let Some(tok) = part.trim().strip_prefix(&format!("{COOKIE}=")) {
                    let s = self.sessions.lock().unwrap();
                    if s.get(tok).map(|e| *e > Instant::now()).unwrap_or(false) {
                        return Some(tok.to_string());
                    }
                }
            }
        }
        None
    }

    /// Returns false if login attempts are currently locked out.
    async fn throttle_failure(&self) {
        let wait = {
            let mut f = self.failures.lock().unwrap();
            if f.1.elapsed() > Duration::from_secs(300) {
                *f = (0, Instant::now());
            }
            f.0 += 1;
            f.0
        };
        tokio::time::sleep(Duration::from_secs(wait.min(10) as u64)).await;
    }

    fn locked_out(&self) -> bool {
        let f = self.failures.lock().unwrap();
        f.0 >= 10 && f.1.elapsed() < Duration::from_secs(300)
    }
}

pub async fn run(host: Arc<Host>, port: u16) -> Result<()> {
    let admin = Admin::new(host.clone());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(host.identity.cert_der.clone())], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(host.identity.key_pkcs8_der())))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let l = crate::state::listen_any(port).await.context("bind admin port")?;
    info!("admin panel on https://<this-host>:{port}/");
    loop {
        let (sock, peer) = l.accept().await?;
        let (admin, acceptor) = (admin.clone(), acceptor.clone());
        tokio::spawn(async move {
            let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await else { return };
            let svc = service_fn(move |req| {
                let admin = admin.clone();
                async move { Ok::<_, Infallible>(handle(admin, req, peer).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
        });
    }
}

fn resp(status: StatusCode, ctype: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", ctype)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY")
        .header("Referrer-Policy", "no-referrer")
        .header("Content-Security-Policy", "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; frame-ancestors 'none'")
        .body(Full::new(body.into()))
        .unwrap()
}

fn js(status: StatusCode, v: Value) -> Response<Full<Bytes>> {
    resp(status, "application/json", v.to_string())
}

fn with_cookie(mut r: Response<Full<Bytes>>, token: &str, max_age: u64) -> Response<Full<Bytes>> {
    r.headers_mut().insert(hyper::header::SET_COOKIE, format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age}").parse().unwrap());
    r
}

async fn body_json(req: Request<Incoming>) -> Option<Value> {
    let b = Limited::new(req.into_body(), 64 * 1024).collect().await.ok()?.to_bytes();
    serde_json::from_slice(&b).ok()
}

/// `?id=N` from the query string.
fn query_id(req: &Request<Incoming>) -> Option<u32> {
    url::form_urlencoded::parse(req.uri().query()?.as_bytes()).find(|(k, _)| k == "id").and_then(|(_, v)| v.parse().ok())
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

async fn handle(admin: Arc<Admin>, req: Request<Incoming>, peer: SocketAddr) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if method == Method::GET && (path == "/" || path == "/index.html") {
        return resp(StatusCode::OK, "text/html; charset=utf-8", PAGE);
    }
    if let Some(name) = path.strip_prefix("/assets/").filter(|_| method == Method::GET) {
        return match ASSETS.iter().find(|a| a.0 == name) {
            Some((_, ctype, bytes)) => {
                let mut r = resp(StatusCode::OK, ctype, Bytes::from_static(bytes));
                r.headers_mut().insert(hyper::header::CACHE_CONTROL, "public, max-age=86400".parse().unwrap());
                r
            }
            None => resp(StatusCode::NOT_FOUND, "text/plain", "not found"),
        };
    }
    if !path.starts_with("/api/") {
        return resp(StatusCode::NOT_FOUND, "text/plain", "not found");
    }
    if method == Method::POST && req.headers().get("x-2moon").map(|v| v.as_bytes()) != Some(b"1") {
        return js(StatusCode::FORBIDDEN, json!({"error": "missing X-2moon header"}));
    }
    let session = admin.session_of(&req);
    let configured = admin.creds.lock().unwrap().is_some();

    match (method.clone(), path.as_str()) {
        (Method::GET, "/api/state") => return js(StatusCode::OK, json!({"setup_required": !configured, "logged_in": session.is_some(), "language": admin.host.settings.get().language})),
        (Method::POST, "/api/setup") => {
            if configured {
                return js(StatusCode::CONFLICT, json!({"error": "déjà configuré"}));
            }
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let token_ok = admin.setup_token.lock().unwrap().as_deref().map(|t| ct_eq(t.as_bytes(), s(&b, "token").trim().as_bytes())).unwrap_or(false);
            if !token_ok {
                admin.throttle_failure().await;
                return js(StatusCode::UNAUTHORIZED, json!({"error": "jeton de configuration incorrect (voir le journal du serveur)"}));
            }
            let (user, pass) = (s(&b, "user"), s(&b, "password"));
            if user.trim().is_empty() || pass.chars().count() < 8 {
                return js(StatusCode::BAD_REQUEST, json!({"error": "mot de passe : 8 caractères minimum"}));
            }
            if let Err(e) = admin.save_creds(user.trim(), &pass) {
                return js(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
            }
            info!("admin account '{}' created from {}", user.trim(), peer.ip());
            let t = admin.new_session();
            return with_cookie(js(StatusCode::OK, json!({"ok": true})), &t, SESSION_TTL.as_secs());
        }
        (Method::POST, "/api/login") => {
            if admin.locked_out() {
                return js(StatusCode::TOO_MANY_REQUESTS, json!({"error": "trop d'échecs, réessayez dans 5 minutes"}));
            }
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            if admin.check_password(&s(&b, "user"), &s(&b, "password")) {
                *admin.failures.lock().unwrap() = (0, Instant::now());
                let t = admin.new_session();
                info!("admin login from {}", peer.ip());
                return with_cookie(js(StatusCode::OK, json!({"ok": true})), &t, SESSION_TTL.as_secs());
            }
            warn!("failed admin login from {}", peer.ip());
            admin.throttle_failure().await;
            return js(StatusCode::UNAUTHORIZED, json!({"error": "identifiants incorrects"}));
        }
        _ => {}
    }

    let Some(token) = session else { return js(StatusCode::UNAUTHORIZED, json!({"error": "non connecté"})) };
    let host = &admin.host;
    match (method, path.as_str()) {
        (Method::POST, "/api/logout") => {
            admin.sessions.lock().unwrap().remove(&token);
            with_cookie(js(StatusCode::OK, json!({"ok": true})), "", 0)
        }
        (Method::GET, "/api/status") => {
            let session = host.current_session();
            let live = host.live.lock().unwrap().clone();
            let cfg = &host.cfg;
            js(
                StatusCode::OK,
                json!({
                    "hostname": host.hostname(),
                    "version": env!("CARGO_PKG_VERSION"),
                    "uptime_s": host.started_at.elapsed().as_secs(),
                    "ports": {"http": cfg.http_port, "https": cfg.https_port, "rtsp": cfg.rtsp_port},
                    "video_source": format!("{:?}", cfg.video_source),
                    "audio_source": format!("{:?}", cfg.audio_source),
                    "encoder": cfg.encoder, "nvenc_preset": host.nvenc_preset(),
                    "codecs": {"h264": true, "hevc": cfg.hevc, "av1": cfg.av1},
                    "session": session.map(|s| json!({"app_id": s.app_id, "client": s.client_ip.to_string(), "width": s.width, "height": s.height, "fps": s.fps})),
                    "live": live,
                    "pending": host.pending_requests.lock().unwrap().iter().map(|(n, a)| json!({"name": n, "address": a})).collect::<Vec<_>>(),
                    "input_events": host.input_events.load(std::sync::atomic::Ordering::Relaxed),
                }),
            )
        }
        (Method::GET, "/api/clients") => {
            let list: Vec<Value> = host.paired_clients().iter().map(|c| json!({"name": c.name, "fingerprint": Host::fingerprint(&c.cert_der)})).collect();
            js(StatusCode::OK, json!(list))
        }
        (Method::POST, "/api/clients/unpair") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            match host.unpair_fingerprint(&s(&b, "fingerprint")) {
                Ok(true) => {
                    info!("client unpaired from the admin panel");
                    js(StatusCode::OK, json!({"ok": true}))
                }
                Ok(false) => js(StatusCode::NOT_FOUND, json!({"error": "appareil inconnu"})),
                Err(e) => js(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()})),
            }
        }
        (Method::GET, "/api/apps") => {
            let current = host.current_session().map(|s| s.app_id);
            let list: Vec<Value> = host
                .apps
                .list()
                .into_iter()
                .map(|a| json!({"id": a.id, "title": a.title, "cmd": a.cmd, "hdr": a.hdr, "cover": host.apps.has_cover(a.id), "running": current == Some(a.id)}))
                .collect();
            js(StatusCode::OK, json!(list))
        }
        (Method::POST, "/api/apps/save") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let id = b.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let hdr = b.get("hdr").and_then(|v| v.as_bool()).unwrap_or(false);
            match host.apps.upsert(id, &s(&b, "title"), &s(&b, "cmd"), hdr) {
                Ok(id) => {
                    info!("app {id} saved from the admin panel");
                    js(StatusCode::OK, json!({"ok": true, "id": id}))
                }
                Err(e) => js(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
            }
        }
        (Method::POST, "/api/apps/delete") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let id = b.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            match host.apps.remove(id) {
                Ok(()) => js(StatusCode::OK, json!({"ok": true})),
                Err(e) => js(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
            }
        }
        (Method::POST, "/api/apps/move") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let id = b.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let dir = b.get("dir").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            match host.apps.shift(id, dir) {
                Ok(()) => js(StatusCode::OK, json!({"ok": true})),
                Err(e) => js(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
            }
        }
        (Method::GET, "/api/apps/cover") => match query_id(&req).and_then(|id| host.apps.cover(id)) {
            Some(png) => resp(StatusCode::OK, "image/png", png),
            None => resp(StatusCode::NOT_FOUND, "text/plain", "no cover"),
        },
        (Method::POST, "/api/apps/cover") => {
            let Some(id) = query_id(&req) else { return js(StatusCode::BAD_REQUEST, json!({"error": "id manquant"})) };
            let Ok(b) = Limited::new(req.into_body(), crate::apps::MAX_COVER_BYTES).collect().await.map(|b| b.to_bytes()) else {
                return js(StatusCode::PAYLOAD_TOO_LARGE, json!({"error": "image trop lourde (8 Mo maximum)"}));
            };
            match host.apps.set_cover(id, (!b.is_empty()).then_some(&b[..])) {
                Ok(()) => js(StatusCode::OK, json!({"ok": true})),
                Err(e) => js(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
            }
        }
        (Method::GET, "/api/settings") => {
            let v = host.settings.get();
            let (dl, dv) = crate::text::detect_layout();
            let keyboards: Vec<Value> = crate::settings::KEYBOARDS.iter().map(|k| json!({"id": k.0, "label": k.3})).collect();
            js(
                StatusCode::OK,
                json!({
                    "language": v.language, "keyboard": v.keyboard, "keyboard_detected": if dv.is_empty() { dl } else { format!("{dl} ({dv})") }, "keyboards": keyboards,
                    "host_name": v.host_name, "host_name_default": host.cfg.hostname,
                    "nvenc_preset": v.nvenc_preset, "nvenc_preset_default": host.cfg.nvenc_preset, "encoder": host.cfg.encoder,
                    "auto_quit_min": v.auto_quit_min, "auto_quit_choices": crate::settings::AUTO_QUIT,
                    "mdns": v.mdns, "mdns_available": host.mdns_available.load(std::sync::atomic::Ordering::Relaxed),
                }),
            )
        }
        (Method::POST, "/api/settings") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            match host.settings.update(&b) {
                Ok(_) => {
                    info!("settings changed from the admin panel: {b}");
                    js(StatusCode::OK, json!({"ok": true}))
                }
                Err(e) => js(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
            }
        }
        (Method::POST, "/api/pin") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let pin = s(&b, "pin");
            if pin.len() != 4 || !pin.chars().all(|c| c.is_ascii_digit()) {
                return js(StatusCode::BAD_REQUEST, json!({"error": "le PIN fait 4 chiffres"}));
            }
            let n = host.submit_pin(&pin);
            js(StatusCode::OK, json!({"delivered": n}))
        }
        (Method::POST, "/api/session/stop") => {
            host.set_session(None);
            host.apps.stop();
            info!("session stopped from the admin panel");
            js(StatusCode::OK, json!({"ok": true}))
        }
        (Method::POST, "/api/session/idr") => {
            host.request_idr();
            js(StatusCode::OK, json!({"ok": true}))
        }
        (Method::GET, "/api/logs") => {
            let n = req.uri().query().and_then(|q| q.strip_prefix("n=")).and_then(|n| n.parse().ok()).unwrap_or(200usize).min(500);
            js(StatusCode::OK, json!(crate::logbuf::recent(n)))
        }
        (Method::POST, "/api/password") => {
            let Some(b) = body_json(req).await else { return js(StatusCode::BAD_REQUEST, json!({"error": "requête invalide"})) };
            let user = admin.creds.lock().unwrap().as_ref().map(|c| c.user.clone()).unwrap_or_default();
            if !admin.check_password(&user, &s(&b, "current")) {
                admin.throttle_failure().await;
                return js(StatusCode::UNAUTHORIZED, json!({"error": "mot de passe actuel incorrect"}));
            }
            let new = s(&b, "new");
            if new.chars().count() < 8 {
                return js(StatusCode::BAD_REQUEST, json!({"error": "8 caractères minimum"}));
            }
            if let Err(e) = admin.save_creds(&user, &new) {
                return js(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e.to_string()}));
            }
            // Invalidate every other session.
            admin.sessions.lock().unwrap().retain(|t, _| *t == token);
            js(StatusCode::OK, json!({"ok": true}))
        }
        _ => js(StatusCode::NOT_FOUND, json!({"error": "not found"})),
    }
}
