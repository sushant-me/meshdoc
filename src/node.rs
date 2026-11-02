//! A running peer: identity + one workspace + documents + change log.
//!
//! # The lifecycle
//!
//! ```text
//! create_workspace -> invite(peer_pubkey) -> [QR/token] -> join_from_invite
//! create_doc -> edit -> (change envelope) -> sync_with(peer) -> convergent state
//!                        \-> push_to_relay -> (other peer pulls later)
//! ```
//!
//! Every local edit produces exactly one [`ChangeEnvelope`]. That envelope is the
//! only unit that syncs, over the mesh or the relay; both paths use the same bytes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::change::{ChangeEnvelope, ChangeError};
use crate::crypto::{seal_key_grant, CryptoError, Identity, PublicIdentity, WorkspaceKey};
use crate::docs::{DocError, DocType, Document};
use crate::ids::{ChangeId, DocId, Epoch, PeerId, WorkspaceId};
use crate::membership::{EntryBuilder, MembershipEntry, MembershipError, MembershipLog};

pub type Result<T> = std::result::Result<T, NodeError>;

#[derive(Debug)]
pub enum NodeError {
    UnknownDoc(DocId),
    NoKey(Epoch),
    Crypto(CryptoError),
    Change(ChangeError),
    Membership(MembershipError),
    Doc(DocError),
    EmptyInvite,
    BadInvite,
    WorkspaceMismatch { mine: WorkspaceId, theirs: WorkspaceId },
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeError::UnknownDoc(d) => write!(f, "unknown document {}", &d.to_hex()[..8]),
            NodeError::NoKey(e) => write!(f, "no workspace key for {e}"),
            NodeError::Crypto(e) => write!(f, "{e}"),
            NodeError::Change(e) => write!(f, "{e}"),
            NodeError::Membership(e) => write!(f, "{e}"),
            NodeError::Doc(e) => write!(f, "{e}"),
            NodeError::EmptyInvite => write!(f, "invite contains no membership entries"),
            NodeError::BadInvite => write!(f, "invite is malformed"),
            NodeError::WorkspaceMismatch { mine, theirs } => write!(
                f, "workspace mismatch: this node is in {}, peer is in {}",
                &mine.to_hex()[..8], &theirs.to_hex()[..8]
            ),
        }
    }
}
impl std::error::Error for NodeError {}

impl From<CryptoError> for NodeError { fn from(e: CryptoError) -> Self { NodeError::Crypto(e) } }
impl From<ChangeError> for NodeError { fn from(e: ChangeError) -> Self { NodeError::Change(e) } }
impl From<MembershipError> for NodeError { fn from(e: MembershipError) -> Self { NodeError::Membership(e) } }
impl From<DocError> for NodeError { fn from(e: DocError) -> Self { NodeError::Doc(e) } }

/// The token a new member needs. It is the entire membership log, which contains
/// a key grant sealed to that member's X25519 key — so possession of the token
/// alone is useful only to the member it was minted for.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<MembershipEntry>,
}

impl Invite {
    /// URL-safe text form, suitable for a QR code or the clipboard.
    pub fn to_token(&self) -> String {
        use base64::Engine as _;
        let json = serde_json::to_vec(self).expect("invite serialises");
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    pub fn from_token(tok: &str) -> Result<Self> {
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(tok.trim())
            .map_err(|_| NodeError::BadInvite)?;
        serde_json::from_slice(&raw).map_err(|_| NodeError::BadInvite)
    }
}

/// Plain (unencrypted) node state. The store encrypts this before it touches disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistState {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<MembershipEntry>,
    pub changes: Vec<Vec<u8>>,
    pub seq: u64,
}

pub struct Node {
    pub identity: Identity,
    pub workspace_id: WorkspaceId,
    pub log: MembershipLog,
    keys: BTreeMap<u32, WorkspaceKey>,
    docs: BTreeMap<DocId, Document>,
    /// The change log: everything we know, encrypted. This is what syncs.
    changes: BTreeMap<ChangeId, ChangeEnvelope>,
    applied: BTreeSet<ChangeId>,
    /// Per-document watermark: the CRDT state vector as of the last change we
    /// authored *or* applied for that document. A local edit emits only what is
    /// past this watermark, so every operation the replica knows reaches the
    /// change log exactly once and can never be known-but-never-transmitted.
    watermarks: BTreeMap<DocId, Vec<u8>>,
    /// Changes we hold but could not apply, with the reason. Observability for
    /// operations: a non-empty map after convergence means a real bug.
    rejected: BTreeMap<ChangeId, String>,
    seq: u64,
}

