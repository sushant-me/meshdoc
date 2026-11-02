//! Signed, append-only membership log with key-rotation epochs.
//!
//! Membership is **not** merely "you have the workspace key": possession is how you
//! read, but authorisation is decided by this log. A change is accepted only if its
//! author appears in the log at the epoch the change claims, as observed causally by
//! the receiver.
//!
//! The log is a hash chain. Each entry names its parent, so entries can be exchanged
//! and replayed in any order and verified independently. Concurrent entries (two
//! members adding someone at once) are permitted; the log is a DAG, not a line.
//!
//! What key rotation does and does not do:
//! * It stops a removed member from reading **new** content written under a later
//!   epoch.
//! * It cannot un-read anything they already decrypted, and a partitioned member
//!   keeps using their old epoch until they observe the rotation.
//! Both limitations are real and are documented rather than papered over.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::crypto::{CryptoError, Identity, PublicIdentity, WorkspaceKey};
use crate::ids::{ChangeId, Epoch, PeerId, WorkspaceId};

/// What a membership entry does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MembershipAction {
    /// Genesis: establishes the workspace and its founding member.
    Found { workspace_id: WorkspaceId, name: String },
    /// Add a member, sealing the epoch key to their X25519 key.
    Add {
        member: PublicIdentity,
        /// WorkspaceKey for `epoch`, sealed to `member` (see crypto::seal_key_grant).
        grant: Vec<u8>,
    },
    /// Remove a member and advance to `epoch`.
    Remove { member: PeerId, epoch: Epoch },
    /// Re-share the key for `epoch` to an existing member (e.g. after rotation).
    Regrant { member: PublicIdentity, grant: Vec<u8>, epoch: Epoch },
}

/// One signed entry in the membership log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipEntry {
    pub workspace_id: WorkspaceId,
    /// Epoch this entry takes effect in.
    pub epoch: Epoch,
    pub action: MembershipAction,
    pub author: PublicIdentity,
    /// Hash of the entry(s) this one extends. Empty for genesis.
    pub parents: Vec<ChangeId>,
    /// Monotonic sequence number chosen by the author (used only for ordering ties).
    pub seq: u64,
    pub signature: Vec<u8>,
}

impl MembershipEntry {
    /// Bytes covered by the signature (everything except the signature itself).
    fn signing_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.signature.clear();
        serde_json::to_vec(&clone).expect("membership entry serialises")
    }

    pub fn verify(&self) -> Result<(), CryptoError> {
        let msg = self.signing_bytes();
        self.author.verify(&msg, &self.signature)
    }

    /// Content address of this entry.
    pub fn id(&self) -> ChangeId {
        let h = blake3::hash(&serde_json::to_vec(self).expect("serialises"));
        let mut out = [0u8; 16];
        out.copy_from_slice(&h.as_bytes()[..16]);
        ChangeId(out)
    }
}

/// Builder for signed entries, so callers cannot forget to sign.
pub struct EntryBuilder<'a> {
    id: &'a Identity,
}

