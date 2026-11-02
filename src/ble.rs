//! BLE discovery: the privacy-preserving advertisement scheme.
//!
//! Discovery must answer one question — *is a workspace peer nearby?* — without
//! telling anyone else: who you are, which workspace you are in, or that you are
//! running MeshDoc at all.
//!
//! # Design
//!
//! A device advertises a **128-bit service UUID** that rotates every
//! [`BUCKET_SECONDS`]:
//!
//! ```text
//! discovery_key = BLAKE3-derive-key("meshdoc-ble-discovery-v1", epoch_key)
//! uuid(bucket)  = BLAKE3-keyed(discovery_key, "uuid" || bucket_le)[0..16]
//! payload(bucket)= BLAKE3-keyed(discovery_key, "payload" || bucket_le)[0..8]
//! ```
//!
//! Only holders of the workspace epoch key can compute the sequence, so an observer
//! sees a random UUID that is unrelated between workspaces and changes every bucket.
//! Peers scan the current and the previous bucket to tolerate clock skew.
//!
//! The 8-byte payload is a cheap *state hint*: peers can tell whether there is any
//! point connecting before paying for a connection. It is a keyed digest of the
//! sender's version-vector summary, so it leaks nothing to non-members.
//!
//! # Honest limits
//!
//! This hides the *application layer*. It does not defeat link-layer tracking: the
//! Bluetooth device address may stay stable depending on the Android stack and chip.
//! Any claim of anonymity would be false; the claim is "no stable application-layer
//! identifier".

use std::collections::BTreeMap;

use crate::crypto::WorkspaceKey;
use crate::ids::{hex, unhex};

/// How long one rotating advertisement identifier is valid.
pub const BUCKET_SECONDS: u64 = 15 * 60;

/// How many buckets back we still accept, to tolerate clock skew between devices.
pub const ACCEPT_BUCKETS_BACK: u64 = 2;

/// Size of the advertisement service UUID in bytes.
pub const UUID_LEN: usize = 16;

/// Size of the service-data payload in bytes.
pub const PAYLOAD_LEN: usize = 8;

/// The advertiser that must appear in a BLE advertisement, in bytes.
pub type ServiceUuid = [u8; UUID_LEN];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryTag {
    /// Which time bucket this tag belongs to.
    pub bucket: u64,
    /// 128-bit service UUID to advertise / filter on.
    pub uuid: ServiceUuid,
    /// 8-byte service-data hint.
    pub payload: [u8; PAYLOAD_LEN],
}

impl DiscoveryTag {
    pub fn uuid_hex(&self) -> String { hex(&self.uuid) }
    pub fn payload_hex(&self) -> String { hex(&self.payload) }

    /// Parse the hex forms the UI and platform channel exchange.
    pub fn from_hex(uuid_hex: &str, payload_hex: &str, bucket: u64) -> Option<Self> {
        let u = unhex(uuid_hex)?;
        let p = unhex(payload_hex)?;
        if u.len() != UUID_LEN || p.len() != PAYLOAD_LEN {
            return None;
        }
        let mut uuid = [0u8; UUID_LEN];
        let mut payload = [0u8; PAYLOAD_LEN];
        uuid.copy_from_slice(&u);
        payload.copy_from_slice(&p);
        Some(Self { bucket, uuid, payload })
    }
}

/// Derive the discovery key for a workspace epoch key.
pub fn discovery_key(epoch_key: &WorkspaceKey) -> [u8; 32] {
    blake3::derive_key("meshdoc-ble-discovery-v1", epoch_key.as_bytes())
}

/// The time bucket for a unix timestamp.
pub fn bucket_at(unix_seconds: u64) -> u64 { unix_seconds / BUCKET_SECONDS }

fn derive(discovery_key: &[u8; 32], label: &[u8], bucket: u64) -> blake3::Hash {
    let mut input = Vec::with_capacity(label.len() + 8);
    input.extend_from_slice(label);
    input.extend_from_slice(&bucket.to_le_bytes());
    let mut hasher = blake3::Hasher::new_keyed(discovery_key);
    hasher.update(&input);
    hasher.finalize()
}

