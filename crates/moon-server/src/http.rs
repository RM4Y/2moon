//! GameStream HTTP (47989) and HTTPS (47984) API: serverinfo, pairing,
//! applist, launch/resume/cancel.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use moon_proto::crypto;
use moon_proto::pairing::ServerPairing;
use moon_proto::xml;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use tracing::{debug, info, warn};

use crate::state::{Host, LaunchSession, PendingPair};

/// Pretend to be a recent GFE/Sunshine generation. The negative 4th component
/// marks a Sunshine-compatible host, which enables the extended protocol.
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";

pub async fn run(host: Arc<Host>) -> Result<()> {
    let http = crate::state::listen_any(host.cfg.http_port).await.context("bind http")?;
    let https = crate::state::listen_any(host.cfg.https_port).await.context("bind https")?;
    info!("HTTP on :{}, HTTPS on :{}", host.cfg.http_port, host.cfg.https_port);
    let tls = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config(&host)?));

    let h2 = host.clone();
    let plain = tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = http.accept().await else { continue };
            let local = sock.local_addr().ok();
            let host = h2.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let host = host.clone();
                    async move { Ok::<_, Infallible>(handle(host, req, Ctx { peer, local, https: false, cert: None, host_name: None }).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(sock), svc).await;
            });
        }
    });
    let secure = tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = https.accept().await else { continue };
            let local = sock.local_addr().ok();
            let host = host.clone();
            let tls = tls.clone();
            tokio::spawn(async move {
                let stream = match tokio::time::timeout(Duration::from_secs(10), tls.accept(sock)).await {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        debug!("TLS handshake with {peer} failed: {e}");
                        return;
                    }
                    Err(_) => return,
                };
                let cert = stream.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| c.to_vec());
                let svc = service_fn(move |req| {
                    let host = host.clone();
                    let cert = cert.clone();
                    async move { Ok::<_, Infallible>(handle(host, req, Ctx { peer, local, https: true, cert, host_name: None }).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
            });
        }
    });
    tokio::select! {
        r = plain => r?,
        r = secure => r?,
    }
    Ok(())
}

fn tls_config(host: &Host) -> Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert(provider)))
        .with_single_cert(
            vec![CertificateDer::from(host.identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(host.identity.key_pkcs8_der())),
        )?;
    Ok(cfg)
}

/// GameStream clients authenticate with self-signed certificates that we pinned
/// during pairing, so the TLS layer accepts any certificate and the HTTP layer
/// checks it against the paired list.
#[derive(Debug)]
struct AcceptAnyClientCert(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: UnixTime) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn client_auth_mandatory(&self) -> bool {
        false
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

struct Ctx {
    peer: SocketAddr,
    local: Option<SocketAddr>,
    https: bool,
    cert: Option<Vec<u8>>,
    /// Address the client used to reach us (Host header, without the port).
    host_name: Option<String>,
}

/// The host part of a Host header ("moon.example.org:47984", "[fd00::1]:47984",
/// "192.168.1.85"), if it looks like a name or an address.
fn host_header_name(v: &str) -> Option<String> {
    let h = if v.starts_with('[') {
        &v[..=v.find(']')?]
    } else {
        match v.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => v,
        }
    };
    let ok = !h.is_empty() && h.len() <= 255 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b));
    ok.then(|| h.to_string())
}