impl Node {
    // ------------------------------------------------------------- construction

    /// Create a brand-new workspace. The caller is the founding member.
    pub fn create_workspace(identity: Identity, name: &str) -> Result<Self> {
        let ws = WorkspaceId::random();
        let key = WorkspaceKey::generate();
        let genesis = EntryBuilder::new(&identity).found(ws, name);
        let genesis_id = genesis.id();
        let mut log = MembershipLog::new();
        log.insert(ws, genesis)?;
        // Give ourselves a grant too, so a reinstall can recover the key from the log.
        let self_grant = seal_key_grant(&identity.public(), &key)?;
        let own = EntryBuilder::new(&identity).regrant(
            ws, Epoch::GENESIS, identity.public(), self_grant, vec![genesis_id], 0,
        );
        log.insert(ws, own)?;

        let mut keys = BTreeMap::new();
        keys.insert(Epoch::GENESIS.0, key);
        Ok(Self {
            identity,
            workspace_id: ws,
            log,
            keys,
            docs: BTreeMap::new(),
            changes: BTreeMap::new(),
            applied: BTreeSet::new(),
            watermarks: BTreeMap::new(),
            rejected: BTreeMap::new(),
            seq: 0,
        })
    }

    /// Mint an invite for a specific joining identity. This is the moment a new
    /// member is authorised: we sign an `Add` entry and seal the current epoch key
    /// to their X25519 key.
    pub fn invite_for(&mut self, joiner: &PublicIdentity) -> Result<Invite> {
        let epoch = self.log.head_epoch();
        let key = self.key(epoch)?.clone();
        let grant = seal_key_grant(joiner, &key)?;
        let parents = self.tip_ids();
        self.seq += 1;
        let entry = EntryBuilder::new(&self.identity).add(
            self.workspace_id, epoch, *joiner, grant, parents, self.seq,
        );
        self.log.insert(self.workspace_id, entry)?;
        Ok(Invite {
            workspace_id: self.workspace_id,
            entries: self.log.entries().cloned().collect(),
        })
    }

    /// Join a workspace from an invite. Verifies every entry and recovers our key
    /// from the grant sealed to us — no out-of-band key transfer needed.
    pub fn join_from_invite(identity: Identity, invite: &Invite) -> Result<Self> {
        if invite.entries.is_empty() { return Err(NodeError::EmptyInvite); }
        let ws = invite.workspace_id;
        let mut log = MembershipLog::new();
        // Insert in dependency order: retry until no progress (parents before children).
        let mut pending: Vec<MembershipEntry> = invite.entries.clone();
        loop {
            let before = log.len();
            let mut still = vec![];
            for e in pending {
                match log.insert(ws, e.clone()) {
                    Ok(_) => {}
                    Err(MembershipError::MissingParent(_)) => still.push(e),
                    Err(err) => return Err(NodeError::Membership(err)),
                }
            }
            let after = log.len();
            if still.is_empty() { break; }
            if after == before { return Err(NodeError::BadInvite); }
            pending = still;
        }
        let head = log.head_epoch();
        let key = log.key_for(&identity, head).ok_or(NodeError::NoKey(head))?;
        let mut keys = BTreeMap::new();
        // Recover every epoch key that was granted to us.
        for e in log.entries() {
            let m = match &e.action {
                crate::membership::MembershipAction::Add { member, .. } => member.fingerprint(),
                crate::membership::MembershipAction::Regrant { member, .. } => member.fingerprint(),
                _ => continue,
            };
            if m == identity.fingerprint() {
                if keys.contains_key(&e.epoch.0) { continue; }
                if let Some(k) = log.key_for(&identity, e.epoch) {
                    keys.insert(e.epoch.0, k);
                }
            }
        }
        keys.insert(head.0, key);
        Ok(Self {
            identity,
            workspace_id: ws,
            log,
            keys,
            docs: BTreeMap::new(),
            changes: BTreeMap::new(),
            applied: BTreeSet::new(),
            watermarks: BTreeMap::new(),
            rejected: BTreeMap::new(),
            seq: 0,
        })
    }