impl<'a> EntryBuilder<'a> {
    pub fn new(id: &'a Identity) -> Self { Self { id } }

    fn sign(&self, mut e: MembershipEntry) -> MembershipEntry {
        let msg = e.signing_bytes();
        e.signature = self.id.sign(&msg).to_vec();
        e
    }

    pub fn found(&self, workspace_id: WorkspaceId, name: &str) -> MembershipEntry {
        self.sign(MembershipEntry {
            workspace_id,
            epoch: Epoch::GENESIS,
            action: MembershipAction::Found {
                workspace_id,
                name: name.to_string(),
            },
            author: self.id.public(),
            parents: vec![],
            seq: 0,
            signature: vec![],
        })
    }

    pub fn add(&self, ws: WorkspaceId, epoch: Epoch, member: PublicIdentity,
               grant: Vec<u8>, parents: Vec<ChangeId>, seq: u64) -> MembershipEntry {
        self.sign(MembershipEntry {
            workspace_id: ws, epoch, action: MembershipAction::Add { member, grant },
            author: self.id.public(), parents, seq, signature: vec![],
        })
    }

    pub fn remove(&self, ws: WorkspaceId, new_epoch: Epoch, member: PeerId,
                  parents: Vec<ChangeId>, seq: u64) -> MembershipEntry {
        self.sign(MembershipEntry {
            workspace_id: ws, epoch: new_epoch,
            action: MembershipAction::Remove { member, epoch: new_epoch },
            author: self.id.public(), parents, seq, signature: vec![],
        })
    }

    pub fn regrant(&self, ws: WorkspaceId, epoch: Epoch, member: PublicIdentity,
                   grant: Vec<u8>, parents: Vec<ChangeId>, seq: u64) -> MembershipEntry {
        self.sign(MembershipEntry {
            workspace_id: ws, epoch,
            action: MembershipAction::Regrant { member, grant, epoch },
            author: self.id.public(), parents, seq, signature: vec![],
        })
    }
}

/// The verified view of a workspace's membership.
#[derive(Clone, Debug, Default)]
pub struct MembershipLog {
    entries: BTreeMap<ChangeId, MembershipEntry>,
    /// Highest epoch observed.
    head_epoch: Epoch,
}

impl MembershipLog {
    pub fn new() -> Self {
        Self { entries: BTreeMap::new(), head_epoch: Epoch::GENESIS }
    }

    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
    pub fn head_epoch(&self) -> Epoch { self.head_epoch }

    pub fn contains(&self, id: &ChangeId) -> bool { self.entries.contains_key(id) }

    /// All entry ids, for anti-entropy exchange.
    pub fn ids(&self) -> Vec<ChangeId> { self.entries.keys().copied().collect() }

    pub fn entries(&self) -> impl Iterator<Item = &MembershipEntry> {
        self.entries.values()
    }

    /// Verify and insert an entry. Returns true if newly inserted.
    ///
    /// Rejects: bad signatures, wrong workspace, unknown parents, and entries whose
    /// author is not currently a member (except genesis).
    pub fn insert(&mut self, ws: WorkspaceId, entry: MembershipEntry) -> Result<bool, MembershipError> {
        if entry.workspace_id != ws {
            return Err(MembershipError::WrongWorkspace);
        }
        let id = entry.id();
        if self.entries.contains_key(&id) {
            return Ok(false);
        }
        entry.verify().map_err(|_| MembershipError::BadSignature)?;

        // Parents must be present unless this is genesis.
        let is_genesis = matches!(entry.action, MembershipAction::Found { .. });
        for p in &entry.parents {
            if !self.entries.contains_key(p) {
                return Err(MembershipError::MissingParent(*p));
            }
        }
        if !is_genesis && entry.parents.is_empty() {
            return Err(MembershipError::NoParents);
        }

        // Authorisation: the author must be a member at or before the entry's epoch.
        if !is_genesis {
            let author = entry.author.fingerprint();
            if !self.is_member_at(author, entry.epoch) {
                return Err(MembershipError::NotAMember(author));
            }
        }

        self.head_epoch = self.head_epoch.max(entry.epoch);
        self.entries.insert(id, entry);
        Ok(true)
    }

    /// Entries that take effect at or before `epoch`, in a deterministic order:
    /// by epoch, then additions before removals within the same epoch, then by
    /// content address. Without this, membership would depend on `BTreeMap`
    /// iteration order, and a `Remove` could be applied before its `Add`.
    fn ordered_until(&self, epoch: Epoch) -> Vec<&MembershipEntry> {
        fn precedence(a: &MembershipAction) -> u8 {
            match a {
                MembershipAction::Remove { .. } => 1, // removals last within an epoch
                _ => 0,
            }
        }
        let mut v: Vec<&MembershipEntry> =
            self.entries.values().filter(|e| e.epoch <= epoch).collect();
        v.sort_by(|a, b| {
            a.epoch
                .cmp(&b.epoch)
                .then_with(|| precedence(&a.action).cmp(&precedence(&b.action)))
                .then_with(|| a.id().cmp(&b.id()))
        });
        v
    }

