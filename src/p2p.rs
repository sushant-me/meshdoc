//! Wi-Fi Direct group formation: role election and the pre-sync handshake.
//!
//! Android's Wi-Fi Direct API decides who becomes the group owner from a
//! *group owner intent* value each side supplies. If both sides ask for 15 or both
//! ask for 0, negotiation can stall or pick an arbitrary side, so the choice must be
//! taken out of the platform's hands and derived from something both peers already
//! agree on.
//!
//! # Role election
//!
//! The peer with the **lexicographically smaller identity fingerprint** requests
//! owner intent 15; the other requests 0. Both sides compute this from the same two
//! fingerprints, so they always ask for opposite roles — no negotiation loop, no
//! tie, and the result is stable across retries.
//!
//! # Why a handshake, not just "connect and sync"
//!
//! The group owner's socket is reachable by *any device that joins the group*,
//! including one that never held the workspace key. Ciphertext is useless to it, but
//! it could still probe, and it would learn timing and sizes. So before any sync
//! frame is exchanged, each side proves possession of a workspace identity key and
//! membership of the workspace; anything else is dropped.
//!
//! ```text
//! client                                   group owner (listener)
//!   |-- Hello{v, ws, pubkey, nonce, sig} ------->|
//!   |                                  verify sig over (ws || nonce)
//!   |                                  verify pubkey is a current member
//!   |<-- Hello{v, ws, pubkey, nonce, sig} --------|
//!   |-- Ok -------------------------------------->|
//!   |                  sync protocol runs here     |
//! ```

use std::collections::BTreeSet;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::crypto::{CryptoError, Identity, PublicIdentity};
use crate::ids::{PeerId, WorkspaceId};
use crate::node::Node;
use crate::transport::{read_frame, write_frame, TransportError};

/// Default TCP port the group owner listens on once the group exists.
pub const P2P_PORT: u16 = 7777;

pub const PROTOCOL_VERSION: u16 = 1;

/// Which side of a Wi-Fi Direct group a device should take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupRole {
    /// The group owner: it provides the access point and listens for peers.
    Owner,
    /// A group client: it connects to the owner's address.
    Client,
}

impl GroupRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            GroupRole::Owner => "owner",
            GroupRole::Client => "client",
        }
    }

    /// The `groupOwnerIntent` value to hand to Android's `WifiP2pConfig`.
    pub fn group_owner_intent(self) -> i32 {
        match self {
            GroupRole::Owner => 15,
            GroupRole::Client => 0,
        }
    }

    pub fn is_owner(self) -> bool { matches!(self, GroupRole::Owner) }
}

/// Deterministic, symmetric role election.
///
/// Both peers must reach opposite answers from the same inputs. Ordering the two
/// fingerprints gives that by construction: the smaller one owns.
pub fn elect_role(mine: PeerId, theirs: PeerId) -> GroupRole {
    if mine.as_bytes() < theirs.as_bytes() {
        GroupRole::Owner
    } else {
        GroupRole::Client
    }
}

/// Address the client should dial once the group is up. Android reports the group
/// owner address on most stacks; 192.168.49.1 is the conventional fallback.
pub const DEFAULT_OWNER_ADDRESS: &str = "192.168.49.1";

#[derive(Debug)]
pub enum P2pError {
    Io(String),
    Transport(TransportError),
    Protocol(String),
    BadSignature,
    NotAMember(PeerId),
    WrongWorkspace,
    VersionMismatch(u16),
}

impl std::fmt::Display for P2pError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            P2pError::Io(e) => write!(f, "p2p i/o: {e}"),
            P2pError::Transport(e) => write!(f, "{e}"),
            P2pError::Protocol(m) => write!(f, "p2p protocol: {m}"),
            P2pError::BadSignature => write!(f, "peer failed to prove possession of its key"),
            P2pError::NotAMember(p) => write!(f, "peer {} is not a member", &p.to_hex()[..8]),
            P2pError::WrongWorkspace => write!(f, "peer is in a different workspace"),
            P2pError::VersionMismatch(v) => write!(f, "peer speaks p2p v{v}"),
        }
    }
}
impl std::error::Error for P2pError {}
impl From<TransportError> for P2pError { fn from(e: TransportError) -> Self { P2pError::Transport(e) } }
impl From<CryptoError> for P2pError { fn from(_: CryptoError) -> Self { P2pError::BadSignature } }

/// Identity proof exchanged before any sync traffic.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct P2pHello {
    pub version: u16,
    pub workspace_id: WorkspaceId,
    pub peer: PublicIdentity,
    /// Random per-connection value; binds the signature to this session.
    pub nonce: [u8; 16],
    pub signature: Vec<u8>,
}