    // ------------------------------------------------------------------ accessors

    pub fn peer_id(&self) -> PeerId { self.identity.fingerprint() }
    pub fn epoch(&self) -> Epoch { self.log.head_epoch() }
    pub fn workspace_name(&self) -> String { self.log.name().unwrap_or_default() }
    pub fn change_count(&self) -> usize { self.changes.len() }
    pub fn applied_count(&self) -> usize { self.applied.len() }
    pub fn pending_count(&self) -> usize { self.changes.len() - self.applied.len() }
    /// Changes held but not applied, with reasons. Must be empty after convergence.
    pub fn rejected(&self) -> &BTreeMap<ChangeId, String> { &self.rejected }
    pub fn rejected_count(&self) -> usize { self.rejected.len() }
    pub fn doc_ids(&self) -> Vec<DocId> { self.docs.keys().copied().collect() }
    pub fn change_ids(&self) -> Vec<ChangeId> { self.changes.keys().copied().collect() }
    pub fn members(&self) -> BTreeSet<PeerId> { self.log.members() }

    pub fn doc(&self, id: &DocId) -> Option<&Document> { self.docs.get(id) }

    /// Export the epoch key to a member. Needed when minting invites and when
    /// (re)granting a key after a rotation. Never call this for a non-member.
    pub fn epoch_key(&self, epoch: Epoch) -> Option<WorkspaceKey> {
        self.keys.get(&epoch.0).cloned()
    }

    fn key(&self, epoch: Epoch) -> Result<&WorkspaceKey> {
        self.keys.get(&epoch.0).ok_or(NodeError::NoKey(epoch))
    }

    fn tip_ids(&self) -> Vec<ChangeId> {
        // Any entry that is not yet a parent of another entry.
        let all: Vec<ChangeId> = self.log.ids();
        let mut parents: BTreeSet<ChangeId> = BTreeSet::new();
        for e in self.log.entries() {
            for p in &e.parents { parents.insert(*p); }
        }
        let tips: Vec<ChangeId> = all.into_iter().filter(|c| !parents.contains(c)).collect();
        if tips.is_empty() { vec![] } else { tips }
    }

    // ----------------------------------------------------------------- documents

    /// Create a document and emit a change that carries its existence and metadata.
    pub fn create_doc(&mut self, ty: DocType, title: &str) -> Result<DocId> {
        let did = DocId::random();
        let doc = Document::create(did, ty, &self.identity.public(), title);
        let full = doc.encode_state();
        let sv = doc.state_vector();
        self.docs.insert(did, doc);
        self.emit_full(did, &full)?;
        self.watermarks.insert(did, sv);
        Ok(did)
    }

    fn emit_full(&mut self, did: DocId, update: &[u8]) -> Result<ChangeId> {
        let epoch = self.log.head_epoch();
        let key = self.key(epoch)?.clone();
        self.seq += 1;
        let env = ChangeEnvelope::create(
            &self.identity, self.workspace_id, did, epoch, self.seq, &key, update,
        )?;
        let id = env.id();
        self.changes.insert(id, env);
        self.applied.insert(id);
        Ok(id)
    }

    /// Run a local edit and wrap the resulting CRDT delta in exactly one change.
    fn edit<F>(&mut self, did: DocId, f: F) -> Result<ChangeId>
    where
        F: FnOnce(&Document) -> bool,
    {
        let (delta, changed, new_sv) = {
            let doc = self.docs.get(&did).ok_or(NodeError::UnknownDoc(did))?;
            let changed = f(doc);
            // Emit the document's full CRDT state rather than a delta. This is
            // deliberately conservative: a delta that misses an operation (for
            // example one still buffered as pending, or one learned from a peer
            // since our last write) would be silently lost, and a lost operation
            // breaks convergence. Full-state changes are larger but every
            // operation is always carried, so convergence cannot be lost.
            let delta = doc.encode_state();
            (delta, changed, doc.state_vector())
        };
        if !changed || delta.is_empty() {
            // Nothing observable changed: do not emit an empty change.
            self.watermarks.insert(did, new_sv);
            return Ok(ChangeId([0u8; 16]));
        }
        let id = self.emit_full(did, &delta)?;
        self.watermarks.insert(did, new_sv);
        Ok(id)
    }

