//! HTTP/HTTPS client side of the GameStream API (serverinfo, pairing,
//! applist, launch/resume/cancel).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use moon_proto::crypto::{self, Identity};
use moon_proto::pairing::{ClientPairing, HashAlgo};
use moon_proto::xml;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tracing::debug;

/// Every official client sends this fixed unique id; hosts tell clients apart by certificate.
pub const UNIQUE_ID: &str = "0123456789ABCDEF";

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub hostname: String,
    pub uuid: String,
    pub app_version: String,
    pub https_port: u16,
    pub http_port: u16,
    pub paired: bool,
    pub current_game: u32,
    pub codec_support: u32,
    pub busy: bool,
    pub local_ip: String,
}

impl ServerInfo {
    pub fn version_major(&self) -> u32 {
        self.app_version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(7)
    }

    /// Sunshine-compatible hosts use a negative fourth version component.
    pub fn is_sunshine(&self) -> bool {
        self.app_version.split('.').nth(3).map(|v| v.starts_with('-')).unwrap_or(false)
    }

    fn parse(doc: &str) -> Result<Self> {
        let p = xml::parse(doc)?;
        let num = |k: &str| p.get(k).and_then(|v| v.parse::<u32>().ok());
        Ok(Self {
            hostname: p.get("hostname").unwrap_or("").to_string(),
            uuid: p.get("uniqueid").unwrap_or("").to_string(),
            app_version: p.require("appversion")?.to_string(),
            https_port: num("HttpsPort").unwrap_or(47984) as u16,
            http_port: num("ExternalPort").unwrap_or(47989) as u16,
            paired: p.get("PairStatus") == Some("1"),
            current_game: num("currentgame").unwrap_or(0),
            codec_support: num("ServerCodecModeSupport").unwrap_or(1),
            busy: p.get("state").map(|s| s.ends_with("_SERVER_BUSY")).unwrap_or(false),
            local_ip: p.get("LocalIP").unwrap_or("").to_string(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct LaunchParams {
    pub app_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rikey: [u8; 16],
    pub rikeyid: u32,
    pub audio_channels: u32,
}

pub struct GsClient {
    pub host: String,
    pub http_port: u16,
    pub https_port: u16,
    pub identity: Identity,
    pub server_cert: Option<Vec<u8>>,
}

impl GsClient {
    pub fn new(host: &str, http_port: u16, identity: Identity) -> Self {
        Self { host: host.to_string(), http_port, https_port: http_port.wrapping_sub(5), identity, server_cert: None }
    }

    fn query(path: &str, params: &[(&str, String)]) -> String {
        let uuid = hex::encode(crypto::random_bytes::<16>());
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("uniqueid", UNIQUE_ID).append_pair("uuid", &uuid);
        for (k, v) in params {
            q.append_pair(k, v);
        }
        format!("{path}?{}", q.finish())
    }

    fn host_header(&self, port: u16) -> String {
        if self.host.contains(':') {
            format!("[{}]:{port}", self.host)
        } else {
            format!("{}:{port}", self.host)
        }
    }

    async fn request<S>(io: S, host: String, path: String, timeout: Duration) -> Result<String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (_, body) = Self::request_raw(io, host, path, timeout).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn request_raw<S>(io: S, host: String, path: String, timeout: Duration) -> Result<(u16, Bytes)>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = hyper::Request::get(path).header("Host", host).body(Empty::<Bytes>::new())?;
        let fut = async {
            let res = sender.send_request(req).await?;
            let status = res.status().as_u16();
            let body = res.into_body().collect().await?.to_bytes();
            Ok::<_, anyhow::Error>((status, body))
        };
        tokio::time::timeout(timeout, fut).await.context("HTTP request timed out")?
    }

    pub async fn http_get(&self, path: &str, params: &[(&str, String)], timeout: Duration) -> Result<String> {
        let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((self.host.as_str(), self.http_port))).await??;
        let pq = Self::query(path, params);
        debug!("GET http {path}");
        Self::request(tcp, self.host_header(self.http_port), pq, timeout).await
    }

    pub async fn https_get(&self, path: &str, params: &[(&str, String)], timeout: Duration) -> Result<String> {
        let (_, body) = self.https_get_raw(path, params, timeout).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn https_get_raw(&self, path: &str, params: &[(&str, String)], timeout: Duration) -> Result<(u16, Bytes)> {
        let cfg = self.tls_config()?;
        let tcp = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect((self.host.as_str(), self.https_port))).await??;
        let name = ServerName::try_from(self.host.clone()).unwrap_or_else(|_| ServerName::try_from("localhost").unwrap().to_owned());
        let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(name, tcp).await.context("TLS handshake")?;
        debug!("GET https {path}");
        Self::request_raw(tls, self.host_header(self.https_port), Self::query(path, params), timeout).await
    }

    /// Box art of an app (PNG, as served by 2moon and Sunshine); `None` if the host has none.
    pub async fn app_asset(&self, app_id: u32) -> Result<Option<Bytes>> {
        let params = [("appid", app_id.to_string()), ("AssetType", "2".into()), ("AssetIdx", "0".into())];
        let (status, body) = self.https_get_raw("/appasset", &params, Duration::from_secs(10)).await?;
        Ok((status == 200 && !body.is_empty()).then_some(body))
    }

    fn tls_config(&self) -> Result<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Ok(rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert { pinned: self.server_cert.clone(), provider }))
            .with_client_auth_cert(
                vec![CertificateDer::from(self.identity.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.identity.key_pkcs8_der())),
            )?)
    }

