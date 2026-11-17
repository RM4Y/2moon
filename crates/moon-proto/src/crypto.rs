//! Cryptographic primitives used by the GameStream-compatible protocol:
//! RSA-2048 identities with self-signed X.509 certificates, SHA-256,
//! AES-128-ECB (pairing), AES-128-GCM (control stream / input) and
//! AES-128-CBC (legacy input encryption).

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use aes_gcm::aead::{AeadInPlace, Nonce, Tag};
use aes_gcm::Aes128Gcm;
use der::{Decode, DecodePem, Encode, EncodePem};
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use spki::{DecodePublicKey, SubjectPublicKeyInfoOwned};
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::Validity;
use x509_cert::Certificate;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("rsa: {0}")]
    Rsa(#[from] rsa::Error),
    #[error("der: {0}")]
    Der(#[from] der::Error),
    #[error("pkcs8: {0}")]
    Pkcs8(#[from] rsa::pkcs8::Error),
    #[error("spki: {0}")]
    Spki(#[from] spki::Error),
    #[error("certificate builder: {0}")]
    Builder(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid data: {0}")]
    Invalid(&'static str),
    #[error("signature verification failed")]
    BadSignature,
    #[error("aead failure")]
    Aead,
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// An RSA key pair plus a self-signed certificate. Used both by the host
/// (server certificate) and by clients (client certificate).
#[derive(Clone)]
pub struct Identity {
    pub key: RsaPrivateKey,
    pub cert_der: Vec<u8>,
}

impl Identity {
    /// Generate a fresh RSA-2048 key and a self-signed certificate valid for 20 years.
    pub fn generate(common_name: &str) -> Result<Self> {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 2048)?;
        let cert_der = self_signed_cert(&key, common_name)?;
        Ok(Self { key, cert_der })
    }

    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self> {
        let cert = Certificate::from_pem(cert_pem.as_bytes())?;
        let key = RsaPrivateKey::from_pkcs8_pem(key_pem)?;
        Ok(Self { key, cert_der: cert.to_der()? })
    }

    pub fn cert_pem(&self) -> String {
        pem_encode_cert(&self.cert_der)
    }

    pub fn key_pem(&self) -> String {
        self.key.to_pkcs8_pem(LineEnding::LF).expect("pkcs8 encode").to_string()
    }

    pub fn key_pkcs8_der(&self) -> Vec<u8> {
        self.key.to_pkcs8_der().expect("pkcs8 encode").as_bytes().to_vec()
    }

    /// Raw signature bytes of our own certificate.
    pub fn cert_signature(&self) -> Vec<u8> {
        cert_signature(&self.cert_der).expect("own certificate is valid")
    }

    /// RSA PKCS#1 v1.5 SHA-256 signature.
    pub fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sk = SigningKey::<Sha256>::new(self.key.clone());
        sk.sign(data).to_vec()
    }

    /// Load identity from `dir/<name>.crt` + `dir/<name>.key`, or generate and save it.
    pub fn load_or_generate(dir: &Path, name: &str, common_name: &str) -> Result<Self> {
        let crt = dir.join(format!("{name}.crt"));
        let key = dir.join(format!("{name}.key"));
        if crt.exists() && key.exists() {
            return Self::from_pem(&std::fs::read_to_string(&crt)?, &std::fs::read_to_string(&key)?);
        }
        std::fs::create_dir_all(dir)?;
        let id = Self::generate(common_name)?;
        std::fs::write(&crt, id.cert_pem())?;
        write_private(&key, id.key_pem().as_bytes())?;
        Ok(id)
    }
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(data)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

fn self_signed_cert(key: &RsaPrivateKey, cn: &str) -> Result<Vec<u8>> {
    let signer = SigningKey::<Sha256>::new(key.clone());
    let spki = SubjectPublicKeyInfoOwned::from_key(key.to_public_key())?;
    let serial: u32 = rand::random::<u32>() & 0x7fff_ffff;
    let validity = Validity::from_now(Duration::from_secs(20 * 365 * 86_400))?;
    let subject = Name::from_str(&format!("CN={cn}")).map_err(|e| CryptoError::Builder(e.to_string()))?;
    let builder = CertificateBuilder::new(Profile::Root, SerialNumber::from(serial.max(1)), validity, subject, spki, &signer)
        .map_err(|e| CryptoError::Builder(e.to_string()))?;
    let cert = builder.build::<Signature>().map_err(|e| CryptoError::Builder(e.to_string()))?;
    Ok(cert.to_der()?)
}

pub fn pem_encode_cert(der: &[u8]) -> String {
    let cert = Certificate::from_der(der).expect("valid der");
    cert.to_pem(LineEnding::LF).expect("pem encode")
}

/// Parse a PEM certificate (tolerates surrounding whitespace / CRLF).
pub fn pem_decode_cert(pem: &str) -> Result<Vec<u8>> {
    let normalized = pem.replace("\r\n", "\n");
    let cert = Certificate::from_pem(normalized.trim().as_bytes())?;
    Ok(cert.to_der()?)
}

/// The raw `signatureValue` bytes of an X.509 certificate.
pub fn cert_signature(cert_der: &[u8]) -> Result<Vec<u8>> {
    let cert = Certificate::from_der(cert_der)?;
    cert.signature.as_bytes().map(|b| b.to_vec()).ok_or(CryptoError::Invalid("unaligned signature bitstring"))
}

pub fn cert_public_key(cert_der: &[u8]) -> Result<RsaPublicKey> {
    let cert = Certificate::from_der(cert_der)?;
    let spki_der = cert.tbs_certificate.subject_public_key_info.to_der()?;
    Ok(RsaPublicKey::from_public_key_der(&spki_der)?)
}

/// Verify an RSA PKCS#1 v1.5 SHA-256 signature made by the certificate's key.
pub fn verify_signature(cert_der: &[u8], data: &[u8], sig: &[u8]) -> Result<()> {
    let pk = cert_public_key(cert_der)?;
    let vk = VerifyingKey::<Sha256>::new(pk);
    let sig = Signature::try_from(sig).map_err(|_| CryptoError::BadSignature)?;
    vk.verify(data, &sig).map_err(|_| CryptoError::BadSignature)
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// AES-128 in ECB mode, no padding. `data.len()` must be a multiple of 16.
pub fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    assert!(data.len() % 16 == 0, "ECB data must be block aligned");
    let c = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        c.encrypt_block(block.into());
    }
    out
}

pub fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() % 16 != 0 {
        return Err(CryptoError::Invalid("ECB ciphertext not block aligned"));
    }
    let c = Aes128::new(key.into());
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        c.decrypt_block(block.into());
    }
    Ok(out)
}

/// AES-128-GCM with detached 16-byte tag. Returns (tag, ciphertext).
pub fn gcm_encrypt(key: &[u8; 16], iv: &[u8; 12], plaintext: &[u8]) -> ([u8; 16], Vec<u8>) {
    let c = Aes128Gcm::new(key.into());
    let mut buf = plaintext.to_vec();
    let tag = c.encrypt_in_place_detached(Nonce::<Aes128Gcm>::from_slice(iv), b"", &mut buf).expect("gcm encrypt");
    (tag.into(), buf)
}

pub fn gcm_decrypt(key: &[u8; 16], iv: &[u8; 12], tag: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if tag.len() != 16 {
        return Err(CryptoError::Invalid("bad gcm tag length"));
    }
    let c = Aes128Gcm::new(key.into());
    let mut buf = ciphertext.to_vec();
    c.decrypt_in_place_detached(Nonce::<Aes128Gcm>::from_slice(iv), b"", &mut buf, Tag::<Aes128Gcm>::from_slice(tag))
        .map_err(|_| CryptoError::Aead)?;
    Ok(buf)
}

/// AES-128-CBC with PKCS#7 padding (legacy GameStream input encryption).
pub fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
    use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    cbc::Encryptor::<Aes128>::new(key.into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

pub fn cbc_decrypt(key: &[u8; 16], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>> {
    use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    cbc::Decryptor::<Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| CryptoError::Invalid("bad cbc padding"))
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip_and_signatures() {
        let id = Identity::generate("Test").unwrap();
        let pem = id.cert_pem();
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        let id2 = Identity::from_pem(&pem, &id.key_pem()).unwrap();
        assert_eq!(id.cert_der, id2.cert_der);
        assert_eq!(pem_decode_cert(&pem.replace('\n', "\r\n")).unwrap(), id.cert_der);
        assert_eq!(id.cert_signature().len(), 256);
        let sig = id.sign(b"hello");
        verify_signature(&id.cert_der, b"hello", &sig).unwrap();
        assert!(verify_signature(&id.cert_der, b"hellO", &sig).is_err());
        // The certificate must verify its own signature (self-signed).
        let cert = Certificate::from_der(&id.cert_der).unwrap();
        let tbs = cert.tbs_certificate.to_der().unwrap();
        verify_signature(&id.cert_der, &tbs, &id.cert_signature()).unwrap();
    }

    #[test]
    fn aes_modes() {
        let key = [7u8; 16];
        let data = [1u8; 48];
        let e = aes_ecb_encrypt(&key, &data);
        assert_ne!(e, data);
        assert_eq!(aes_ecb_decrypt(&key, &e).unwrap(), data);

        let iv = [3u8; 12];
        let (tag, ct) = gcm_encrypt(&key, &iv, b"control message");
        assert_eq!(gcm_decrypt(&key, &iv, &tag, &ct).unwrap(), b"control message");
        let mut bad = ct.clone();
        bad[0] ^= 1;
        assert!(gcm_decrypt(&key, &iv, &tag, &bad).is_err());

        let civ = [9u8; 16];
        let ct = cbc_encrypt(&key, &civ, b"input");
        assert_eq!(ct.len(), 16);
        assert_eq!(cbc_decrypt(&key, &civ, &ct).unwrap(), b"input");
    }
}
