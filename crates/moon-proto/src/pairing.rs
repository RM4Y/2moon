//! GameStream PIN pairing, implemented as two pure state machines
//! (no I/O) so that both sides can be unit tested against each other.
//!
//! Flow (all over plain HTTP `/pair`, the last step over HTTPS):
//! 1. `phrase=getservercert&salt=..&clientcert=..`  -> server cert
//! 2. `clientchallenge=E(k, cc)`                    -> E(k, H(cc|srv_sig|srv_secret) | sc)
//! 3. `serverchallengeresp=E(k, H(sc|cli_sig|cli_secret))` -> srv_secret | sign(srv_secret)
//! 4. `clientpairingsecret=cli_secret | sign(cli_secret)` -> paired=1
//! 5. (HTTPS) `phrase=pairchallenge`                 -> paired=1
//!
//! where `k = H(salt | pin)[..16]` and `H` is SHA-256 for generation 7+ hosts
//! (SHA-1 for very old hosts).

use crate::crypto::{self, CryptoError, Identity};
use sha1::Digest as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha1,
    Sha256,
}

impl HashAlgo {
    /// Generation 7+ hosts use SHA-256.
    pub fn for_server_major(major: u32) -> Self {
        if major >= 7 {
            HashAlgo::Sha256
        } else {
            HashAlgo::Sha1
        }
    }

    pub fn len(self) -> usize {
        match self {
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha256 => 32,
        }
    }

    pub fn hash(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            HashAlgo::Sha256 => crypto::sha256(parts).to_vec(),
            HashAlgo::Sha1 => {
                let mut h = sha1::Sha1::new();
                for p in parts {
                    h.update(p);
                }
                h.finalize().to_vec()
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("pairing step out of order")]
    OutOfOrder,
    #[error("malformed pairing message")]
    Malformed,
    #[error("PIN mismatch or tampered exchange")]
    PinMismatch,
    #[error("peer signature invalid")]
    BadSignature,
}

pub type Result<T> = std::result::Result<T, PairError>;

pub fn derive_key(algo: HashAlgo, salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let h = algo.hash(&[salt, pin.as_bytes()]);
    h[..16].try_into().unwrap()
}

fn pad16(mut v: Vec<u8>) -> Vec<u8> {
    let r = v.len() % 16;
    if r != 0 {
        v.resize(v.len() + 16 - r, 0);
    }
    v
}

/// Host side of a pairing attempt with one client.
pub struct ServerPairing {
    pub client_cert_der: Vec<u8>,
    key: [u8; 16],
    server_challenge: Option<[u8; 16]>,
    server_secret: [u8; 16],
    client_hash: Option<Vec<u8>>,
}

impl ServerPairing {
    /// Step 1: the client sent its salt and certificate; the PIN has been entered on the host.
    pub fn new(salt: &[u8; 16], pin: &str, client_cert_der: Vec<u8>) -> Self {
        Self {
            client_cert_der,
            key: derive_key(HashAlgo::Sha256, salt, pin),
            server_challenge: None,
            server_secret: crypto::random_bytes(),
            client_hash: None,
        }
    }

    /// Step 2: answer the client's encrypted challenge.
    pub fn on_client_challenge(&mut self, host: &Identity, enc_challenge: &[u8]) -> Result<Vec<u8>> {
        let challenge = crypto::aes_ecb_decrypt(&self.key, enc_challenge)?;
        if challenge.len() < 16 {
            return Err(PairError::Malformed);
        }
        let hash = HashAlgo::Sha256.hash(&[&challenge[..16], &host.cert_signature(), &self.server_secret]);
        let sc: [u8; 16] = crypto::random_bytes();
        self.server_challenge = Some(sc);
        let mut plain = hash;
        plain.extend_from_slice(&sc);
        Ok(crypto::aes_ecb_encrypt(&self.key, &pad16(plain)))
    }

    /// Step 3: store the client's hash and reveal our signed secret.
    pub fn on_server_challenge_resp(&mut self, host: &Identity, enc: &[u8]) -> Result<Vec<u8>> {
        if self.server_challenge.is_none() {
            return Err(PairError::OutOfOrder);
        }
        let dec = crypto::aes_ecb_decrypt(&self.key, enc)?;
        if dec.len() < 32 {
            return Err(PairError::Malformed);
        }
        self.client_hash = Some(dec[..32].to_vec());
        let mut out = self.server_secret.to_vec();
        out.extend_from_slice(&host.sign(&self.server_secret));
        Ok(out)
    }