    /// Query serverinfo over HTTPS when we have a pinned certificate (that is the only
    /// way to learn the pair status), falling back to HTTP.
    pub async fn server_info(&self) -> Result<ServerInfo> {
        if self.server_cert.is_some() {
            if let Ok(doc) = self.https_get("/serverinfo", &[], Duration::from_secs(5)).await {
                if let Ok(info) = ServerInfo::parse(&doc) {
                    return Ok(info);
                }
            }
        }
        let doc = self.http_get("/serverinfo", &[], Duration::from_secs(5)).await?;
        let mut info = ServerInfo::parse(&doc)?;
        info.paired = false;
        Ok(info)
    }

    /// Run the full PIN pairing exchange. On success the host certificate is pinned.
    pub async fn pair(&mut self, pin: &str, device_name: &str) -> Result<()> {
        let info = self.server_info().await?;
        self.https_port = info.https_port;
        let algo = HashAlgo::for_server_major(info.version_major());
        let mut p = ClientPairing::new(algo, pin);
        let long = Duration::from_secs(130);
        let short = Duration::from_secs(10);
        let base = |extra: Vec<(&'static str, String)>| {
            let mut v = vec![("devicename", device_name.to_string()), ("updateState", "1".to_string())];
            v.extend(extra);
            v
        };

        // 1. exchange certificates (the host waits for its operator to type the PIN)
        let doc = self
            .http_get("/pair", &base(vec![("phrase", "getservercert".into()), ("salt", hex::encode(p.salt)), ("clientcert", hex::encode(self.identity.cert_pem()))]), long)
            .await?;
        let r = xml::parse(&doc)?;
        if r.get("paired") != Some("1") {
            bail!("host refused pairing (already pairing with another device?)");
        }
        let plaincert = r.get("plaincert").filter(|s| !s.is_empty()).context("host sent no certificate (pairing already in progress?)")?;
        let server_cert = crypto::pem_decode_cert(&String::from_utf8(hex::decode(plaincert)?)?)?;
        p.set_server_cert(server_cert.clone());

        let fail = |this: &Self| {
            let this_host = this.host.clone();
            let port = this.http_port;
            let id = this.identity.clone();
            tokio::spawn(async move {
                let c = GsClient::new(&this_host, port, id);
                let _ = c.http_get("/unpair", &[], Duration::from_secs(3)).await;
            });
        };

        // 2. our challenge
        let doc = self.http_get("/pair", &base(vec![("clientchallenge", hex::encode(p.encrypted_client_challenge()))]), short).await?;
        let r = xml::parse(&doc)?;
        if r.get("paired") != Some("1") {
            fail(self);
            bail!("host rejected client challenge");
        }
        let resp = hex::decode(r.require("challengeresponse")?)?;
        let scr = p.on_challenge_response(&self.identity, &resp)?;

        // 3. answer the host challenge
        let doc = self.http_get("/pair", &base(vec![("serverchallengeresp", hex::encode(scr))]), short).await?;
        let r = xml::parse(&doc)?;
        if r.get("paired") != Some("1") {
            fail(self);
            bail!("host rejected challenge response");
        }
        let secret = hex::decode(r.require("pairingsecret")?)?;
        if let Err(e) = p.on_pairing_secret(&secret) {
            fail(self);
            bail!("host authentication failed (wrong PIN or MITM): {e}");
        }

        // 4. reveal our secret
        let doc = self.http_get("/pair", &base(vec![("clientpairingsecret", hex::encode(p.client_pairing_secret(&self.identity)))]), short).await?;
        let r = xml::parse(&doc)?;
        if r.get("paired") != Some("1") {
            fail(self);
            bail!("host rejected our pairing secret (wrong PIN?)");
        }

        // 5. confirm over the authenticated channel
        self.server_cert = Some(server_cert);
        let doc = self.https_get("/pair", &base(vec![("phrase", "pairchallenge".into())]), short).await?;
        if xml::parse(&doc)?.get("paired") != Some("1") {
            self.server_cert = None;
            bail!("pairing challenge over HTTPS failed");
        }
        Ok(())
    }

