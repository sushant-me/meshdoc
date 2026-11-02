//! # MeshDoc
//!
//! Offline-first, end-to-end-encrypted collaboration for teams without reliable
//! connectivity.
//!
//! This crate is the reusable library stack. The pieces:
//!
//! * [`crypto`] — device identity (Ed25519 + X25519), workspace epoch keys,
//!   XChaCha20-Poly1305 envelopes, key grants.
//! * [`ids`] — workspace/document/peer/change identifiers and epochs.
//! * [`membership`] — signed, append-only membership log with key-rotation epochs.
//! * [`docs`] — typed CRDT documents (checklist, supply form, geo point, note).
//! * [`change`] — the encrypted, signed change envelope: the only thing that
//!   ever travels over any wire.
//! * [`sync`] — anti-entropy reconciliation between peers.
//! * [`transport`] — the `MeshTransport` abstraction plus implementations.
//! * [`store`] — encrypted local persistence.
//! * [`relay`] — the client side of the dumb store-and-forward relay.
//! * [`node`] — ties it together into a running peer.
//!
//! Design invariants:
//! 1. Every wire format is encrypted then transported.
//! 2. No server is required for correctness. The relay is optional and untrusted.
//! 3. Every change is attributable to a key that appears in the membership log.
//! 4. The workspace id is never derived from a key, so rotation never breaks it.

pub mod ble;
pub mod change;
pub mod crypto;
pub mod docs;
pub mod ids;
pub mod membership;
pub mod node;
pub mod p2p;
pub mod relay;
pub mod store;
pub mod sync;
pub mod transport;

pub use ids::{ChangeId, DocId, Epoch, PeerId, WorkspaceId};

/// Version string surfaced by the CLI and relay.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
