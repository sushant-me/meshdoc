//! On-disk blob store.
//!
//! Layout (all state lives under a single data directory):
//!
//! ```text
//! <data>/<wsid>/seq                        # decimal counter: highest seq ever assigned
//! <data>/<wsid>/000000000001-<id>.blob     # one opaque file per blob
//! ```
//!
//! Blobs are written with a temp-file + fsync + rename + directory-fsync dance so
//! a crash can never leave a half-written blob under its final name. The in-memory
//! index is rebuilt from the directory on startup, taking the max of the filename
//! sequences and the persisted `seq` counter, so sequence numbers never go
//! backwards even after deletions or restarts.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// Name of the per-workspace sequence counter file.
pub const SEQ_FILE_NAME: &str = "seq";
/// Suffix for blob files.
pub const BLOB_SUFFIX: &str = ".blob";

/// Prefix for in-progress temporary files. Leftovers are removed on startup.
const TEMP_PREFIX: &str = ".tmp-";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Lock a mutex, recovering from poisoning instead of panicking. A panic in one
/// request handler must not take the whole mailbox down.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A workspace id is exactly 32 lowercase hex characters.
pub fn is_valid_wsid(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A blob id is exactly 32 lowercase hex characters.
pub fn is_valid_blob_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Blob id = first 32 hex chars (16 bytes) of blake3(body).
pub fn compute_blob_id(data: &[u8]) -> String {
    let hex = blake3::hash(data).to_hex();
    hex.as_str()[..32].to_string()
}

fn blob_file_name(seq: u64, id: &str) -> String {
    format!("{:012}-{}{}", seq, id, BLOB_SUFFIX)
}

fn parse_blob_file_name(name: &str) -> Option<(u64, String)> {
    let stem = name.strip_suffix(BLOB_SUFFIX)?;
    if stem.len() != 12 + 1 + 32 {
        return None;
    }
    let (seq_part, rest) = stem.split_at(12);
    let id = rest.strip_prefix('-')?;
    if !seq_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !is_valid_blob_id(id) {
        return None;
    }
    let seq: u64 = seq_part.parse().ok()?;
    if seq == 0 {
        return None;
    }
    Some((seq, id.to_string()))
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Write `data` to `path` atomically: temp file in the same directory, fsync the
/// file, rename over the final name, then fsync the directory.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = dir.join(format!(
        "{}{}-{}-{}",
        TEMP_PREFIX,
        std::process::id(),
        counter,
        now_nanos()
    ));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        // Make the rename itself durable. On some filesystems opening a
        // directory for fsync may fail; that is non-fatal, the data is still safe.
        if let Ok(dir_handle) = File::open(dir) {
            let _ = dir_handle.sync_all();
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

struct BlobEntry {
    id: String,
    path: PathBuf,
}

struct Workspace {
    dir: PathBuf,
    /// Highest sequence number ever assigned (never decreases).
    next_seq: u64,
    by_id: HashMap<String, u64>,
    entries: BTreeMap<u64, BlobEntry>,
}

impl Workspace {
    fn load(dir: PathBuf) -> io::Result<Workspace> {
        let mut workspace = Workspace {
            dir,
            next_seq: 0,
            by_id: HashMap::new(),
            entries: BTreeMap::new(),
        };
        let mut persisted_seq: u64 = 0;
        let mut scanned_files = Vec::new();

        for entry in fs::read_dir(&workspace.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == SEQ_FILE_NAME {
                if let Ok(text) = fs::read_to_string(entry.path()) {
                    persisted_seq = text.trim().parse().unwrap_or(0);
                }
                continue;
            }
            if name.starts_with(TEMP_PREFIX) {
                // Leftover from a crashed atomic write; never visible to clients.
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if let Some((seq, id)) = parse_blob_file_name(&name) {
                scanned_files.push((seq, id, entry.path()));
            }
        }

        scanned_files.sort_by_key(|(seq, _, _)| *seq);
        for (seq, id, path) in scanned_files {
            workspace.next_seq = workspace.next_seq.max(seq);
            // Defend against duplicate ids (only possible via manual tampering).
            // The lowest seq wins; extra files are left on disk untouched.
            if let std::collections::hash_map::Entry::Vacant(slot) = workspace.by_id.entry(id.clone()) {
                slot.insert(seq);
                workspace.entries.insert(seq, BlobEntry { id, path });
            }
        }
        workspace.next_seq = workspace.next_seq.max(persisted_seq);
        Ok(workspace)
    }

    fn head(&self) -> u64 {
        self.entries.keys().next_back().copied().unwrap_or(0)
    }

    fn count(&self) -> usize {
        self.entries.len()
    }
}

/// Result of storing a blob.
#[derive(Debug, Clone)]
pub struct PutOutcome {
    /// Sequence number assigned to this content (existing one for duplicate posts).
    pub seq: u64,
    /// Content id: first 32 hex chars of blake3(body).
    pub id: String,
    /// True when a new blob file was created, false for an idempotent re-post.
    pub created: bool,
}

/// One blob as returned by a list operation.
#[derive(Debug, Clone)]
pub struct BlobRecord {
    pub seq: u64,
    pub id: String,
    pub data: Vec<u8>,
}

/// Result of listing a workspace.
#[derive(Debug, Clone)]
pub struct ListOutcome {
    pub blobs: Vec<BlobRecord>,
    /// Highest seq included, or the `since` value when nothing was returned.
    pub cursor: u64,
    /// True when more blobs exist beyond this page.
    pub more: bool,
}

/// The store: data directory plus a newtype-guarded per-workspace index.
pub struct Store {
    root: PathBuf,
    workspaces: Mutex<HashMap<String, Arc<Mutex<Workspace>>>>,
}

impl Store {
    /// Open (or create) a data directory and rebuild the in-memory index by
    /// scanning every workspace subdirectory.
    pub fn open(root: PathBuf) -> io::Result<Store> {
        fs::create_dir_all(&root)?;
        let mut workspaces = HashMap::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_valid_wsid(&name) {
                // Never touch foreign directories in the data dir.
                continue;
            }
            let workspace = Workspace::load(entry.path())?;
            workspaces.insert(name, Arc::new(Mutex::new(workspace)));
        }
        Ok(Store {
            root,
            workspaces: Mutex::new(workspaces),
        })
    }

    /// Data directory this store lives in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn get_workspace(&self, wsid: &str) -> Option<Arc<Mutex<Workspace>>> {
        lock(&self.workspaces).get(wsid).cloned()
    }

    fn get_or_create_workspace(&self, wsid: &str) -> io::Result<Arc<Mutex<Workspace>>> {
        if !is_valid_wsid(wsid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid workspace id",
            ));
        }
        let mut workspaces = lock(&self.workspaces);
        if let Some(existing) = workspaces.get(wsid) {
            return Ok(Arc::clone(existing));
        }
        let dir = self.root.join(wsid);
        fs::create_dir_all(&dir)?;
        let workspace = Arc::new(Mutex::new(Workspace::load(dir)?));
        workspaces.insert(wsid.to_string(), Arc::clone(&workspace));
        Ok(workspace)
    }

    /// Store an opaque blob. Identical bytes in the same workspace are
    /// idempotent: the original sequence number is returned and nothing is
    /// written. Sequence numbers are assigned under the workspace lock, so two
    /// concurrent posts can never receive the same `seq`.
    pub fn put(&self, wsid: &str, body: &[u8]) -> io::Result<PutOutcome> {
        let id = compute_blob_id(body);
        let workspace_handle = self.get_or_create_workspace(wsid)?;
        let mut workspace = lock(&workspace_handle);

        if let Some(existing_seq) = workspace.by_id.get(&id).copied() {
            return Ok(PutOutcome {
                seq: existing_seq,
                id,
                created: false,
            });
        }

        let seq = workspace.next_seq + 1;
        let path = workspace.dir.join(blob_file_name(seq, &id));
        // Blob first, then counter. A crash between the two is recoverable: the
        // counter on restart is max(filename seq, seq file).
        atomic_write(&path, body)?;
        atomic_write(
            &workspace.dir.join(SEQ_FILE_NAME),
            format!("{}\n", seq).as_bytes(),
        )?;

        workspace.next_seq = seq;
        workspace.by_id.insert(id.clone(), seq);
        workspace.entries.insert(
            seq,
            BlobEntry {
                id: id.clone(),
                path,
            },
        );
        Ok(PutOutcome {
            seq,
            id,
            created: true,
        })
    }

    /// List blobs with `seq > since`, ascending, at most `limit` entries.
    pub fn list(&self, wsid: &str, since: u64, limit: usize) -> io::Result<ListOutcome> {
        let Some(workspace_handle) = self.get_workspace(wsid) else {
            return Ok(ListOutcome {
                blobs: Vec::new(),
                cursor: since,
                more: false,
            });
        };

        // Copy the index entries we need while holding the lock, but read the
        // payloads after releasing it, so a slow read does not block writers.
        let (metas, page_more) = {
            let workspace = lock(&workspace_handle);
            let mut metas = Vec::new();
            let mut more = false;
            for (seq, entry) in workspace.entries.range((Excluded(since), Unbounded)) {
                if metas.len() >= limit {
                    more = true;
                    break;
                }
                metas.push((*seq, entry.id.clone(), entry.path.clone()));
            }
            (metas, more)
        };

        let mut blobs = Vec::with_capacity(metas.len());
        let mut cursor = since;
        for (seq, id, path) in metas {
            match fs::read(&path) {
                Ok(data) => {
                    cursor = seq;
                    blobs.push(BlobRecord { seq, id, data });
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // Deleted between snapshot and read: skip it, never expose it.
                    cursor = seq;
                }
                Err(_err) => {
                    // Transient read failure: stop the page at the last good
                    // cursor and ask the client to retry.
                    return Ok(ListOutcome {
                        blobs,
                        cursor,
                        more: true,
                    });
                }
            }
        }

        Ok(ListOutcome {
            blobs,
            cursor,
            more: page_more,
        })
    }

    /// Delete a blob by id. Returns the seq that was freed, or None if unknown.
    /// The sequence counter is intentionally not rewound: seq must stay monotonic.
    pub fn delete(&self, wsid: &str, id: &str) -> io::Result<Option<u64>> {
        if !is_valid_blob_id(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid blob id"));
        }
        let Some(workspace_handle) = self.get_workspace(wsid) else {
            return Ok(None);
        };
        let mut workspace = lock(&workspace_handle);
        let Some(seq) = workspace.by_id.remove(id) else {
            return Ok(None);
        };
        if let Some(entry) = workspace.entries.remove(&seq) {
            match fs::remove_file(&entry.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    // Disk and index must not drift: restore the entry, report error.
                    workspace.by_id.insert(id.to_string(), seq);
                    workspace.entries.insert(seq, entry);
                    return Err(err);
                }
            }
        }
        Ok(Some(seq))
    }

    /// Highest stored sequence and number of blobs in a workspace (zero if unknown).
    pub fn head(&self, wsid: &str) -> (u64, usize) {
        match self.get_workspace(wsid) {
            Some(workspace_handle) => {
                let workspace = lock(&workspace_handle);
                (workspace.head(), workspace.count())
            }
            None => (0, 0),
        }
    }

    /// Total number of blobs across all workspaces (used by /v1/health).
    pub fn total_blobs(&self) -> usize {
        let handles: Vec<Arc<Mutex<Workspace>>> =
            lock(&self.workspaces).values().cloned().collect();
        handles
            .iter()
            .map(|handle| lock(handle).count())
            .sum()
    }

    /// Evict blobs whose mtime is at least `max_age` old. Returns how many were removed.
    pub fn evict_older_than(&self, max_age: Duration) -> io::Result<usize> {
        let now = SystemTime::now();
        let handles: Vec<Arc<Mutex<Workspace>>> =
            lock(&self.workspaces).values().cloned().collect();
        let mut removed = 0usize;

        for handle in handles {
            let mut workspace = lock(&handle);
            let mut expired = Vec::new();
            for (&seq, entry) in workspace.entries.iter() {
                let old = match fs::metadata(&entry.path) {
                    Ok(metadata) => match metadata.modified() {
                        Ok(modified) => now
                            .duration_since(modified)
                            .map(|age| age >= max_age)
                            .unwrap_or(false),
                        Err(_) => false,
                    },
                    Err(err) => err.kind() == io::ErrorKind::NotFound,
                };
                if old {
                    expired.push(seq);
                }
            }
            for seq in expired {
                if let Some(entry) = workspace.entries.remove(&seq) {
                    workspace.by_id.remove(&entry.id);
                    match fs::remove_file(&entry.path) {
                        Ok(()) => removed += 1,
                        Err(err) if err.kind() == io::ErrorKind::NotFound => removed += 1,
                        Err(_) => {}
                    }
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsid_validation() {
        assert!(is_valid_wsid("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_wsid("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_valid_wsid("../../0123456789abcdef01234567"));
        assert!(!is_valid_wsid("0123456789abcdef0123456789abcde")); // 31 chars
        assert!(!is_valid_wsid("0123456789abcdef0123456789abcdef0")); // 33 chars
        assert!(!is_valid_wsid("0123456789abcdef0123456789abcdeg"));
        assert!(!is_valid_wsid(""));
    }

    #[test]
    fn blob_file_names_round_trip() {
        let id = "0123456789abcdef0123456789abcdef";
        let name = blob_file_name(42, id);
        assert_eq!(name, "000000000042-0123456789abcdef0123456789abcdef.blob");
        assert_eq!(parse_blob_file_name(&name), Some((42, id.to_string())));
        assert_eq!(parse_blob_file_name("000000000042-xyz.blob"), None);
        assert_eq!(parse_blob_file_name("000000000000-0123456789abcdef0123456789abcdef.blob"), None);
    }

    #[test]
    fn blob_id_is_truncated_blake3() {
        let id = compute_blob_id(b"hello");
        assert_eq!(id.len(), 32);
        assert!(is_valid_blob_id(&id));
        assert_eq!(id, compute_blob_id(b"hello"));
        assert_ne!(id, compute_blob_id(b"hellp"));
    }

    #[test]
    fn eviction_removes_only_old_blobs_and_keeps_seq_monotonic() {
        let wsid = "0123456789abcdef0123456789abcdef";
        let dir = std::env::temp_dir().join(format!(
            "meshdoc-relay-store-test-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        let store = Store::open(dir.clone()).expect("open store");

        let old = store.put(wsid, b"old blob").expect("put old");
        let fresh = store.put(wsid, b"fresh blob").expect("put fresh");
        assert_eq!(old.seq, 1);
        assert_eq!(fresh.seq, 2);

        // Backdate only the first blob by two hours.
        let old_path = dir.join(wsid).join(blob_file_name(old.seq, &old.id));
        let handle = OpenOptions::new()
            .write(true)
            .open(&old_path)
            .expect("open old blob");
        let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 3600);
        handle
            .set_times(fs::FileTimes::new().set_modified(two_hours_ago))
            .expect("backdate mtime");
        drop(handle);

        let removed = store
            .evict_older_than(Duration::from_secs(3600))
            .expect("evict");
        assert_eq!(removed, 1, "only the backdated blob is evicted");
        assert!(!old_path.exists(), "evicted file is gone from disk");
        assert_eq!(store.head(wsid), (2, 1), "head/count reflect live blobs");

        let listed = store.list(wsid, 0, 10).expect("list");
        assert_eq!(listed.blobs.len(), 1);
        assert_eq!(listed.blobs[0].id, fresh.id);

        // Eviction must never rewind the sequence counter.
        let third = store.put(wsid, b"third blob").expect("put third");
        assert_eq!(third.seq, 3);

        let _ = fs::remove_dir_all(&dir);
    }
}
