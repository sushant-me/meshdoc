//! MeshDoc: offline-first, end-to-end-encrypted collaboration for teams without
//! reliable connectivity.
//!
//! The reusable library stack, in pieces:
//!
//! * [`crypto`]: device identity (Ed25519 + X25519), workspace epoch keys,
//!   XChaCha20-Poly1305 envelopes, key grants.
//! * [`ids`]: workspace, document, peer and change identifiers.
//! * [`membership`]: signed, append-only membership log with key-rotation epochs.
//! * [`docs`]: typed CRDT documents (checklist, supply form, geo point, note).
//! * [`change`]: the encrypted, signed change envelope, the only thing that ever
//!   crosses a wire.
//! * [`sync`]: anti-entropy reconciliation between peers.
//! * [`transport`]: the `MeshTransport` abstraction and its implementations.
//! * [`store`]: encrypted local persistence.
//! * [`relay`]: client side of the store-and-forward relay.
//! * [`node`]: ties it together into a running peer.
//!
//! Four rules that hold everywhere:
//!
//! 1. Encrypt first, then transport. The mesh, the relay and a seized disk all see
//!    the same thing: ciphertext.
//! 2. No server is needed for correctness. The relay is optional and untrusted.
//! 3. A change is attributable to a key in the membership log, so a leaked
//!    workspace key lets you read but not write.
//! 4. A workspace id never comes from a key. Rotation must not break references.

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