    /// Is `who` a member as of `epoch`? Evaluated over observed entries only —
    /// this is a local, causal view, which is exactly the honest guarantee.
    pub fn is_member_at(&self, who: PeerId, epoch: Epoch) -> bool {
        self.members_at(epoch).contains(&who)
    }

    pub fn members_at(&self, epoch: Epoch) -> BTreeSet<PeerId> {
        let mut members: BTreeSet<PeerId> = BTreeSet::new();
        for e in self.ordered_until(epoch) {
            match &e.action {
                MembershipAction::Found { .. } => { members.insert(e.author.fingerprint()); }
                MembershipAction::Add { member, .. } => { members.insert(member.fingerprint()); }
                MembershipAction::Remove { member, .. } => { members.remove(member); }
                MembershipAction::Regrant { member, .. } => { members.insert(member.fingerprint()); }
            }
        }
        members
    }

    /// Current members (highest epoch seen).
    pub fn members(&self) -> BTreeSet<PeerId> { self.members_at(self.head_epoch) }

    /// Recover the workspace key for `epoch` addressed to `me`, by scanning grants.
    pub fn key_for(&self, me: &Identity, epoch: Epoch) -> Option<WorkspaceKey> {
        let mine = me.fingerprint();
        // Latest grant addressed to us at or before `epoch` wins.
        let mut best: Option<(&Epoch, &Vec<u8>)> = None;
        for e in self.entries.values() {
            let (member, grant) = match &e.action {
                MembershipAction::Add { member, grant } => (*member, grant),
                MembershipAction::Regrant { member, grant, .. } => (*member, grant),
                _ => continue,
            };
            if member.fingerprint() != mine || e.epoch > epoch { continue; }
            if best.map_or(true, |(be, _)| e.epoch >= *be) {
                best = Some((&e.epoch, grant));
            }
        }
        let (_, grant) = best?;
        crate::crypto::open_key_grant(me, grant).ok()
    }