    pub fn add_checklist_item(&mut self, did: DocId, text: &str) -> Result<ChangeId> {
        let mut id = String::new();
        self.edit(did, |d| { id = d.add_item(text); true })?;
        let _ = id;
        Ok(*self.changes.keys().next_back().unwrap_or(&ChangeId([0u8;16])))
    }

    pub fn toggle_checklist_item(&mut self, did: DocId, item_id: &str, done: bool) -> Result<ChangeId> {
        let me = self.identity.public();
        let iid = item_id.to_string();
        self.edit(did, move |d| d.set_item_done(&iid, done, &me))
    }

    pub fn add_checklist_item_id(&mut self, did: DocId, text: &str) -> Result<(ChangeId, String)> {
        let mut new_id = String::new();
        let cid = self.edit(did, |d| { new_id = d.add_item(text); true })?;
        Ok((cid, new_id))
    }

    pub fn set_supply_field(&mut self, did: DocId, field: &str, value: &str, unit: &str) -> Result<ChangeId> {
        let f = field.to_string();
        let v = value.to_string();
        let u = unit.to_string();
        self.edit(did, move |d| d.set_field(&f, &v, &u))
    }

    pub fn set_location(&mut self, did: DocId, lat: f64, lon: f64, label: &str, kind: &str) -> Result<ChangeId> {
        let l = label.to_string();
        let k = kind.to_string();
        self.edit(did, move |d| { d.set_location(lat, lon, &l, &k); true })
    }

    pub fn set_note(&mut self, did: DocId, text: &str) -> Result<ChangeId> {
        let t = text.to_string();
        self.edit(did, move |d| { d.set_text(&t); true })
    }

    // -------------------------------------------------------------------- sync

    /// Apply a change from the network. Returns `Ok(true)` if it was new.
    ///
    /// Acceptance rules, in order: known already (dedupe by content address),
    /// decryptable under our epoch key, signature valid, author a member at the
    /// claimed epoch (checked inside `open`), CRDT accepts the update.
    pub fn ingest(&mut self, env: ChangeEnvelope) -> Result<bool> {
        let id = env.id();
        if self.changes.contains_key(&id) { return Ok(false); }
        self.changes.insert(id, env.clone());
        self.try_apply(&id)
    }

    fn try_apply(&mut self, id: &ChangeId) -> Result<bool> {
        if self.applied.contains(id) { return Ok(false); }
        let env = match self.changes.get(id) { Some(e) => e.clone(), None => return Ok(false) };
        let key = match self.keys.get(&env.epoch.0) {
            Some(k) => k.clone(),
            // We may not hold this epoch's key yet (rotation raced ahead of us).
            // Keep the envelope: it will be applied once the key arrives.
            None => {
                self.rejected.insert(*id, format!("no key for epoch {}", env.epoch));
                return Ok(false);
            }
        };
        let update = match env.open(&self.log, &key) {
            Ok(u) => u,
            // Not decryptable / not authorised yet. Keep it; a later membership or
            // key update may make it valid. (A hostile change simply never applies.)
            Err(e) => {
                self.rejected.insert(*id, e.to_string());
                return Ok(false);
            }
        };
        if !self.docs.contains_key(&env.doc_id) {
            // A document we have never seen: materialise it from the change.
            let doc = Document::from_doc(env.doc_id, yrs::Doc::new());
            self.docs.insert(env.doc_id, doc);
        }
        let doc = self.docs.get_mut(&env.doc_id).expect("just inserted");
        // Apply first; never create containers on a replica before the origin's
        // update lands (see Document::migrate for why).
        if let Err(e) = doc.apply_update(&update) {
            self.rejected.insert(*id, format!("crdt: {e}"));
            return Err(NodeError::Change(ChangeError::Malformed));
        }
        self.rejected.remove(id);
        self.applied.insert(*id);
        // Everything this change carried is now in our log, so it must not be
        // re-emitted inside our own next change.
        let sv = self
            .docs
            .get(&env.doc_id)
            .map(|d| d.state_vector())
            .unwrap_or_default();
        self.watermarks.insert(env.doc_id, sv);
        Ok(true)
    }

