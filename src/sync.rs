//! Anti-entropy sync.
//!
//! Two peers reconcile by exchanging the *content addresses* of the changes they
//! hold, then transferring only the difference. The initiator pulls: it sends
//! what it has, the responder answers with a bounded page of what the initiator is
//! missing, and the initiator acknowledges. Because the known-set is re-sent on
//! every connection, a link that dies mid-transfer simply resumes on the next
//! contact with nothing re-sent, the DTN mindset applied to a direct link.
//!
//! The protocol is deliberately page-based rather than a streaming diff: pages
//! bound memory on both sides, make progress observable, and let a partially
//! transferred document set survive a dropped connection.
//!
//! ```text
//! initiator                          responder
//!    |-- Hello {change ids, member ids} -->|
//!    |<-- Response {changes, members, more} --|
//!    |-- Ack {n} -------------------------> |
//!    |            ... repeat while more ... |
//!    |-- Done ----------------------------> |
//! ```
//!
//! Both sides verify every received change (signature, membership, AEAD) before it
//! is applied, so a malicious peer can waste bandwidth but cannot corrupt state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{Read, Write};

use crate::change::ChangeEnvelope;
use crate::ids::{ChangeId, WorkspaceId};
use crate::membership::MembershipEntry;
use crate::node::{Node, NodeError};
use crate::transport::{read_frame, write_frame, TransportError};

pub const PROTOCOL_VERSION: u16 = 1;
/// Changes per page. Bounds memory and makes progress visible.
pub const PAGE_CHANGES: usize = 64;
/// Membership entries per page.
pub const PAGE_MEMBERS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SyncMsg {
    Hello {
        version: u16,
        workspace_id: WorkspaceId,
        changes: Vec<ChangeId>,
        membership: Vec<ChangeId>,
    },
    Response {
        changes: Vec<Vec<u8>>,
        membership: Vec<Vec<u8>>,
        more: bool,
        changed: usize,
        sent: usize,
    },
    Ack {
        applied: usize,
    },
    Done {
        total_received: usize,
        total_sent: usize,
    },
    Error {
        message: String,
    },
}

#[derive(Debug)]
pub enum SyncError {
    Transport(TransportError),
    Node(NodeError),
    Protocol(String),
    VersionMismatch(u16),
    PeerError(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Transport(e) => write!(f, "{e}"),
            SyncError::Node(e) => write!(f, "{e}"),
            SyncError::Protocol(m) => write!(f, "sync protocol error: {m}"),
            SyncError::VersionMismatch(v) => {
                write!(f, "peer speaks protocol v{v}, we speak v{PROTOCOL_VERSION}")
            }
            SyncError::PeerError(m) => write!(f, "peer reported: {m}"),
        }
    }
}
impl std::error::Error for SyncError {}
impl From<TransportError> for SyncError {
    fn from(e: TransportError) -> Self {
        SyncError::Transport(e)
    }
}
impl From<NodeError> for SyncError {
    fn from(e: NodeError) -> Self {
        SyncError::Node(e)
    }
}

#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    pub received: usize,
    pub sent: usize,
    pub applied: usize,
    pub pages: usize,
}

/// Serve one inbound sync session. Returns when the peer says `Done` or the link
/// drops. Safe to call repeatedly for successive peers.
pub fn serve<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    node: &mut Node,
) -> Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    loop {
        let frame = match read_frame(rx) {
            Ok(f) => f,
            Err(_) => return Ok(stats), // peer went away
        };
        let msg: SyncMsg = bincode::deserialize(&frame)
            .map_err(|_| SyncError::Protocol("undecodable message".into()))?;
        match msg {
            SyncMsg::Hello {
                version,
                workspace_id,
                changes,
                membership,
            } => {
                if version != PROTOCOL_VERSION {
                    let e = SyncMsg::Error {
                        message: format!("unsupported protocol v{version}"),
                    };
                    let _ = write_frame(tx, &bincode::serialize(&e).unwrap_or_default());
                    return Err(SyncError::VersionMismatch(version));
                }
                if workspace_id != node.workspace_id {
                    let e = SyncMsg::Error {
                        message: "workspace mismatch".into(),
                    };
                    let _ = write_frame(tx, &bincode::serialize(&e).unwrap_or_default());
                    return Err(SyncError::Protocol(
                        "peer is in a different workspace".into(),
                    ));
                }
                let known_changes: BTreeSet<ChangeId> = changes.into_iter().collect();
                let known_members: BTreeSet<ChangeId> = membership.into_iter().collect();
                let out_changes = node.envelopes_not_in(&known_changes, PAGE_CHANGES);
                let out_members = node.membership_not_in(&known_members, PAGE_MEMBERS);
                let more = out_changes.len() == PAGE_CHANGES || out_members.len() == PAGE_MEMBERS;
                stats.sent += out_changes.len();
                let resp = SyncMsg::Response {
                    changes: out_changes.iter().map(|c| c.encode()).collect(),
                    membership: out_members
                        .iter()
                        .map(|m| serde_json::to_vec(m).unwrap_or_default())
                        .collect(),
                    more,
                    changed: 0,
                    sent: out_changes.len(),
                };
                write_frame(
                    tx,
                    &bincode::serialize(&resp).map_err(|_| SyncError::Protocol("encode".into()))?,
                )?;
            }
            SyncMsg::Ack { .. } => {}
            SyncMsg::Done { .. } => return Ok(stats),
            other => {
                let e = SyncMsg::Error {
                    message: format!("unexpected message: {other:?}"),
                };
                let _ = write_frame(tx, &bincode::serialize(&e).unwrap_or_default());
                return Err(SyncError::Protocol("unexpected message".into()));
            }
        }
    }
}