    /// The workspace name from the genesis entry.
    pub fn name(&self) -> Option<String> {
        self.entries.values().find_map(|e| match &e.action {
            MembershipAction::Found { name, .. } => Some(name.clone()),
            _ => None,
        })
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.entries.values().next().map(|e| e.workspace_id)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum MembershipError {
    WrongWorkspace,
    BadSignature,
    MissingParent(ChangeId),
    NoParents,
    NotAMember(PeerId),
}

impl std::fmt::Display for MembershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MembershipError::WrongWorkspace => write!(f, "entry belongs to a different workspace"),
            MembershipError::BadSignature => write!(f, "bad signature"),
            MembershipError::MissingParent(id) => write!(f, "missing parent {}", &id.to_hex()[..8]),
            MembershipError::NoParents => write!(f, "non-genesis entry has no parents"),
            MembershipError::NotAMember(p) => write!(f, "author {} is not a member at this epoch", &p.to_hex()[..8]),
        }
    }
}
impl std::error::Error for MembershipError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_key_grant;

    fn setup() -> (WorkspaceId, Identity, MembershipLog, MembershipEntry) {
        let ws = WorkspaceId::random();
        let alice = Identity::generate();
        let genesis = EntryBuilder::new(&alice).found(ws, "Quake Response");
        let mut log = MembershipLog::new();
        log.insert(ws, genesis.clone()).unwrap();
        (ws, alice, log, genesis)
    }

    #[test]
    fn genesis_establishes_workspace_and_founder() {
        let (ws, alice, log, _) = setup();
        assert_eq!(log.name().as_deref(), Some("Quake Response"));
        assert_eq!(log.workspace_id(), Some(ws));
        assert!(log.is_member_at(alice.fingerprint(), Epoch::GENESIS));
    }

    #[test]
    fn add_member_and_recover_key_from_grant() {
        let (ws, alice, mut log, genesis) = setup();
        let bob = Identity::generate();
        let wk = WorkspaceKey::generate();
        let grant = seal_key_grant(&bob.public(), &wk).unwrap();
        let entry = EntryBuilder::new(&alice).add(
            ws, Epoch::GENESIS, bob.public(), grant, vec![genesis.id()], 1,
        );
        assert!(log.insert(ws, entry).unwrap());
        assert!(log.is_member_at(bob.fingerprint(), Epoch::GENESIS));
        // Bob can recover the workspace key from the log alone.
        let recovered = log.key_for(&bob, Epoch::GENESIS).unwrap();
        assert_eq!(recovered, wk);
        // A stranger cannot.
        let eve = Identity::generate();
        assert!(log.key_for(&eve, Epoch::GENESIS).is_none());
    }

    #[test]
    fn removal_revokes_membership_at_later_epoch() {
        let (ws, alice, mut log, genesis) = setup();
        let bob = Identity::generate();
        let wk = WorkspaceKey::generate();
        let add = EntryBuilder::new(&alice).add(
            ws, Epoch::GENESIS, bob.public(), seal_key_grant(&bob.public(), &wk).unwrap(),
            vec![genesis.id()], 1,
        );
        let add_id = add.id();
        log.insert(ws, add).unwrap();
        let rem = EntryBuilder::new(&alice).remove(
            ws, Epoch(1), bob.fingerprint(), vec![add_id], 2,
        );
        log.insert(ws, rem).unwrap();
        assert!(log.is_member_at(bob.fingerprint(), Epoch::GENESIS));
        assert!(!log.is_member_at(bob.fingerprint(), Epoch(1)));
        assert_eq!(log.head_epoch(), Epoch(1));
    }

    #[test]
    fn rejects_forged_signature() {
        let (ws, alice, mut log, genesis) = setup();
        let bob = Identity::generate();
        let wk = WorkspaceKey::generate();
        let mut entry = EntryBuilder::new(&alice).add(
            ws, Epoch::GENESIS, bob.public(), seal_key_grant(&bob.public(), &wk).unwrap(),
            vec![genesis.id()], 1,
        );
        entry.signature[0] ^= 0xff;
        assert_eq!(log.insert(ws, entry), Err(MembershipError::BadSignature));
    }

    #[test]
    fn rejects_non_member_author() {
        let (ws, _alice, mut log, genesis) = setup();
        let mallory = Identity::generate();
        let victim = Identity::generate();
        let wk = WorkspaceKey::generate();
        let forged = EntryBuilder::new(&mallory).add(
            ws, Epoch::GENESIS, victim.public(),
            seal_key_grant(&victim.public(), &wk).unwrap(), vec![genesis.id()], 1,
        );
        assert!(matches!(log.insert(ws, forged), Err(MembershipError::NotAMember(_))));
    }

    #[test]
    fn rejects_missing_parent_and_wrong_workspace() {
        let (ws, alice, mut log, _genesis) = setup();
        let bob = Identity::generate();
        let wk = WorkspaceKey::generate();
        let orphan = EntryBuilder::new(&alice).add(
            ws, Epoch::GENESIS, bob.public(),
            seal_key_grant(&bob.public(), &wk).unwrap(),
            vec![ChangeId([9u8; 16])], 1,
        );
        assert!(matches!(log.insert(ws, orphan), Err(MembershipError::MissingParent(_))));

        let other_ws = WorkspaceId::random();
        let genesis2 = EntryBuilder::new(&alice).found(other_ws, "Other");
        assert_eq!(log.insert(ws, genesis2), Err(MembershipError::WrongWorkspace));
    }

    #[test]
    fn duplicate_entry_is_idempotent() {
        let (ws, _alice, mut log, genesis) = setup();
        assert!(!log.insert(ws, genesis).unwrap());
        assert_eq!(log.len(), 1);
    }
}