impl P2pHello {
    fn message(workspace_id: &WorkspaceId, nonce: &[u8; 16]) -> Vec<u8> {
        let mut m = Vec::with_capacity(48);
        m.extend_from_slice(b"meshdoc-p2p-hello-v1");
        m.extend_from_slice(workspace_id.as_bytes());
        m.extend_from_slice(nonce);
        m
    }

    pub fn create(identity: &Identity, workspace_id: WorkspaceId) -> Self {
        use rand::RngCore;
        let mut nonce = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let sig = identity.sign(&Self::message(&workspace_id, &nonce));
        Self {
            version: PROTOCOL_VERSION,
            workspace_id,
            peer: identity.public(),
            nonce,
            signature: sig.to_vec(),
        }
    }

    /// Verify the peer's signature, workspace and membership.
    pub fn verify(&self, expected_ws: WorkspaceId, members: &BTreeSet<PeerId>) -> Result<PeerId, P2pError> {
        if self.version != PROTOCOL_VERSION {
            return Err(P2pError::VersionMismatch(self.version));
        }
        if self.workspace_id != expected_ws {
            return Err(P2pError::WrongWorkspace);
        }
        self.peer
            .verify(&Self::message(&self.workspace_id, &self.nonce), &self.signature)
            .map_err(|_| P2pError::BadSignature)?;
        let fp = self.peer.fingerprint();
        if !members.contains(&fp) {
            return Err(P2pError::NotAMember(fp));
        }
        Ok(fp)
    }
}

/// Run the client half of the handshake. Returns the authenticated peer.
pub fn handshake_client<R: Read, W: Write>(rx: &mut R, tx: &mut W, node: &Node) -> Result<PeerId, P2pError> {
    let mine = P2pHello::create(&node.identity, node.workspace_id);
    write_frame(tx, &bincode::serialize(&mine).map_err(|_| P2pError::Protocol("encode".into()))?)?;

    let frame = read_frame(rx)?;
    let hello: P2pHello = bincode::deserialize(&frame)
        .map_err(|_| P2pError::Protocol("undecodable hello".into()))?;
    // Validate against the caller's own membership view, not the peer's claim.
    hello.verify(node.workspace_id, &node.members())?;

    // Acknowledge so the server knows we accepted its identity too.
    write_frame(tx, b"OK")?;
    Ok(hello.peer.fingerprint())
}

/// Run the server half of the handshake. Returns the authenticated peer.
pub fn handshake_serve<R: Read, W: Write>(rx: &mut R, tx: &mut W, node: &Node) -> Result<PeerId, P2pError> {
    let frame = read_frame(rx)?;
    let hello: P2pHello = bincode::deserialize(&frame)
        .map_err(|_| P2pError::Protocol("undecodable hello".into()))?;
    let peer = hello.verify(node.workspace_id, &node.members())?;

    let mine = P2pHello::create(&node.identity, node.workspace_id);
    write_frame(tx, &bincode::serialize(&mine).map_err(|_| P2pError::Protocol("encode".into()))?)?;

    let ack = read_frame(rx)?;
    if ack != b"OK" {
        return Err(P2pError::Protocol("peer rejected our identity".into()));
    }
    Ok(peer)
}

/// One complete client session over an already-formed Wi-Fi Direct link:
/// authenticate, then pull changes.
///
/// The caller supplies separate read and write halves (e.g. `TcpStream::try_clone`)
/// because the handshake and the sync protocol both read and write.
pub fn session_client<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    node: &mut Node,
) -> Result<crate::sync::SyncStats, P2pError> {
    handshake_client(rx, tx, node)?;
    crate::sync::pull(rx, tx, node).map_err(|e| P2pError::Protocol(e.to_string()))
}

/// One complete server session over an already-formed Wi-Fi Direct link:
/// authenticate, then serve changes.
pub fn session_serve<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    node: &mut Node,
) -> Result<crate::sync::SyncStats, P2pError> {
    handshake_serve(rx, tx, node)?;
    crate::sync::serve(rx, tx, node).map_err(|e| P2pError::Protocol(e.to_string()))
}

/// State of the radio link, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum P2pState {
    /// No group, not scanning.
    Idle,
    /// Advertising over BLE and/or scanning for the rotating identifier.
    Discovering { peers: usize },
    /// A peer was chosen; the platform is forming the group.
    Forming { role: GroupRole },
    /// The group exists and a socket is connected and authenticated.
    Connected { peer: PeerId, role: GroupRole },
    /// Sync is running.
    Syncing,
    /// Finished or failed; `reason` explains which.
    Finished { ok: bool, reason: String },
}