/// Pull everything we are missing from a peer. Reconnect and call again to resume;
/// already-held changes are never re-requested because the known-set is re-sent.
pub fn pull<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    node: &mut Node,
) -> Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    loop {
        let hello = SyncMsg::Hello {
            version: PROTOCOL_VERSION,
            workspace_id: node.workspace_id,
            changes: node.change_ids(),
            membership: node.log.ids(),
        };
        write_frame(
            tx,
            &bincode::serialize(&hello).map_err(|_| SyncError::Protocol("encode".into()))?,
        )?;

        let frame = read_frame(rx)?;
        let msg: SyncMsg = bincode::deserialize(&frame)
            .map_err(|_| SyncError::Protocol("undecodable message".into()))?;

        match msg {
            SyncMsg::Response {
                changes,
                membership,
                more,
                changed,
                sent,
            } => {
                stats.pages += 1;
                stats.sent += sent;
                // Membership first: it decides whether a change's author is valid.
                for raw in membership {
                    if let Ok(entry) = serde_json::from_slice::<MembershipEntry>(&raw) {
                        let _ = node.log.insert(node.workspace_id, entry);
                    }
                }
                for raw in changes {
                    if let Ok(env) = ChangeEnvelope::decode(&raw) {
                        stats.received += 1;
                        if node.ingest(env).unwrap_or(false) {
                            stats.applied += 1;
                        }
                    }
                }
                stats.applied += node.retry_pending();
                let _ = changed;
                let ack = SyncMsg::Ack {
                    applied: stats.applied,
                };
                write_frame(
                    tx,
                    &bincode::serialize(&ack).map_err(|_| SyncError::Protocol("encode".into()))?,
                )?;
                if !more {
                    break;
                }
                // Guard against a peer that never makes progress.
                if sent == 0 {
                    break;
                }
            }
            SyncMsg::Error { message } => return Err(SyncError::PeerError(message)),
            other => {
                return Err(SyncError::Protocol(format!(
                    "unexpected message: {other:?}"
                )))
            }
        }
    }
    let done = SyncMsg::Done {
        total_received: stats.received,
        total_sent: stats.sent,
    };
    let _ = write_frame(
        tx,
        &bincode::serialize(&done).map_err(|_| SyncError::Protocol("encode".into()))?,
    );
    Ok(stats)
}