/// Build the tag to advertise at `unix_seconds`, optionally hinting local state.
pub fn discovery_tag(
    epoch_key: &WorkspaceKey,
    unix_seconds: u64,
    state_hint: Option<&[u8]>,
) -> DiscoveryTag {
    let key = discovery_key(epoch_key);
    let bucket = bucket_at(unix_seconds);

    let uuid_hash = derive(&key, b"uuid", bucket);
    let mut uuid = [0u8; UUID_LEN];
    uuid.copy_from_slice(&uuid_hash.as_bytes()[..UUID_LEN]);
    // Present as a valid v4-format UUID: version nibble 4, variant bits 10.
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;

    let mut payload = [0u8; PAYLOAD_LEN];
    if let Some(hint) = state_hint {
        let mut hasher = blake3::Hasher::new_keyed(&key);
        hasher.update(b"payload");
        hasher.update(&bucket.to_le_bytes());
        hasher.update(hint);
        payload.copy_from_slice(&hasher.finalize().as_bytes()[..PAYLOAD_LEN]);
    }
    DiscoveryTag { bucket, uuid, payload }
}

/// Does an observed advertisement match `epoch_key` within the accepted window?
///
/// Returns the bucket that matched, so the caller can tell a fresh sighting from a
/// stale one. Non-members cannot produce a match because they cannot derive the
/// UUID sequence without the epoch key.
pub fn match_advertisement(
    epoch_key: &WorkspaceKey,
    observed_uuid: &[u8],
    now_seconds: u64,
) -> Option<u64> {
    if observed_uuid.len() != UUID_LEN {
        return None;
    }
    let current = bucket_at(now_seconds);
    for back in 0..=ACCEPT_BUCKETS_BACK {
        let bucket = current.saturating_sub(back);
        let tag = discovery_tag(epoch_key, bucket * BUCKET_SECONDS, None);
        if tag.uuid == observed_uuid {
            return Some(bucket);
        }
    }
    None
}

/// A peer learned about through discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// Link-layer address (Bluetooth MAC or Wi-Fi Direct address). Not an identity.
    pub address: String,
    /// Time bucket in which we last matched their advertisement.
    pub bucket: u64,
    /// Signal strength in dBm, if the platform reported it.
    pub rssi: Option<i16>,
    /// When we last saw them (unix seconds).
    pub last_seen: u64,
}

/// Bounded, deduplicated view of nearby peers.
#[derive(Debug, Default)]
pub struct PeerRegistry {
    peers: BTreeMap<String, DiscoveredPeer>,
    max_peers: usize,
}

impl PeerRegistry {
    pub fn new(max_peers: usize) -> Self {
        Self { peers: BTreeMap::new(), max_peers: max_peers.max(1) }
    }

    /// Record a sighting. Returns true if this is a new peer.
    pub fn observe(&mut self, peer: DiscoveredPeer) -> bool {
        // Evict the least recently seen peer if we are at capacity and this is new.
        if !self.peers.contains_key(&peer.address) && self.peers.len() >= self.max_peers {
            if let Some(oldest) = self
                .peers
                .iter()
                .min_by_key(|(_, p)| p.last_seen)
                .map(|(k, _)| k.clone())
            {
                self.peers.remove(&oldest);
            }
        }
        let is_new = !self.peers.contains_key(&peer.address);
        self.peers.insert(peer.address.clone(), peer);
        is_new
    }

    pub fn get(&self, address: &str) -> Option<&DiscoveredPeer> {
        self.peers.get(address)
    }

    pub fn len(&self) -> usize { self.peers.len() }
    pub fn is_empty(&self) -> bool { self.peers.is_empty() }

    /// Peers seen within `ttl_seconds` of `now`, most recently seen first.
    pub fn recent(&self, now: u64, ttl_seconds: u64) -> Vec<&DiscoveredPeer> {
        let mut v: Vec<&DiscoveredPeer> = self
            .peers
            .values()
            .filter(|p| now.saturating_sub(p.last_seen) <= ttl_seconds)
            .collect();
        v.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        v
    }

