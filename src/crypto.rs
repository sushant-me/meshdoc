//! Cryptographic primitives.
//!
//! Design:
//! * Identity = an Ed25519 signing key + an X25519 key-agreement key, generated on
//!   the device and never transmitted.
//! * A workspace epoch has a random 256-bit symmetric key, shared out of band (QR)
//!   or re-shared to members by a *key grant* sealed to their X25519 key.
//! * Payloads are encrypted with XChaCha20-Poly1305, authenticated against a
//!   header passed as AAD.
//! * Every change is signed by its author's Ed25519 key.
//!
//! We do not implement primitives ourselves; these are audited crates.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};
use zeroize::Zeroize;

use crate::ids::{hex, unhex, PeerId};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const SIG_LEN: usize = 64;
pub const GRANT_LEN: usize = 32 + NONCE_LEN + KEY_LEN + 16; // eph || nonce || ct+tag

#[derive(Debug)]
pub enum CryptoError {
    Seal,
    Open,
    Short,
    Verify,
    BadKey,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Seal => write!(f, "encryption failed"),
            CryptoError::Open => write!(f, "decryption failed (wrong key, wrong epoch, or tampered)"),
            CryptoError::Short => write!(f, "ciphertext too short"),
            CryptoError::Verify => write!(f, "signature verification failed"),
            CryptoError::BadKey => write!(f, "malformed key material"),
        }
    }
}
impl std::error::Error for CryptoError {}

/// A device's long-lived identity. Never leaves the device.
pub struct Identity {
    signing: SigningKey,
    dh: StaticSecret,
}

/// The public half of an identity: what other peers need in order to verify
/// signatures and to seal key grants.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub verify: [u8; 32],
    pub dh: [u8; 32],
}

impl PublicIdentity {
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.verify);
        out[32..].copy_from_slice(&self.dh);
        out
    }

    pub fn from_bytes(b: &[u8; 64]) -> Self {
        let mut v = [0u8; 32];
        let mut d = [0u8; 32];
        v.copy_from_slice(&b[..32]);
        d.copy_from_slice(&b[32..]);
        Self { verify: v, dh: d }
    }

    pub fn to_hex(&self) -> String { hex(&self.to_bytes()) }

    pub fn from_hex(s: &str) -> Option<Self> {
        let raw = unhex(s)?;
        if raw.len() != 64 { return None; }
        let mut b = [0u8; 64];
        b.copy_from_slice(&raw);
        Some(Self::from_bytes(&b))
    }

    pub fn fingerprint(&self) -> PeerId {
        let h = blake3::hash(&self.to_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.as_bytes()[..16]);
        PeerId(out)
    }

    pub fn verifying_key(&self) -> Result<VerifyingKey, CryptoError> {
        VerifyingKey::from_bytes(&self.verify).map_err(|_| CryptoError::BadKey)
    }

    pub fn dh_public(&self) -> X25519Public { X25519Public::from(self.dh) }

    /// Verify `sig` over `msg`.
    pub fn verify(&self, msg: &[u8], sig: &[u8]) -> Result<(), CryptoError> {
        if sig.len() != SIG_LEN { return Err(CryptoError::Short); }
        let vk = self.verifying_key()?;
        let mut s = [0u8; SIG_LEN];
        s.copy_from_slice(sig);
        vk.verify(msg, &Signature::from_bytes(&s))
            .map_err(|_| CryptoError::Verify)
    }
}

impl Identity {
    pub fn generate() -> Self {
        let signing = SigningKey::generate(&mut OsRng);
        let dh = StaticSecret::random_from_rng(OsRng);
        Self { signing, dh }
    }

    /// Deterministically rebuild from a 32-byte seed plus a 32-byte DH secret.
    pub fn from_secret_bytes(seed: &[u8; 32], dh_secret: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let dh = StaticSecret::from(*dh_secret);
        Self { signing, dh }
    }

    /// Serialise the secrets so the device can persist them. Callers must
    /// encrypt this at rest before writing it to disk.
    pub fn to_secret_bytes(&self) -> ([u8; 32], [u8; 32]) {
        (self.signing.to_bytes(), self.dh.to_bytes())
    }

    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            verify: self.signing.verifying_key().to_bytes(),
            dh: X25519Public::from(&self.dh).to_bytes(),
        }
    }

    pub fn fingerprint(&self) -> PeerId { self.public().fingerprint() }

    pub fn sign(&self, msg: &[u8]) -> [u8; SIG_LEN] {
        self.signing.sign(msg).to_bytes()
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        // Best-effort scrubbing; the dalek types also zeroize on drop.
        let mut z = self.signing.to_bytes();
        z.zeroize();
    }
}

/// A workspace epoch's symmetric key.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceKey(pub [u8; KEY_LEN]);