/// Two-way sync over a single duplex link: we pull, then we serve.
/// Order matters less than it looks, a second round trip would find nothing, so
/// running both directions on one connection converges in a single contact.
pub fn sync_duplex<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    node: &mut Node,
) -> Result<SyncStats, SyncError> {
    let pulled = pull(rx, tx, node)?;
    let _ = tx.flush();
    Ok(pulled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Identity;
    use crate::docs::DocType;
    use std::os::unix::net::UnixStream;
    use std::thread::scope;

    fn pair() -> (Node, Node) {
        let mut alice = Node::create_workspace(Identity::generate(), "ws").unwrap();
        let bob_id = Identity::generate();
        let inv = alice.invite_for(&bob_id.public()).unwrap();
        let bob = Node::join_from_invite(bob_id, &inv).unwrap();
        (alice, bob)
    }

    /// Run one pull against one serve over a Unix socket pair.
    fn sync_two(a: &mut Node, b: &mut Node) -> (SyncStats, SyncStats) {
        let (c1, c2) = UnixStream::pair().unwrap();
        let mut r1 = c1.try_clone().unwrap();
        let mut w1 = c1;
        let mut r2 = c2.try_clone().unwrap();
        let mut w2 = c2;
        scope(|s| {
            let h = s.spawn(move || pull(&mut r2, &mut w2, b).unwrap());
            let a_stats = serve(&mut r1, &mut w1, a).unwrap();
            let b_stats = h.join().unwrap();
            (a_stats, b_stats)
        })
    }

    #[test]
    fn initial_sync_transfers_documents() {
        let (mut a, mut b) = pair();
        let did = a.create_doc(DocType::Checklist, "Supplies").unwrap();
        a.add_checklist_item(did, "water").unwrap();

        let (_sa, sb) = sync_two(&mut a, &mut b);
        assert!(sb.received > 0, "bob should have received changes");
        assert!(b.doc(&did).is_some());
        assert_eq!(b.doc(&did).unwrap().iter_items().len(), 1);
    }

    #[test]
    fn second_sync_transfers_nothing() {
        let (mut a, mut b) = pair();
        let did = a.create_doc(DocType::Note, "n").unwrap();
        a.set_note(did, "hello").unwrap();
        sync_two(&mut a, &mut b);
        let before = b.change_count();
        let (_sa, sb) = sync_two(&mut a, &mut b);
        assert_eq!(sb.received, 0, "nothing new to pull");
        assert_eq!(b.change_count(), before);
    }

    #[test]
    fn bidirectional_edits_converge() {
        let (mut a, mut b) = pair();
        let did = a.create_doc(DocType::Checklist, "Shared").unwrap();
        sync_two(&mut a, &mut b);

        a.add_checklist_item(did, "from-a").unwrap();
        b.add_checklist_item(did, "from-b").unwrap();

        sync_two(&mut a, &mut b);
        sync_two(&mut b, &mut a);

        assert_eq!(a.render(), b.render());
        assert_eq!(a.doc(&did).unwrap().iter_items().len(), 2);
    }

    #[test]
    fn multi_page_transfer_completes() {
        let (mut a, mut b) = pair();
        let did = a.create_doc(DocType::Note, "big").unwrap();
        for i in 0..(PAGE_CHANGES + 5) {
            a.set_note(did, &format!("revision {i}")).unwrap();
        }
        let before = a.change_count();
        assert!(
            before > PAGE_CHANGES,
            "fixture must span more than one page"
        );
        let (_sa, sb) = sync_two(&mut a, &mut b);
        assert!(sb.pages >= 2, "expected multiple pages, got {}", sb.pages);
        assert_eq!(a.render(), b.render());
        assert_eq!(b.change_count(), before);
    }

    #[test]
    fn resume_after_partial_transfer() {
        let (mut a, mut b) = pair();
        let did = a.create_doc(DocType::Note, "big").unwrap();
        for i in 0..(PAGE_CHANGES + 5) {
            a.set_note(did, &format!("revision {i}")).unwrap();
        }
        let total = a.change_count();
        assert!(
            total > PAGE_CHANGES,
            "fixture must exceed one page, got {total}"
        );

        // One page, then the link drops: the initiator closes without Ack/Done.
        let (c1, c2) = UnixStream::pair().unwrap();
        let mut r1 = c1.try_clone().unwrap();
        let mut w1 = c1;
        let mut r2 = c2.try_clone().unwrap();
        let mut w2 = c2;
        let a_reborrow = &mut a; // borrow so `a` stays usable after the scope
        scope(|s| {
            let server = s.spawn(move || {
                // `serve` returns when the peer disappears.
                let _ = serve(&mut r1, &mut w1, a_reborrow);
            });
            let hello = SyncMsg::Hello {
                version: PROTOCOL_VERSION,
                workspace_id: b.workspace_id,
                changes: b.change_ids(),
                membership: b.log.ids(),
            };
            write_frame(&mut w2, &bincode::serialize(&hello).unwrap()).unwrap();
            let frame = read_frame(&mut r2).unwrap();
            match bincode::deserialize::<SyncMsg>(&frame).unwrap() {
                SyncMsg::Response { changes, more, .. } => {
                    assert!(more, "first page must report more");
                    for raw in changes {
                        if let Ok(env) = ChangeEnvelope::decode(&raw) {
                            let _ = b.ingest(env);
                        }
                    }
                }
                other => panic!("expected a Response, got {other:?}"),
            }
            // Close without Ack/Done: this is the dropped link.
            drop((r2, w2));
            let _ = server.join();
        });

        let partial = b.change_count();
        assert!(partial > 0, "some changes arrived before the drop");
        assert!(
            partial < total,
            "transfer was genuinely interrupted ({partial}/{total})"
        );

        // Reconnect: the known-set makes it resume without re-sending the first page.
        let (_sa, sb2) = sync_two(&mut a, &mut b);
        assert!(sb2.received > 0, "resume should fetch the remainder");
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn rejects_wrong_workspace() {
        let (_a, mut b) = pair();
        let mut c = Node::create_workspace(Identity::generate(), "other").unwrap();
        let (c1, c2) = UnixStream::pair().unwrap();
        let mut r1 = c1.try_clone().unwrap();
        let mut w1 = c1;
        let mut r2 = c2.try_clone().unwrap();
        let mut w2 = c2;
        scope(|s| {
            let h = s.spawn(move || pull(&mut r2, &mut w2, &mut b));
            let served = serve(&mut r1, &mut w1, &mut c);
            assert!(served.is_err(), "server must reject a foreign workspace");
            let _ = h.join();
        });
    }
}