    /// Import membership entries in dependency order. Entries whose parents have
    /// not arrived yet are retried, so a peer's log is never silently truncated by
    /// `BTreeMap` iteration order.
    fn import_membership(&mut self, entries: Vec<MembershipEntry>) -> usize {
        let mut inserted = 0usize;
        let mut pending = entries;
        loop {
            let before = self.log.len();
            let mut retry = Vec::new();
            for e in pending.drain(..) {
                match self.log.insert(self.workspace_id, e.clone()) {
                    Ok(_) => inserted += 1,
                    Err(MembershipError::MissingParent(_)) => retry.push(e),
                    Err(_) => {} // invalid entries are dropped, never trusted
                }
            }
            if retry.is_empty() || self.log.len() == before {
                break;
            }
            pending = retry;
        }
        inserted
    }

    /// Anti-entropy with another node: exchange whatever each side is missing.
    /// Returns (changes received, changes sent).
    pub fn sync_with(&mut self, other: &mut Node) -> Result<(usize, usize)> {
        // 0. Never mix two different workspaces, even if a caller asks us to.
        if other.workspace_id != self.workspace_id {
            return Err(NodeError::WorkspaceMismatch {
                mine: self.workspace_id,
                theirs: other.workspace_id,
            });
        }
        // 1. Exchange membership entries so authorisation decisions agree.
        //    Imported in dependency order: a child never outruns its parent.
        let mine: BTreeSet<ChangeId> = self.log.ids().into_iter().collect();
        let theirs: BTreeSet<ChangeId> = other.log.ids().into_iter().collect();
        let for_me: Vec<MembershipEntry> = other
            .log
            .entries()
            .filter(|e| !mine.contains(&e.id()))
            .cloned()
            .collect();
        let for_other: Vec<MembershipEntry> = self
            .log
            .entries()
            .filter(|e| !theirs.contains(&e.id()))
            .cloned()
            .collect();
        let got_mem = self.import_membership(for_me);
        other.import_membership(for_other);
        // 2. Exchange change envelopes: only what the peer lacks.
        let my_ids: BTreeSet<ChangeId> = self.changes.keys().copied().collect();
        let their_ids: BTreeSet<ChangeId> = other.changes.keys().copied().collect();
        let to_other: Vec<ChangeEnvelope> = self.changes.iter()
            .filter(|(id, _)| !their_ids.contains(id))
            .map(|(_, e)| e.clone()).collect();
        let to_me: Vec<ChangeEnvelope> = other.changes.iter()
            .filter(|(id, _)| !my_ids.contains(id))
            .map(|(_, e)| e.clone()).collect();
        let sent = to_other.len();
        for env in to_other { let _ = other.ingest(env); }
        for env in to_me { let _ = self.ingest(env); }
        let _ = got_mem;
        // 3. Reconcile both sides to a fixpoint. A change can arrive before the
        //    membership entry or epoch key that authorises it, on either side, so
        //    retrying only the caller would leave the peer holding envelopes it
        //    never applies. Keep going while either side makes progress.
        for _ in 0..8 {
            let before = (self.applied_count(), other.applied_count());
            let ids: Vec<ChangeId> = self.changes.keys().copied().collect();
            for id in ids { let _ = self.try_apply(&id); }
            let ids: Vec<ChangeId> = other.changes.keys().copied().collect();
            for id in ids { let _ = other.try_apply(&id); }
            if (self.applied_count(), other.applied_count()) == before { break; }
        }
        Ok((other.change_count(), sent))
    }

    // --------------------------------------------------------------- bulk export