    /// Drop peers not seen for `ttl_seconds`.
    pub fn prune(&mut self, now: u64, ttl_seconds: u64) -> usize {
        let before = self.peers.len();
        self.peers.retain(|_, p| now.saturating_sub(p.last_seen) <= ttl_seconds);
        before - self.peers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WorkspaceKey { WorkspaceKey::generate() }

    #[test]
    fn tag_is_stable_within_a_bucket() {
        let k = key();
        // Align to a bucket boundary so both timestamps really are in one bucket.
        let base = BUCKET_SECONDS * 1_111;
        let a = discovery_tag(&k, base, None);
        let b = discovery_tag(&k, base + BUCKET_SECONDS - 1, None);
        assert_eq!(a.bucket, b.bucket);
        assert_eq!(a.uuid, b.uuid);
    }

    #[test]
    fn tag_rotates_between_buckets() {
        let k = key();
        let a = discovery_tag(&k, 1_000_000, None);
        let b = discovery_tag(&k, 1_000_000 + BUCKET_SECONDS, None);
        assert_ne!(a.uuid, b.uuid, "the identifier must rotate");
    }

    #[test]
    fn different_workspaces_are_unrelated() {
        let a = discovery_tag(&key(), 1_000_000, None);
        let b = discovery_tag(&key(), 1_000_000, None);
        assert_ne!(a.uuid, b.uuid);
    }

    #[test]
    fn uuid_has_valid_version_and_variant_bits() {
        let t = discovery_tag(&key(), 1_700_000_000, None);
        assert_eq!(t.uuid[6] >> 4, 4, "version nibble must be 4");
        assert_eq!(t.uuid[8] >> 6, 0b10, "variant bits must be 10");
    }

    #[test]
    fn matching_accepts_current_and_recent_buckets() {
        let k = key();
        let now = 1_700_000_000u64;
        let current = discovery_tag(&k, now, None);
        assert_eq!(match_advertisement(&k, &current.uuid, now), Some(current.bucket));

        // One bucket stale still matches (clock skew tolerance).
        let stale = discovery_tag(&k, now - BUCKET_SECONDS, None);
        assert_eq!(match_advertisement(&k, &stale.uuid, now), Some(stale.bucket));

        // Too old is rejected.
        let ancient = discovery_tag(&k, now - BUCKET_SECONDS * (ACCEPT_BUCKETS_BACK + 2), None);
        assert_eq!(match_advertisement(&k, &ancient.uuid, now), None);
    }

    #[test]
    fn non_member_cannot_match_even_with_the_right_shape() {
        let member = key();
        let stranger = key();
        let now = 1_700_000_000u64;
        let stranger_tag = discovery_tag(&stranger, now, None);
        assert_eq!(match_advertisement(&member, &stranger_tag.uuid, now), None);
    }

    #[test]
    fn wrong_length_uuid_is_rejected() {
        let k = key();
        assert_eq!(match_advertisement(&k, &[0u8; 8], 1_700_000_000), None);
    }

    #[test]
    fn state_hint_changes_the_payload_not_the_uuid() {
        let k = key();
        let now = 1_700_000_000;
        let a = discovery_tag(&k, now, Some(b"vv-a"));
        let b = discovery_tag(&k, now, Some(b"vv-b"));
        assert_eq!(a.uuid, b.uuid, "uuid identifies the workspace+bucket only");
        assert_ne!(a.payload, b.payload, "payload carries the state hint");
    }

    #[test]
    fn tag_hex_roundtrip() {
        let t = discovery_tag(&key(), 1_700_000_000, Some(b"x"));
        let back = DiscoveryTag::from_hex(&t.uuid_hex(), &t.payload_hex(), t.bucket).unwrap();
        assert_eq!(back.uuid, t.uuid);
        assert_eq!(back.payload, t.payload);
    }

    #[test]
    fn registry_dedupes_and_orders_by_recency() {
        let mut r = PeerRegistry::new(8);
        assert!(r.observe(DiscoveredPeer {
            address: "aa".into(), bucket: 1, rssi: Some(-60), last_seen: 100,
        }));
        assert!(!r.observe(DiscoveredPeer {
            address: "aa".into(), bucket: 2, rssi: Some(-55), last_seen: 200,
        }));
        r.observe(DiscoveredPeer { address: "bb".into(), bucket: 2, rssi: None, last_seen: 150 });
        assert_eq!(r.len(), 2);
        let recent = r.recent(200, 1000);
        assert_eq!(recent[0].address, "aa", "most recently seen first");
        assert_eq!(recent[0].rssi, Some(-55), "latest sighting wins");
    }

    #[test]
    fn registry_prunes_stale_and_caps_size() {
        let mut r = PeerRegistry::new(2);
        r.observe(DiscoveredPeer { address: "a".into(), bucket: 0, rssi: None, last_seen: 10 });
        r.observe(DiscoveredPeer { address: "b".into(), bucket: 0, rssi: None, last_seen: 20 });
        r.observe(DiscoveredPeer { address: "c".into(), bucket: 0, rssi: None, last_seen: 30 });
        assert_eq!(r.len(), 2, "capacity enforced");
        assert!(r.get("a").is_none(), "oldest evicted");

        assert_eq!(r.prune(10_000, 60), 2);
        assert!(r.is_empty());
    }
}