    /// Step 4: verify the client knew the PIN and owns its certificate.
    pub fn on_client_pairing_secret(&self, data: &[u8]) -> Result<()> {
        let (sc, client_hash) = match (&self.server_challenge, &self.client_hash) {
            (Some(sc), Some(h)) => (sc, h),
            _ => return Err(PairError::OutOfOrder),
        };
        if data.len() <= 16 {
            return Err(PairError::Malformed);
        }
        let (secret, sig) = data.split_at(16);
        crypto::verify_signature(&self.client_cert_der, secret, sig).map_err(|_| PairError::BadSignature)?;
        let client_sig = crypto::cert_signature(&self.client_cert_der)?;
        let expected = HashAlgo::Sha256.hash(&[sc, &client_sig, secret]);
        if &expected != client_hash {
            return Err(PairError::PinMismatch);
        }
        Ok(())
    }
}

/// Client side of a pairing attempt.
pub struct ClientPairing {
    pub algo: HashAlgo,
    pub salt: [u8; 16],
    key: [u8; 16],
    client_challenge: [u8; 16],
    client_secret: [u8; 16],
    server_cert_der: Option<Vec<u8>>,
    server_hash: Option<Vec<u8>>,
}

impl ClientPairing {
    pub fn new(algo: HashAlgo, pin: &str) -> Self {
        let salt = crypto::random_bytes();
        Self {
            algo,
            salt,
            key: derive_key(algo, &salt, pin),
            client_challenge: crypto::random_bytes(),
            client_secret: crypto::random_bytes(),
            server_cert_der: None,
            server_hash: None,
        }
    }

    pub fn set_server_cert(&mut self, der: Vec<u8>) {
        self.server_cert_der = Some(der);
    }

    pub fn server_cert(&self) -> Option<&[u8]> {
        self.server_cert_der.as_deref()
    }

    /// Step 2 request payload.
    pub fn encrypted_client_challenge(&self) -> Vec<u8> {
        crypto::aes_ecb_encrypt(&self.key, &self.client_challenge)
    }

    /// Handle step 2 response; returns step 3 request payload.
    pub fn on_challenge_response(&mut self, me: &Identity, enc: &[u8]) -> Result<Vec<u8>> {
        let dec = crypto::aes_ecb_decrypt(&self.key, enc)?;
        let hl = self.algo.len();
        if dec.len() < hl + 16 {
            return Err(PairError::Malformed);
        }
        self.server_hash = Some(dec[..hl].to_vec());
        let sc = &dec[hl..hl + 16];
        let h = self.algo.hash(&[sc, &me.cert_signature(), &self.client_secret]);
        Ok(crypto::aes_ecb_encrypt(&self.key, &pad16(h)))
    }

    /// Handle step 3 response: authenticate the server.
    pub fn on_pairing_secret(&self, data: &[u8]) -> Result<()> {
        let cert = self.server_cert_der.as_ref().ok_or(PairError::OutOfOrder)?;
        let server_hash = self.server_hash.as_ref().ok_or(PairError::OutOfOrder)?;
        if data.len() <= 16 {
            return Err(PairError::Malformed);
        }
        let (secret, sig) = data.split_at(16);
        crypto::verify_signature(cert, secret, sig).map_err(|_| PairError::BadSignature)?;
        let expected = self.algo.hash(&[&self.client_challenge, &crypto::cert_signature(cert)?, secret]);
        if &expected != server_hash {
            return Err(PairError::PinMismatch);
        }
        Ok(())
    }

    /// Step 4 request payload.
    pub fn client_pairing_secret(&self, me: &Identity) -> Vec<u8> {
        let mut out = self.client_secret.to_vec();
        out.extend_from_slice(&me.sign(&self.client_secret));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn ids() -> &'static (Identity, Identity) {
        static IDS: OnceLock<(Identity, Identity)> = OnceLock::new();
        IDS.get_or_init(|| (Identity::generate("host").unwrap(), Identity::generate("client").unwrap()))
    }

    fn run(client_pin: &str, server_pin: &str) -> std::result::Result<(), PairError> {
        let (host, client) = ids();
        let mut c = ClientPairing::new(HashAlgo::Sha256, client_pin);
        let mut s = ServerPairing::new(&c.salt, server_pin, client.cert_der.clone());
        c.set_server_cert(host.cert_der.clone());
        let resp = s.on_client_challenge(host, &c.encrypted_client_challenge())?;
        let scr = c.on_challenge_response(client, &resp)?;
        let ps = s.on_server_challenge_resp(host, &scr)?;
        c.on_pairing_secret(&ps)?;
        s.on_client_pairing_secret(&c.client_pairing_secret(client))
    }

    #[test]
    fn pairing_succeeds_with_same_pin() {
        run("1234", "1234").unwrap();
    }

    #[test]
    fn pairing_fails_with_wrong_pin() {
        assert!(run("1234", "4321").is_err());
    }

    #[test]
    fn out_of_order_rejected() {
        let (host, client) = ids();
        let mut s = ServerPairing::new(&[0; 16], "0000", client.cert_der.clone());
        assert!(matches!(s.on_server_challenge_resp(host, &[0; 32]), Err(PairError::OutOfOrder)));
        assert!(matches!(s.on_client_pairing_secret(&[0; 272]), Err(PairError::OutOfOrder)));
    }
}