    /// Envelopes the peer does not already have, capped at `limit`.
    pub fn envelopes_not_in(
        &self,
        known: &BTreeSet<ChangeId>,
        limit: usize,
    ) -> Vec<ChangeEnvelope> {
        self.changes
            .iter()
            .filter(|(id, _)| !known.contains(id))
            .map(|(_, e)| e.clone())
            .take(limit)
            .collect()
    }

    /// Membership entries the peer does not already have, capped at `limit`.
    pub fn membership_not_in(
        &self,
        known: &BTreeSet<ChangeId>,
        limit: usize,
    ) -> Vec<MembershipEntry> {
        self.log
            .entries()
            .filter(|e| !known.contains(&e.id()))
            .cloned()
            .take(limit)
            .collect()
    }

    pub fn has_change(&self, id: &ChangeId) -> bool { self.changes.contains_key(id) }

    /// Total bytes of ciphertext we are holding (for quota reporting).
    pub fn stored_bytes(&self) -> usize {
        self.changes.values().map(|e| e.approx_len()).sum()
    }

    // ------------------------------------------------------------------ persistence

    /// Everything needed to reconstruct this node, minus the identity secrets
    /// (which the store encrypts separately).
    pub fn export_state(&self) -> PersistState {
        PersistState {
            workspace_id: self.workspace_id,
            entries: self.log.entries().cloned().collect(),
            changes: self.changes.values().map(|e| e.encode()).collect(),
            seq: self.seq,
        }
    }

    /// Rebuild a node from persisted state. Changes are re-ingested, so anything
    /// that was pending stays pending rather than being silently trusted.
    pub fn import_state(identity: Identity, state: PersistState) -> Result<Self> {
        let invite = Invite { workspace_id: state.workspace_id, entries: state.entries };
        let mut node = Node::join_from_invite(identity, &invite)?;
        node.seq = state.seq;
        for bytes in state.changes {
            if let Ok(env) = ChangeEnvelope::decode(&bytes) {
                let _ = node.ingest(env);
            }
        }
        node.retry_pending();
        Ok(node)
    }

    /// Retry all pending changes (e.g. after a key rotation arrives).
    pub fn retry_pending(&mut self) -> usize {
        let ids: Vec<ChangeId> = self.changes.keys().copied().collect();
        let mut n = 0;
        for id in ids { if matches!(self.try_apply(&id), Ok(true)) { n += 1; } }
        n
    }

    // ------------------------------------------------------------------ rendering

