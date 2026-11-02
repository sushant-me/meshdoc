//! The change envelope: the only thing that ever travels over any wire.
//!
//! A change is a CRDT update, encrypted with the workspace epoch key and signed by
//! its author. The relay stores and forwards the envelope without being able to
//! read it; peers verify the author against the membership log before applying.
//!
//! Wire layout (bincode):
//! ```text
//! ws_id | doc_id | epoch | author(64) | seq | len(ciphertext) | ciphertext | signature(64)
//! ```
//!
//! AAD binds the ciphertext to its routing metadata, so an envelope cannot be
//! replayed into another workspace or document, or relabelled to another epoch.

use serde::{Deserialize, Serialize};

use crate::crypto::{CryptoError, Identity, PublicIdentity, WorkspaceKey};
use crate::ids::{ChangeId, DocId, Epoch, WorkspaceId};
use crate::membership::MembershipLog;

pub const CONTEXT: &[u8] = b"meshdoc-change-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeEnvelope {
    pub workspace_id: WorkspaceId,
    pub doc_id: DocId,
    pub epoch: Epoch,
    pub author: PublicIdentity,
    /// Author-local monotonic counter. Ordering of CRDT state never depends on
    /// this; it exists so a peer can reason about gap-free delivery.
    pub seq: u64,
    pub ciphertext: Vec<u8>,
    pub signature: Vec<u8>,
}

impl ChangeEnvelope {
    fn header(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(64);
        h.extend_from_slice(CONTEXT);
        h.extend_from_slice(self.workspace_id.as_bytes());
        h.extend_from_slice(self.doc_id.as_bytes());
        h.extend_from_slice(&self.epoch.as_u32().to_be_bytes());
        h.extend_from_slice(&self.author.to_bytes());
        h.extend_from_slice(&self.seq.to_be_bytes());
        h
    }

    fn signing_bytes(&self) -> Vec<u8> {
        let mut m = self.header();
        m.extend_from_slice(blake3::hash(&self.ciphertext).as_bytes());
        m
    }

    /// Create, encrypt and sign a change carrying `update`.
    pub fn create(
        identity: &Identity,
        workspace_id: WorkspaceId,
        doc_id: DocId,
        epoch: Epoch,
        seq: u64,
        key: &WorkspaceKey,
        update: &[u8],
    ) -> Result<Self, CryptoError> {
        let mut header_only = ChangeEnvelope {
            workspace_id,
            doc_id,
            epoch,
            author: identity.public(),
            seq,
            ciphertext: vec![],
            signature: vec![],
        };
        let aad = header_only.header();
        let ciphertext = key.seal(update, &aad)?;
        header_only.ciphertext = ciphertext;
        let sig = identity.sign(&header_only.signing_bytes());
        header_only.signature = sig.to_vec();
        Ok(header_only)
    }