    pub async fn app_list(&self) -> Result<Vec<xml::App>> {
        let doc = self.https_get("/applist", &[], Duration::from_secs(10)).await?;
        Ok(xml::parse_apps(&doc)?)
    }

    /// Start (or resume) a session. Returns the RTSP session URL.
    pub async fn launch(&self, lp: &LaunchParams, resume: bool) -> Result<String> {
        // surroundAudioInfo = channel mask << 16 | channel count (stereo: 0x3 << 16 | 2)
        let mask: u32 = match lp.audio_channels {
            6 => 0x3f,
            8 => 0x63f,
            _ => 0x3,
        };
        let params = vec![
            ("appid", lp.app_id.to_string()),
            ("mode", format!("{}x{}x{}", lp.width, lp.height, lp.fps)),
            ("additionalStates", "1".into()),
            ("sops", "0".into()),
            ("rikey", hex::encode(lp.rikey)),
            ("rikeyid", (lp.rikeyid as i32).to_string()),
            ("localAudioPlayMode", "0".into()),
            ("surroundAudioInfo", ((mask << 16) | lp.audio_channels).to_string()),
            ("remoteControllersBitmap", "0".into()),
            // We support encrypted RTSP (rtspenc://).
            ("corever", "1".into()),
            ("gcmap", "0".into()),
        ];
        let path = if resume { "/resume" } else { "/launch" };
        let doc = self.https_get(path, &params, Duration::from_secs(60)).await?;
        let r = xml::parse(&doc)?;
        let ok_tag = if resume { "resume" } else { "gamesession" };
        if r.get(ok_tag).map(|v| v == "0").unwrap_or(true) {
            bail!("host refused to {}", if resume { "resume" } else { "launch" });
        }
        Ok(r.get("sessionUrl0").unwrap_or("").to_string())
    }

    pub async fn cancel(&self) -> Result<()> {
        let doc = self.https_get("/cancel", &[], Duration::from_secs(10)).await?;
        xml::parse(&doc)?;
        Ok(())
    }
}

/// Accept exactly the certificate pinned during pairing (or anything while not yet paired).
#[derive(Debug)]
struct PinnedCert {
    pinned: Option<Vec<u8>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(&self, end: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        match &self.pinned {
            Some(p) if p.as_slice() != end.as_ref() => Err(rustls::Error::General("server certificate does not match the paired one".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