    /// Deterministic whole-workspace rendering. Two converged nodes produce
    /// byte-identical output, which is what the convergence tests assert on.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "workspace {} \"{}\" epoch={} members={} docs={} changes={} pending={}\n",
            self.workspace_id, self.workspace_name(), self.epoch(),
            self.members().len(), self.docs.len(), self.changes.len(), self.pending_count()
        ));
        for (did, doc) in &self.docs {
            s.push_str(&format!("  -- doc {} --\n", &did.to_hex()[..8]));
            for line in doc.render().lines() {
                s.push_str("    ");
                s.push_str(line);
                s.push('\n');
            }
        }
        s
    }

    /// Compact summary used by the CLI list view.
    pub fn summary(&self) -> String {
        format!(
            "{} | \"{}\" | {} docs | {} members | {} changes",
            &self.workspace_id.to_hex()[..8],
            self.workspace_name(),
            self.docs.len(),
            self.members().len(),
            self.changes.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_nodes() -> (Node, Node) {
        let alice = Node::create_workspace(Identity::generate(), "Quake Response").unwrap();
        let bob_id = Identity::generate();
        let mut alice = alice;
        let invite = alice.invite_for(&bob_id.public()).unwrap();
        let bob = Node::join_from_invite(bob_id, &invite).unwrap();
        (alice, bob)
    }

    #[test]
    fn create_and_join_recovers_key_from_grant() {
        let (alice, bob) = two_nodes();
        assert_eq!(alice.workspace_id, bob.workspace_id);
        assert_eq!(alice.workspace_name(), "Quake Response");
        assert!(bob.members().contains(&alice.peer_id()));
        assert!(bob.members().contains(&bob.peer_id()));
        assert_eq!(bob.epoch(), alice.epoch());
    }

    #[test]
    fn invite_token_roundtrip() {
        let (alice, _bob) = two_nodes();
        let invite = Invite { workspace_id: alice.workspace_id, entries: alice.log.entries().cloned().collect() };
        let tok = invite.to_token();
        let back = Invite::from_token(&tok).unwrap();
        assert_eq!(back.workspace_id, alice.workspace_id);
        assert_eq!(back.entries.len(), invite.entries.len());
    }

    #[test]
    fn edits_sync_between_peers() {
        let (mut alice, mut bob) = two_nodes();
        let did = alice.create_doc(DocType::Checklist, "Casualties").unwrap();
        let (_cid, item) = alice.add_checklist_item_id(did, "water for 20").unwrap();
        alice.sync_with(&mut bob).unwrap();
        let bdoc = bob.doc(&did).expect("bob received the document");
        assert_eq!(bdoc.iter_items().len(), 1);
        assert_eq!(bdoc.iter_items()[0].text, "water for 20");

        // Bob edits, syncs back.
        bob.toggle_checklist_item(did, &item, true).unwrap();
        bob.sync_with(&mut alice).unwrap();
        let adoc = alice.doc(&did).unwrap();
        assert_eq!(adoc.checklist_progress(), (1, 1));
        assert_eq!(alice.render(), bob.render());
    }

    #[test]
    fn concurrent_edits_converge() {
        let (mut alice, mut bob) = two_nodes();
        let did = alice.create_doc(DocType::Checklist, "Supplies").unwrap();
        alice.sync_with(&mut bob).unwrap();

        // Both edit while disconnected.
        alice.add_checklist_item(did, "blankets").unwrap();
        bob.add_checklist_item(did, "radio").unwrap();

        // Reconnect and converge.
        alice.sync_with(&mut bob).unwrap();
        bob.sync_with(&mut alice).unwrap();

        assert_eq!(alice.render(), bob.render());
        assert_eq!(alice.doc(&did).unwrap().iter_items().len(), 2);
    }

    #[test]
    fn sync_is_idempotent() {
        let (mut alice, mut bob) = two_nodes();
        let did = alice.create_doc(DocType::Note, "Sitrep").unwrap();
        alice.set_note(did, "all clear").unwrap();
        alice.sync_with(&mut bob).unwrap();
        let n1 = bob.change_count();
        alice.sync_with(&mut bob).unwrap();
        bob.sync_with(&mut alice).unwrap();
        assert_eq!(bob.change_count(), n1);
        assert_eq!(alice.render(), bob.render());
    }

    #[test]
    fn three_nodes_propagate_through_gossip() {
        // A meets B, B meets C: C must end up with A's changes (epidemic sync).
        let (mut a, mut b) = two_nodes();
        let c_id = Identity::generate();
        let invite = b.invite_for(&c_id.public()).unwrap();
        let mut c = Node::join_from_invite(c_id, &invite).unwrap();

        let did = a.create_doc(DocType::GeoPoint, "Water point").unwrap();
        a.set_location(did, 27.7, 85.3, "spring", "water").unwrap();

        a.sync_with(&mut b).unwrap();   // A -> B
        b.sync_with(&mut c).unwrap();   // B -> C  (C never met A)
        assert!(c.doc(&did).is_some());
        assert!((c.doc(&did).unwrap().location().lat - 27.7).abs() < 1e-9);
    }

    #[test]
    fn outsider_cannot_inject_a_change() {
        let (mut alice, mut bob) = two_nodes();
        let did = alice.create_doc(DocType::Note, "n").unwrap();
        alice.set_note(did, "legit").unwrap();
        alice.sync_with(&mut bob).unwrap();

        // Eve knows the workspace id and even the key, but is not in the log.
        let eve = Identity::generate();
        let eve_env = ChangeEnvelope::create(
            &eve, alice.workspace_id, did, Epoch::GENESIS, 99,
            &alice.key(Epoch::GENESIS).unwrap().clone(),
            b"forged",
        ).unwrap();
        let before = bob.change_count();
        bob.ingest(eve_env).unwrap();
        // Envelope is retained but never applied.
        assert_eq!(bob.change_count(), before + 1);
        assert_eq!(bob.doc(&did).unwrap().text(), "legit");
    }
}