fn reply(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/xml")
        .header("Connection", "close")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn ok(x: xml::Response) -> Response<Full<Bytes>> {
    reply(StatusCode::OK, x.build())
}

async fn handle(host: Arc<Host>, req: Request<Incoming>, mut ctx: Ctx) -> Response<Full<Bytes>> {
    ctx.host_name = req.headers().get(hyper::header::HOST).and_then(|v| v.to_str().ok()).and_then(host_header_name);
    let path = req.uri().path().to_string();
    let q: HashMap<String, String> = url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned().collect();
    debug!("{} {} {}{}", if ctx.https { "HTTPS" } else { "HTTP" }, ctx.peer, path, if q.is_empty() { "" } else { " (query)" });
    let paired = ctx.cert.as_deref().map(|c| host.is_paired(c)).unwrap_or(false);

    match path.as_str() {
        "/serverinfo" => ok(server_info(&host, &ctx, paired)),
        "/pair" => match pair(&host, &q, &ctx).await {
            Ok(x) => ok(x),
            Err(e) => {
                warn!("pairing failed: {e:#}");
                if let Some(id) = q.get("uniqueid") {
                    host.pending.lock().unwrap().remove(id);
                }
                ok(xml::Response::ok().field("paired", 0))
            }
        },
        "/pin" if !ctx.https && ctx.peer.ip().is_loopback() => {
            let n = host.submit_pin(q.get("pin").map(|s| s.as_str()).unwrap_or(""));
            reply(StatusCode::OK, format!("{n} pairing request(s) received the PIN\n"))
        }
        _ if !ctx.https => reply(StatusCode::NOT_FOUND, xml::Response::error(404, "Not found").build()),
        _ if !paired => reply(StatusCode::UNAUTHORIZED, xml::Response::error(401, "The client is not authorized. Certificate verification failed.").build()),
        "/applist" => ok(xml::Response::ok().raw(&host.apps.xml())),
        "/launch" | "/resume" => match launch(&host, &q, &ctx, path == "/resume") {
            Ok(x) => ok(x),
            Err(e) => {
                warn!("launch failed: {e:#}");
                ok(xml::Response::error(400, &format!("{e}")).field("gamesession", 0))
            }
        },
        "/cancel" => {
            host.set_session(None);
            host.apps.stop();
            info!("session cancelled by client");
            ok(xml::Response::ok().field("cancel", 1))
        }
        "/unpair" => {
            if let Some(c) = &ctx.cert {
                let _ = host.remove_paired(c);
            }
            ok(xml::Response::ok())
        }
        "/appasset" => match q.get("appid").and_then(|a| a.parse().ok()).and_then(|id| host.apps.cover(id)) {
            Some(png) => Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "image/png")
                .header("Connection", "close")
                .body(Full::new(Bytes::from(png)))
                .unwrap(),
            None => reply(StatusCode::NOT_FOUND, String::new()),
        },
        _ => reply(StatusCode::NOT_FOUND, xml::Response::error(404, "Not found").build()),
    }
}