impl WorkspaceKey {
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut k = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut k);
        Self(k)
    }

    pub fn from_slice(s: &[u8]) -> Result<Self, CryptoError> {
        if s.len() != KEY_LEN { return Err(CryptoError::Short); }
        let mut k = [0u8; KEY_LEN];
        k.copy_from_slice(s);
        Ok(Self(k))
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] { &self.0 }

    /// Encrypt `plaintext` with `aad` bound in. Output: nonce || ciphertext.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        use rand::RngCore;
        let cipher = XChaCha20Poly1305::new_from_slice(&self.0).map_err(|_| CryptoError::BadKey)?;
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
            .map_err(|_| CryptoError::Seal)?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt output of [`WorkspaceKey::seal`].
    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < NONCE_LEN + 16 { return Err(CryptoError::Short); }
        let cipher = XChaCha20Poly1305::new_from_slice(&self.0).map_err(|_| CryptoError::BadKey)?;
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        cipher
            .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| CryptoError::Open)
    }
}

impl std::fmt::Debug for WorkspaceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        write!(f, "WorkspaceKey(<redacted>)")
    }
}

impl Drop for WorkspaceKey {
    fn drop(&mut self) { self.0.zeroize(); }
}

fn grant_kdf(shared: &[u8]) -> [u8; 32] {
    blake3::derive_key("meshdoc key-grant v1", shared)
}

/// Seal `key` to a member's X25519 public key using an ephemeral sender key.
/// Output: eph_pub(32) || nonce(24) || ciphertext(32+16).
pub fn seal_key_grant(
    recipient: &PublicIdentity,
    key: &WorkspaceKey,
) -> Result<Vec<u8>, CryptoError> {
    let eph = StaticSecret::random_from_rng(OsRng);
    let shared = eph.diffie_hellman(&recipient.dh_public());
    let sym = WorkspaceKey(grant_kdf(shared.as_bytes()));
    let sealed = sym.seal(&key.0, b"meshdoc-key-grant-v1")?;
    let mut out = Vec::with_capacity(32 + sealed.len());
    out.extend_from_slice(X25519Public::from(&eph).as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open a key grant addressed to `me`.
pub fn open_key_grant(me: &Identity, blob: &[u8]) -> Result<WorkspaceKey, CryptoError> {
    if blob.len() < 32 + NONCE_LEN + 16 { return Err(CryptoError::Short); }
    let (eph_bytes, rest) = blob.split_at(32);
    let mut eb = [0u8; 32];
    eb.copy_from_slice(eph_bytes);
    let eph_pub = X25519Public::from(eb);
    // StaticSecret::diffie_hellman is available on the secret side only, so we
    // reconstruct a shared secret via the public-key API.
    let shared = me.dh.diffie_hellman(&eph_pub);
    let sym = WorkspaceKey(grant_kdf(shared.as_bytes()));
    let pt = sym.open(rest, b"meshdoc-key-grant-v1")?;
    WorkspaceKey::from_slice(&pt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let k = WorkspaceKey::generate();
        let ct = k.seal(b"hello mesh", b"aad42").unwrap();
        assert_ne!(&ct[..], b"hello mesh");
        assert_eq!(k.open(&ct, b"aad42").unwrap(), b"hello mesh");
    }

    #[test]
    fn open_fails_with_wrong_aad() {
        let k = WorkspaceKey::generate();
        let ct = k.seal(b"hello", b"aad-one").unwrap();
        assert!(k.open(&ct, b"aad-two").is_err());
    }

    #[test]
    fn open_fails_with_wrong_key() {
        let k1 = WorkspaceKey::generate();
        let k2 = WorkspaceKey::generate();
        let ct = k1.seal(b"secret", b"a").unwrap();
        assert!(k2.open(&ct, b"a").is_err());
    }

    #[test]
    fn sign_verify_roundtrip() {
        let id = Identity::generate();
        let sig = id.sign(b"payload");
        id.public().verify(b"payload", &sig).unwrap();
        assert!(id.public().verify(b"payload2", &sig).is_err());
    }

    #[test]
    fn key_grant_roundtrip() {
        let _alice = Identity::generate();
        let bob = Identity::generate();
        let wk = WorkspaceKey::generate();
        let grant = seal_key_grant(&bob.public(), &wk).unwrap();
        let opened = open_key_grant(&bob, &grant).unwrap();
        assert_eq!(opened, wk);
        // carol must not be able to open bob's grant
        let carol = Identity::generate();
        assert!(open_key_grant(&carol, &grant).is_err());
    }

    #[test]
    fn fingerprint_is_public_identity_bound() {
        let a = Identity::generate();
        let b = Identity::generate();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.public().fingerprint());
    }
}