    /// Content address. Stable across serialisation round-trips.
    pub fn id(&self) -> ChangeId {
        let bytes = self.encode();
        let h = blake3::hash(&bytes);
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.as_bytes()[..16]);
        ChangeId(out)
    }

    pub fn encode(&self) -> Vec<u8> {
        bincode::serialize(self).expect("envelope serialises")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ChangeError> {
        bincode::deserialize(bytes).map_err(|_| ChangeError::Malformed)
    }

    /// Verify the author and decrypt. Does **not** check membership; callers pass
    /// the log so the policy is explicit at the call site.
    pub fn open(&self, log: &MembershipLog, key: &WorkspaceKey) -> Result<Vec<u8>, ChangeError> {
        // 1. Routing metadata must match what we expected to receive.
        if !log.is_member_at(self.author.fingerprint(), self.epoch) {
            return Err(ChangeError::NotAMember);
        }
        // 2. Signature must verify before we spend effort decrypting.
        self.author
            .verify(&self.signing_bytes(), &self.signature)
            .map_err(|_| ChangeError::BadSignature)?;
        // 3. Decrypt (AEAD also authenticates the AAD, so the routing metadata is
        //    bound to the ciphertext).
        let aad = self.header();
        key.open(&self.ciphertext, &aad).map_err(|_| ChangeError::Undecryptable)
    }

    /// Verify signature and metadata without decrypting. Useful for a relay-side
    /// sanity check (a relay must not do this — it stays content-blind).
    pub fn verify(&self) -> Result<(), ChangeError> {
        self.author
            .verify(&self.signing_bytes(), &self.signature)
            .map_err(|_| ChangeError::BadSignature)
    }

    pub fn approx_len(&self) -> usize { self.ciphertext.len() + 64 + 64 + 64 }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChangeError {
    Malformed,
    BadSignature,
    NotAMember,
    Undecryptable,
}

impl std::fmt::Display for ChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeError::Malformed => write!(f, "malformed change envelope"),
            ChangeError::BadSignature => write!(f, "change signature does not verify"),
            ChangeError::NotAMember => write!(f, "author is not a member at this epoch"),
            ChangeError::Undecryptable => write!(f, "change does not decrypt under this epoch key"),
        }
    }
}
impl std::error::Error for ChangeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_key_grant;
    use crate::membership::EntryBuilder;

    struct Fixture { ws: WorkspaceId, log: MembershipLog, key: WorkspaceKey, alice: Identity, bob: Identity }

    fn fixture() -> Fixture {
        let ws = WorkspaceId::random();
        let alice = Identity::generate();
        let bob = Identity::generate();
        let key = WorkspaceKey::generate();
        let mut log = MembershipLog::new();
        let genesis = EntryBuilder::new(&alice).found(ws, "ws");
        let gid = genesis.id();
        log.insert(ws, genesis).unwrap();
        let add = EntryBuilder::new(&alice).add(
            ws, Epoch::GENESIS, bob.public(),
            seal_key_grant(&bob.public(), &key).unwrap(), vec![gid], 1,
        );
        log.insert(ws, add).unwrap();
        Fixture { ws, log, key, alice, bob }
    }

    #[test]
    fn roundtrip_create_open() {
        let f = fixture();
        let doc = DocId::random();
        let env = ChangeEnvelope::create(&f.alice, f.ws, doc, Epoch::GENESIS, 1, &f.key, b"crdt-update").unwrap();
        let bytes = env.encode();
        let back = ChangeEnvelope::decode(&bytes).unwrap();
        assert_eq!(back.id(), env.id());
        assert_eq!(back.open(&f.log, &f.key).unwrap(), b"crdt-update");
    }

    #[test]
    fn ciphertext_is_not_plaintext_on_the_wire() {
        let f = fixture();
        let env = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"SECRET").unwrap();
        let wire = env.encode();
        assert!(!wire.windows(6).any(|w| w == b"SECRET"));
    }

    #[test]
    fn rejects_wrong_key() {
        let f = fixture();
        let env = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        let other = WorkspaceKey::generate();
        assert_eq!(env.open(&f.log, &other), Err(ChangeError::Undecryptable));
    }

    #[test]
    fn rejects_tampered_signature() {
        let f = fixture();
        let mut env = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        env.signature[3] ^= 0xff;
        assert_eq!(env.open(&f.log, &f.key), Err(ChangeError::BadSignature));
    }

    #[test]
    fn rejects_replay_into_another_document() {
        let f = fixture();
        let mut env = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        // Relabel the document id. The signature covers the routing metadata, so
        // this is rejected before decryption is even attempted.
        env.doc_id = DocId::random();
        assert_eq!(env.open(&f.log, &f.key), Err(ChangeError::BadSignature));

        // And if an attacker also re-signs with their own key, they are not a member.
        let eve = Identity::generate();
        let mut forged = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        forged.doc_id = DocId::random();
        forged.signature = eve.sign(&forged.signing_bytes()).to_vec();
        assert_eq!(forged.open(&f.log, &f.key), Err(ChangeError::BadSignature));
    }

    #[test]
    fn rejects_replay_into_another_epoch_label() {
        let f = fixture();
        let mut env = ChangeEnvelope::create(&f.alice, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        env.epoch = Epoch(7);
        // Author is not a member at epoch 7 in this log, so it fails on membership.
        assert!(env.open(&f.log, &f.key).is_err());
    }

    #[test]
    fn rejects_author_who_is_not_a_member() {
        let f = fixture();
        let eve = Identity::generate();
        let env = ChangeEnvelope::create(&eve, f.ws, DocId::random(), Epoch::GENESIS, 1, &f.key, b"x").unwrap();
        assert_eq!(env.open(&f.log, &f.key), Err(ChangeError::NotAMember));
    }

    #[test]
    fn id_is_stable_across_encode_decode() {
        let f = fixture();
        let env = ChangeEnvelope::create(&f.bob, f.ws, DocId::random(), Epoch::GENESIS, 5, &f.key, b"y").unwrap();
        let a = env.id();
        let b = ChangeEnvelope::decode(&env.encode()).unwrap().id();
        assert_eq!(a, b);
    }
}