fn server_info(host: &Host, ctx: &Ctx, paired: bool) -> xml::Response {
    let session = host.current_session();
    let local_ip = ctx.local.map(|a| canonical_ip(a.ip()).to_string()).unwrap_or_default();
    xml::Response::ok()
        .field("hostname", host.hostname())
        .field("appversion", APP_VERSION)
        .field("GfeVersion", GFE_VERSION)
        .field("uniqueid", &host.uuid)
        .field("HttpsPort", host.cfg.https_port)
        .field("ExternalPort", host.cfg.http_port)
        .field("MaxLumaPixelsHEVC", if host.cfg.hevc { 1869449984u64 } else { 0 })
        .field("mac", "00:00:00:00:00:00")
        .field("LocalIP", local_ip)
        // SCM flags: H.264 0x1, HEVC 0x100, AV1 main8 0x10000
        .field(
            "ServerCodecModeSupport",
            // H.264, HEVC, AV1 main 8-bit, then the Sunshine 4:4:4 extension bits.
            0x0001 | if host.cfg.hevc { 0x100 } else { 0 } | if host.cfg.av1 { 0x10000 } else { 0 } | if host.cfg.yuv444_h264 { 0x40000 } else { 0 } | if host.cfg.yuv444_hevc { 0x80000 } else { 0 }
                | if host.cfg.hdr10_hevc { 0x200 } else { 0 } | if host.cfg.hdr10_av1 { 0x20000 } else { 0 },
        )
        .raw(
            "<SupportedDisplayMode><DisplayMode><Width>1920</Width><Height>1080</Height><RefreshRate>60</RefreshRate></DisplayMode>\
             <DisplayMode><Width>1280</Width><Height>720</Height><RefreshRate>60</RefreshRate></DisplayMode></SupportedDisplayMode>",
        )
        .field("PairStatus", (ctx.https && paired) as u8)
        .field("currentgame", session.as_ref().map(|s| s.app_id).unwrap_or(0))
        .field("state", if session.is_some() { "SUNSHINE_SERVER_BUSY" } else { "SUNSHINE_SERVER_FREE" })
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

fn hex_param(q: &HashMap<String, String>, k: &str) -> Result<Vec<u8>> {
    hex::decode(q.get(k).with_context(|| format!("missing {k}"))?).with_context(|| format!("bad hex in {k}"))
}

async fn pair(host: &Arc<Host>, q: &HashMap<String, String>, ctx: &Ctx) -> Result<xml::Response> {
    let uid = q.get("uniqueid").cloned().unwrap_or_default();

    if q.get("phrase").map(|s| s.as_str()) == Some("getservercert") {
        let salt: [u8; 16] = hex_param(q, "salt")?.try_into().map_err(|_| anyhow::anyhow!("salt must be 16 bytes"))?;
        let pem = String::from_utf8(hex_param(q, "clientcert")?)?;
        let cert = crypto::pem_decode_cert(&pem)?;
        let name = q.get("devicename").cloned().unwrap_or_else(|| "unknown".into());
        let pin = match &host.cfg.preset_pin {
            Some(p) => p.clone(),
            None => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                host.pin_waiters.lock().unwrap().push(tx);
                host.pending_requests.lock().unwrap().push((name.clone(), ctx.peer.ip().to_string()));
                info!(
                    "Pairing request from '{name}' ({}). Type the PIN shown on the client in this console, or run: curl 'http://127.0.0.1:{}/pin?pin=XXXX'",
                    ctx.peer.ip(),
                    host.cfg.http_port
                );
                let r = tokio::time::timeout(Duration::from_secs(120), rx).await;
                host.pending_requests.lock().unwrap().retain(|(n, _)| n != &name);
                r.context("timed out waiting for PIN")??
            }
        };
        host.pending.lock().unwrap().insert(uid, PendingPair { pairing: ServerPairing::new(&salt, &pin, cert), device_name: name });
        return Ok(xml::Response::ok().field("paired", 1).field("plaincert", hex::encode(host.identity.cert_pem())));
    }

    if q.get("phrase").map(|s| s.as_str()) == Some("pairchallenge") {
        let paired = ctx.https && ctx.cert.as_deref().map(|c| host.is_paired(c)).unwrap_or(false);
        return Ok(xml::Response::ok().field("paired", paired as u8));
    }

    let mut pending = host.pending.lock().unwrap();
    let p = pending.get_mut(&uid).context("no pairing in progress for this client")?;
    if q.contains_key("clientchallenge") {
        let r = p.pairing.on_client_challenge(&host.identity, &hex_param(q, "clientchallenge")?)?;
        Ok(xml::Response::ok().field("paired", 1).field("challengeresponse", hex::encode_upper(r)))
    } else if q.contains_key("serverchallengeresp") {
        let r = p.pairing.on_server_challenge_resp(&host.identity, &hex_param(q, "serverchallengeresp")?)?;
        Ok(xml::Response::ok().field("paired", 1).field("pairingsecret", hex::encode_upper(r)))
    } else if q.contains_key("clientpairingsecret") {
        let p = pending.remove(&uid).unwrap();
        p.pairing.on_client_pairing_secret(&hex_param(q, "clientpairingsecret")?)?;
        host.add_paired(p.device_name.clone(), p.pairing.client_cert_der.clone())?;
        info!("Paired with '{}'", p.device_name);
        Ok(xml::Response::ok().field("paired", 1))
    } else {
        anyhow::bail!("unknown pairing phrase")
    }
}