impl P2pState {
    pub fn is_idle(&self) -> bool { matches!(self, P2pState::Idle) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::DocType;
    use crate::ids::WorkspaceId;
    use std::os::unix::net::UnixStream;
    use std::thread::scope;

    fn ids(seed: u8) -> PeerId {
        let mut b = [0u8; 16];
        b[0] = seed;
        PeerId(b)
    }

    #[test]
    fn role_election_is_symmetric_and_opposite() {
        let a = ids(1);
        let b = ids(2);
        assert_eq!(elect_role(a, b), GroupRole::Owner);
        assert_eq!(elect_role(b, a), GroupRole::Client);
        // And the two answers are always opposite, for any pair.
        for x in 0u8..8 {
            for y in 0u8..8 {
                if x == y { continue; }
                assert_ne!(elect_role(ids(x), ids(y)), elect_role(ids(y), ids(x)));
            }
        }
    }

    #[test]
    fn owner_intent_matches_role() {
        assert_eq!(GroupRole::Owner.group_owner_intent(), 15);
        assert_eq!(GroupRole::Client.group_owner_intent(), 0);
        assert!(GroupRole::Owner.is_owner());
    }

    fn pair() -> (Node, Node) {
        let mut alice = Node::create_workspace(Identity::generate(), "ws").unwrap();
        let bob_id = Identity::generate();
        let inv = alice.invite_for(&bob_id.public()).unwrap();
        let bob = Node::join_from_invite(bob_id, &inv).unwrap();
        (alice, bob)
    }

    #[test]
    fn handshake_authenticates_both_sides() {
        let (mut a, mut b) = pair();
        let _ = a.create_doc(DocType::Note, "hello").unwrap();
        let (a_expect, b_expect) = (a.peer_id(), b.peer_id());
        let (c1, c2) = UnixStream::pair().unwrap();
        let mut r1 = c1.try_clone().unwrap();
        let mut w1 = c1;
        let mut r2 = c2.try_clone().unwrap();
        let mut w2 = c2;
        scope(|s| {
            let ar = &a;
            let br = &b;
            let server = s.spawn(move || handshake_serve(&mut r1, &mut w1, ar));
            let client = handshake_client(&mut r2, &mut w2, br);
            let served = server.join().unwrap();
            let c_peer = client.expect("client handshake");
            let s_peer = served.expect("server handshake");
            assert_eq!(c_peer, a_expect);
            assert_eq!(s_peer, b_expect);
        });
    }

    #[test]
    fn outsider_holding_the_workspace_key_is_refused() {
        let (a, _b) = pair();
        let ws = a.workspace_id;
        // Eve has the workspace key (worst case) but no membership entry.
        let eve = Identity::generate();
        let eve_fp = eve.fingerprint();
        let (c1, c2) = UnixStream::pair().unwrap();
        let mut r1 = c1.try_clone().unwrap();
        let mut w1 = c1;
        let mut r2 = c2.try_clone().unwrap();
        let mut w2 = c2;
        scope(|s| {
            let ar = &a;
            let server = s.spawn(move || handshake_serve(&mut r1, &mut w1, ar));
            let eve_hello = P2pHello::create(&eve, ws);
            write_frame(&mut w2, &bincode::serialize(&eve_hello).unwrap()).unwrap();
            let _ = read_frame(&mut r2); // server replies, we ignore it
            let served = server.join().unwrap();
            assert!(matches!(served, Err(P2pError::NotAMember(p)) if p == eve_fp));
        });
    }

    #[test]
    fn hello_rejects_wrong_workspace_and_bad_signature() {
        let (a, _b) = pair();
        let members = a.members();
        let other_ws = WorkspaceId::random();
        let hello = P2pHello::create(&a.identity, other_ws);
        assert!(matches!(hello.verify(a.workspace_id, &members), Err(P2pError::WrongWorkspace)));

        let mut tampered = P2pHello::create(&a.identity, a.workspace_id);
        tampered.nonce[0] ^= 0xff;
        assert!(matches!(tampered.verify(a.workspace_id, &members), Err(P2pError::BadSignature)));
    }

    #[test]
    fn state_machine_reaches_connected_and_finished() {
        let s = P2pState::Idle;
        assert!(s.is_idle());
        let s = P2pState::Discovering { peers: 2 };
        assert!(!s.is_idle());
        let s = P2pState::Forming { role: GroupRole::Owner };
        assert!(matches!(s, P2pState::Forming { role } if role.is_owner()));
        let s = P2pState::Connected { peer: ids(3), role: GroupRole::Client };
        assert!(matches!(s, P2pState::Connected { .. }));
        let s = P2pState::Finished { ok: true, reason: "converged".into() };
        assert!(matches!(s, P2pState::Finished { ok: true, .. }));
    }

    #[test]
    fn default_owner_address_is_the_android_convention() {
        assert_eq!(DEFAULT_OWNER_ADDRESS, "192.168.49.1");
        assert_eq!(P2P_PORT, 7777);
    }
}
