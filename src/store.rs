//! Encrypted local persistence.
//!
//! Everything on disk is ciphertext. A passphrase is stretched with Argon2id into a
//! master key, which seals each record with XChaCha20-Poly1305. The on-disk layout:
//!
//! ```text
//! <dir>/identity.mds               sealed identity secrets (Ed25519 + X25519)
//! <dir>/workspaces/<wsid>.mds      sealed node state (membership + change log)
//! ```
//!
//! File format: `"MDS1" | salt(16) | nonce(24) | ciphertext`
//!
//! The change log is sealed as a whole. That is simpler and safer than per-record
//! encryption, at the cost of rewriting the file on each save — acceptable for the
//! document sizes this tool targets, and it means a torn write cannot leave a
//! half-encrypted log (writes go to a temp file and are renamed).

use std::fs;
use std::path::{Path, PathBuf};

use argon2::Argon2;
use rand::RngCore;

use crate::crypto::{CryptoError, Identity, WorkspaceKey};
use crate::ids::WorkspaceId;
use crate::node::{Node, PersistState};

const MAGIC: &[u8; 4] = b"MDS1";
const SALT_LEN: usize = 16;

#[derive(Debug)]
pub enum StoreError {
    Io(String),
    Crypto(String),
    Format(String),
    NoIdentity,
    NoSuchWorkspace(WorkspaceId),
    WrongPassphrase,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store i/o: {e}"),
            StoreError::Crypto(e) => write!(f, "store crypto: {e}"),
            StoreError::Format(e) => write!(f, "store format: {e}"),
            StoreError::NoIdentity => write!(f, "no identity in this store (run `init` first)"),
            StoreError::NoSuchWorkspace(w) => write!(f, "no such workspace {}", &w.to_hex()[..8]),
            StoreError::WrongPassphrase => write!(f, "wrong passphrase"),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<std::io::Error> for StoreError { fn from(e: std::io::Error) -> Self { StoreError::Io(e.to_string()) } }
impl From<CryptoError> for StoreError { fn from(e: CryptoError) -> Self { StoreError::Crypto(e.to_string()) } }

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join("workspaces"))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path { &self.dir }

    fn identity_path(&self) -> PathBuf { self.dir.join("identity.mds") }
    fn workspace_path(&self, ws: &WorkspaceId) -> PathBuf {
        self.dir.join("workspaces").join(format!("{}.mds", ws.to_hex()))
    }

    pub fn has_identity(&self) -> bool { self.identity_path().exists() }

    /// Save the device identity, sealed under a passphrase.
    pub fn save_identity(&self, id: &Identity, passphrase: &str) -> Result<(), StoreError> {
        let (seed, dh) = id.to_secret_bytes();
        let mut blob = Vec::with_capacity(64);
        blob.extend_from_slice(&seed);
        blob.extend_from_slice(&dh);
        let sealed = seal_record(passphrase, &blob)?;
        atomic_write(&self.identity_path(), &sealed)
    }

    pub fn load_identity(&self, passphrase: &str) -> Result<Identity, StoreError> {
        if !self.has_identity() { return Err(StoreError::NoIdentity); }
        let raw = fs::read(self.identity_path())?;
        let plain = open_record(passphrase, &raw)?;
        if plain.len() != 64 { return Err(StoreError::Format("identity record length".into())); }
        let mut seed = [0u8; 32];
        let mut dh = [0u8; 32];
        seed.copy_from_slice(&plain[..32]);
        dh.copy_from_slice(&plain[32..]);
        Ok(Identity::from_secret_bytes(&seed, &dh))
    }

    pub fn save_node(&self, node: &Node, passphrase: &str) -> Result<(), StoreError> {
        let state = node.export_state();
        let json = serde_json::to_vec(&state).map_err(|e| StoreError::Format(e.to_string()))?;
        let sealed = seal_record(passphrase, &json)?;
        atomic_write(&self.workspace_path(&node.workspace_id), &sealed)
    }

    pub fn load_node(
        &self,
        identity: Identity,
        ws: WorkspaceId,
        passphrase: &str,
    ) -> Result<Node, StoreError> {
        let path = self.workspace_path(&ws);
        if !path.exists() { return Err(StoreError::NoSuchWorkspace(ws)); }
        let raw = fs::read(path)?;
        let plain = open_record(passphrase, &raw)?;
        let state: PersistState = serde_json::from_slice(&plain)
            .map_err(|e| StoreError::Format(e.to_string()))?;
        Node::import_state(identity, state).map_err(|e| StoreError::Format(e.to_string()))
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceId>, StoreError> {
        let mut out = vec![];
        for entry in fs::read_dir(self.dir.join("workspaces"))? {
            let p = entry?.path();
            if p.extension().and_then(|e| e.to_str()) != Some("mds") { continue; }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if let Some(ws) = WorkspaceId::from_hex(stem) { out.push(ws); }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn delete_workspace(&self, ws: &WorkspaceId) -> Result<(), StoreError> {
        let p = self.workspace_path(ws);
        if p.exists() { fs::remove_file(p)?; }
        Ok(())
    }
}

/// Argon2id-stretch the passphrase, then seal the plaintext.
pub fn seal_record(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let key = derive_key(passphrase, &salt)?;
    let sealed = WorkspaceKey(key).seal(plaintext, MAGIC)?;
    let mut out = Vec::with_capacity(4 + SALT_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&sealed);
    Ok(out)
}

pub fn open_record(passphrase: &str, record: &[u8]) -> Result<Vec<u8>, StoreError> {
    if record.len() < 4 + SALT_LEN + 24 + 16 {
        return Err(StoreError::Format("record too short".into()));
    }
    if &record[..4] != MAGIC {
        return Err(StoreError::Format("bad magic".into()));
    }
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&record[4..4 + SALT_LEN]);
    let key = derive_key(passphrase, &salt)?;
    let sealed = &record[4 + SALT_LEN..];
    WorkspaceKey(key)
        .open(sealed, MAGIC)
        .map_err(|_| StoreError::WrongPassphrase)
}

fn derive_key(passphrase: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; 32], StoreError> {
    let mut key = [0u8; 32];
    // Argon2id with explicit, modest parameters: ~19 MiB, 2 passes.
    // Chosen to be usable on a laptop while still being expensive to attack.
    let params = argon2::Params::new(19 * 1024, 2, 1, Some(32))
        .map_err(|e| StoreError::Crypto(format!("argon2 params: {e}")))?;
    Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| StoreError::Crypto(format!("argon2: {e}")))?;
    Ok(key)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    // Make the rename durable where the platform lets us.
    if let Ok(f) = fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        if let Ok(d) = fs::File::open(parent) { let _ = d.sync_all(); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Identity;
    use crate::docs::DocType;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "meshdoc-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn identity_roundtrip() {
        let dir = tmpdir("id");
        let store = Store::open(&dir).unwrap();
        let id = Identity::generate();
        let fp = id.fingerprint();
        store.save_identity(&id, "correct horse battery staple").unwrap();
        let back = store.load_identity("correct horse battery staple").unwrap();
        assert_eq!(back.fingerprint(), fp);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let dir = tmpdir("wrongpass");
        let store = Store::open(&dir).unwrap();
        store.save_identity(&Identity::generate(), "one").unwrap();
        assert!(matches!(store.load_identity("two"), Err(StoreError::WrongPassphrase)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_on_disk_are_not_plaintext() {
        let dir = tmpdir("cipher");
        let store = Store::open(&dir).unwrap();
        let id = Identity::generate();
        store.save_identity(&id, "pw").unwrap();
        let raw = fs::read(dir.join("identity.mds")).unwrap();
        // The seed bytes must not appear in the file.
        let (seed, _) = id.to_secret_bytes();
        assert!(!raw.windows(32).any(|w| w == seed));
        assert_eq!(&raw[..4], MAGIC);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn node_roundtrip_preserves_state() {
        let dir = tmpdir("node");
        let store = Store::open(&dir).unwrap();
        let id = Identity::generate();
        let mut node = Node::create_workspace(id, "Persisted").unwrap();
        // Persist the identity too, so we can reload exactly as a CLI run would.
        store.save_identity(&node.identity, "pw").unwrap();
        let did = node.create_doc(DocType::Checklist, "Tasks").unwrap();
        node.add_checklist_item(did, "evacuate").unwrap();
        let ws = node.workspace_id;
        let before = node.render();
        store.save_node(&node, "pw").unwrap();
        drop(node);

        let reloaded_id = store.load_identity("pw").unwrap();
        let reloaded = store.load_node(reloaded_id, ws, "pw").unwrap();
        assert_eq!(reloaded.render(), before);
        assert_eq!(reloaded.workspace_id, ws);
        assert_eq!(store.list_workspaces().unwrap(), vec![ws]);
        let _ = fs::remove_dir_all(&dir);
    }
}