fn launch(host: &Host, q: &HashMap<String, String>, ctx: &Ctx, resume: bool) -> Result<xml::Response> {
    let existing = host.current_session();
    if resume && existing.is_none() {
        anyhow::bail!("no session to resume");
    }
    let app_id: u32 = match q.get("appid") {
        Some(a) => a.parse()?,
        None => existing.as_ref().map(|s| s.app_id).unwrap_or(1),
    };
    if host.apps.get(app_id).is_none() {
        anyhow::bail!("unknown app {app_id}");
    }
    let (mut width, mut height, mut fps) = (1920, 1080, 60);
    if let Some(mode) = q.get("mode") {
        let parts: Vec<u32> = mode.split('x').filter_map(|p| p.parse().ok()).collect();
        if parts.len() == 3 {
            (width, height, fps) = (parts[0], parts[1], parts[2]);
        }
    }
    let rikey: [u8; 16] = hex_param(q, "rikey")?.try_into().map_err(|_| anyhow::anyhow!("rikey must be 16 bytes"))?;
    let rikeyid: i64 = q.get("rikeyid").context("missing rikeyid")?.parse()?;
    let session = LaunchSession {
        app_id,
        width,
        height,
        fps,
        rikey,
        rikeyid: rikeyid as u32,
        client_ip: canonical_ip(ctx.peer.ip()),
        ss_ping_payload: rand_alnum16(),
        control_connect_data: rand::random(),
    };
    info!("{} app {app_id} at {width}x{height}@{fps} for {}", if resume { "Resuming" } else { "Launching" }, session.client_ip);
    host.set_session(Some(session));
    if !resume {
        host.apps.start(app_id);
    }
    let local = ctx.local.map(|a| canonical_ip(a.ip())).unwrap_or(IpAddr::from([127, 0, 0, 1]));
    // Clients announcing `corever` support encrypted RTSP; use it unless disabled.
    let scheme = if q.contains_key("corever") && std::env::var_os("MOON_NO_RTSPENC").is_none() { "rtspenc" } else { "rtsp" };
    // The address the client reached us at (a host behind NAT is known by its public
    // name or IP, not by `local`): 2moon clients up to 0.5.1 connect RTSP to this host;
    // Moonlight only reads the port.
    let url = match (&ctx.host_name, local) {
        (Some(h), _) => format!("{scheme}://{h}:{}", host.cfg.rtsp_port),
        (None, IpAddr::V4(v4)) => format!("{scheme}://{v4}:{}", host.cfg.rtsp_port),
        (None, IpAddr::V6(v6)) => format!("{scheme}://[{v6}]:{}", host.cfg.rtsp_port),
    };
    let r = xml::Response::ok();
    Ok(if resume { r.field("resume", 1) } else { r.field("gamesession", 1) }.field("sessionUrl0", url))
}

fn rand_alnum16() -> [u8; 16] {
    const CH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let r: [u8; 16] = crypto::random_bytes();
    r.map(|b| CH[b as usize % CH.len()])
}

#[cfg(test)]
mod host_header_tests {
    use super::host_header_name;

    #[test]
    fn host_header() {
        assert_eq!(host_header_name("moon.example.org:47984").as_deref(), Some("moon.example.org"));
        assert_eq!(host_header_name("192.168.1.85").as_deref(), Some("192.168.1.85"));
        assert_eq!(host_header_name("[fd00::1]:47984").as_deref(), Some("[fd00::1]"));
        assert_eq!(host_header_name("fd00::1").as_deref(), Some("fd00::1"));
        assert_eq!(host_header_name("a<b>:1"), None);
        assert_eq!(host_header_name(""), None);
    }
}
