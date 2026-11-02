//! C ABI for the MeshDoc core.
//!
//! The Android client links `libmeshdoc.so` through `dart:ffi`. The contract is
//! frozen in `docs/INTERFACES.md` section A.
//!
//! Design notes:
//! * **Stateless.** Every call opens the encrypted store, performs one operation and
//!   closes. That mirrors the CLI, survives process death, and removes any handle
//!   lifetime bugs across the FFI boundary.
//! * **JSON in, JSON out.** One allocation per result, freed by [`meshdoc_free`].
//! * **No unwinding across FFI.** Every entry point is wrapped in `catch_unwind`.
//! * **Errors never hide.** A NULL return is always accompanied by a message from
//!   [`meshdoc_last_error`] on the same thread.

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use meshdoc::ble;
use meshdoc::crypto::{Identity, PublicIdentity};
use meshdoc::docs::DocType;
use meshdoc::ids::{DocId, Epoch, WorkspaceId};
use meshdoc::node::{Invite, Node};
use meshdoc::store::Store;
use meshdoc::sync;
use meshdoc::transport::TcpMesh;

use serde_json::json;

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::new("").unwrap());
}

fn set_error(msg: impl Into<String>) {
    let s = CString::new(msg.into().replace('\0', " ")).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = s);
}

fn ok(value: serde_json::Value) -> *mut c_char {
    let s = value.to_string();
    CString::new(s).map(|c| c.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn fail(msg: impl Into<String>) -> *mut c_char {
    set_error(msg);
    std::ptr::null_mut()
}

/// Run `f`, converting a panic into an error message instead of unwinding.
fn guard<F: FnOnce() -> *mut c_char>(f: F) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(p) => p,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "panic in native code".to_string()
            };
            fail(format!("internal error: {msg}"))
        }
    }
}

/// Read a NUL-terminated C string. `None` for NULL, `Err` for invalid UTF-8.
unsafe fn as_str<'a>(p: *const c_char) -> Result<Option<&'a str>, String> {
    if p.is_null() {
        return Ok(None);
    }
    CStr::from_ptr(p).to_str().map(Some).map_err(|_| "argument is not valid UTF-8".to_string())
}

fn req<'a>(p: *const c_char, name: &str) -> Result<&'a str, String> {
    match unsafe { as_str(p) }? {
        Some(s) => Ok(s),
        None => Err(format!("{name} must not be NULL")),
    }
}

/// Open the store and resolve which workspace to act on.
struct Ctx {
    store: Store,
    pass: String,
    node: Node,
}

impl Ctx {
    fn open(store_dir: &str, pass: &str, ws: &str) -> Result<Self, String> {
        let store = Store::open(PathBuf::from(store_dir)).map_err(|e| e.to_string())?;
        let identity = store
            .load_identity(pass)
            .map_err(|e| format!("{} (run init first)", e))?;
        let wsid = if ws.is_empty() {
            let list = store.list_workspaces().map_err(|e| e.to_string())?;
            match list.len() {
                0 => return Err("no workspace in this store".into()),
                1 => list[0],
                n => return Err(format!("store holds {n} workspaces; specify which")),
            }
        } else {
            WorkspaceId::from_hex(ws).ok_or_else(|| format!("malformed workspace id: {ws}"))?
        };
        let node = store.load_node(identity, wsid, pass).map_err(|e| e.to_string())?;
        Ok(Self { store, pass: pass.to_string(), node })
    }

    fn save(&self) -> Result<(), String> {
        self.store.save_node(&self.node, &self.pass).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Version / memory / errors
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn meshdoc_version() -> *mut c_char {
    ok(json!(env!("CARGO_PKG_VERSION")))
}

/// Free a string returned by any function in this library. NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn meshdoc_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// Message for the most recent failing call on this thread. Valid until the next
/// failing call on the same thread. Never NULL.
#[no_mangle]
pub extern "C" fn meshdoc_last_error() -> *mut c_char {
    LAST_ERROR.with(|e| {
        let b = e.borrow();
        CString::new(b.to_bytes()).unwrap_or_else(|_| CString::new("error").unwrap()).into_raw()
    })
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Create a device identity if none exists. Idempotent.
#[no_mangle]
pub extern "C" fn meshdoc_init(store: *const c_char, pass: *const c_char) -> *mut c_char {
    guard(|| {
        let (store, pass) = match (req(store, "store"), req(pass, "pass")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        let s = match Store::open(PathBuf::from(store)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        let created = !s.has_identity();
        let id = if created {
            let id = Identity::generate();
            if let Err(e) = s.save_identity(&id, pass) {
                return fail(e.to_string());
            }
            id
        } else {
            match s.load_identity(pass) {
                Ok(id) => id,
                Err(e) => return fail(e.to_string()),
            }
        };
        ok(json!({
            "fingerprint": id.fingerprint().to_hex(),
            "public": id.public().to_hex(),
            "created": created,
        }))
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_identity(store: *const c_char, pass: *const c_char) -> *mut c_char {
    guard(|| {
        let (store, pass) = match (req(store, "store"), req(pass, "pass")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        let s = match Store::open(PathBuf::from(store)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        match s.load_identity(pass) {
            Ok(id) => ok(json!({
                "fingerprint": id.fingerprint().to_hex(),
                "public": id.public().to_hex(),
            })),
            Err(e) => fail(format!("{e} (run init first)")),
        }
    })
}

// ---------------------------------------------------------------------------
// Workspace lifecycle
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn meshdoc_workspace_create(
    store: *const c_char,
    pass: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass) = match (req(store, "store"), req(pass, "pass")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        let name = match req(name, "name") {
            Ok(n) => n,
            Err(e) => return fail(e),
        };
        let s = match Store::open(PathBuf::from(store)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        let identity = if s.has_identity() {
            match s.load_identity(pass) {
                Ok(i) => i,
                Err(e) => return fail(e.to_string()),
            }
        } else {
            let i = Identity::generate();
            if let Err(e) = s.save_identity(&i, pass) {
                return fail(e.to_string());
            }
            i
        };
        match Node::create_workspace(identity, name) {
            Ok(node) => {
                if let Err(e) = s.save_node(&node, pass) {
                    return fail(e.to_string());
                }
                ok(json!({
                    "workspace_id": node.workspace_id.to_hex(),
                    "name": node.workspace_name(),
                    "epoch": node.epoch().as_u32(),
                }))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_workspace_list(store: *const c_char, pass: *const c_char) -> *mut c_char {
    guard(|| {
        let (store, pass) = match (req(store, "store"), req(pass, "pass")) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        let s = match Store::open(PathBuf::from(store)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        let _identity = match s.load_identity(pass) {
            Ok(i) => i,
            Err(e) => return fail(format!("{e} (run init first)")),
        };
        let ids = match s.list_workspaces() {
            Ok(l) => l,
            Err(e) => return fail(e.to_string()),
        };
        let mut out = Vec::new();
        for id in ids {
            match s.load_node(identity_from(&s, pass), id, pass) {
                Ok(n) => out.push(json!({
                    "id": id.to_hex(),
                    "name": n.workspace_name(),
                    "epoch": n.epoch().as_u32(),
                    "docs": n.doc_ids().len(),
                    "members": n.members().len(),
                })),
                Err(_) => out.push(json!({"id": id.to_hex(), "name": "", "epoch": 0, "docs": 0, "members": 0})),
            }
        }
        ok(json!({ "workspaces": out }))
    })
}

fn identity_from(store: &Store, pass: &str) -> Identity {
    store.load_identity(pass).unwrap_or_else(|_| Identity::generate())
}

#[no_mangle]
pub extern "C" fn meshdoc_workspace_invite(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    joiner_public_hex: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws) = match (req(store, "store"), req(pass, "pass"), req(ws, "ws")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return fail("store, pass and ws are required"),
        };
        let joiner_hex = match req(joiner_public_hex, "joiner_public_hex") {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let joiner = match PublicIdentity::from_hex(joiner_hex) {
            Some(j) => j,
            None => return fail("joiner_public_hex must be 128 hex characters"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        match ctx.node.invite_for(&joiner) {
            Ok(inv) => {
                let token = inv.to_token();
                if let Err(e) = ctx.save() {
                    return fail(e);
                }
                ok(json!({"token": token, "bytes": token.len()}))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_workspace_join(
    store: *const c_char,
    pass: *const c_char,
    token: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, token) = match (req(store, "store"), req(pass, "pass"), req(token, "token")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return fail("store, pass and token are required"),
        };
        let s = match Store::open(PathBuf::from(store)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        let identity = if s.has_identity() {
            match s.load_identity(pass) {
                Ok(i) => i,
                Err(e) => return fail(e.to_string()),
            }
        } else {
            let i = Identity::generate();
            if let Err(e) = s.save_identity(&i, pass) {
                return fail(e.to_string());
            }
            i
        };
        let invite = match Invite::from_token(token) {
            Ok(i) => i,
            Err(e) => return fail(e.to_string()),
        };
        match Node::join_from_invite(identity, &invite) {
            Ok(node) => {
                if let Err(e) = s.save_node(&node, pass) {
                    return fail(e.to_string());
                }
                ok(json!({
                    "workspace_id": node.workspace_id.to_hex(),
                    "name": node.workspace_name(),
                    "members": node.members().len(),
                }))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn meshdoc_state(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws) = match (req(store, "store"), req(pass, "pass"), req(ws, "ws")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return fail("store, pass and ws are required"),
        };
        let ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let mut docs = Vec::new();
        for did in ctx.node.doc_ids() {
            if let Some(doc) = ctx.node.doc(&did) {
                let summary = match doc.ty() {
                    DocType::Checklist => {
                        let (done, total) = doc.checklist_progress();
                        format!("{done}/{total} done")
                    }
                    DocType::SupplyForm => format!("{} fields", doc.iter_fields().len()),
                    DocType::GeoPoint => {
                        let g = doc.location();
                        format!("{:.4}, {:.4} {}", g.lat, g.lon, g.label)
                    }
                    DocType::Note => format!("{} chars", doc.text().chars().count()),
                };
                docs.push(json!({
                    "id": did.to_hex(),
                    "type": doc.ty().as_str(),
                    "title": doc.title(),
                    "summary": summary,
                }));
            }
        }
        ok(json!({
            "workspace_id": ctx.node.workspace_id.to_hex(),
            "name": ctx.node.workspace_name(),
            "epoch": ctx.node.epoch().as_u32(),
            "members": ctx.node.members().len(),
            "documents": docs,
            "changes": ctx.node.change_count(),
            "pending": ctx.node.pending_count(),
        }))
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_document(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    doc_id: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws, did) = match (
            req(store, "store"),
            req(pass, "pass"),
            req(ws, "ws"),
            req(doc_id, "doc_id"),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            _ => return fail("store, pass, ws and doc_id are required"),
        };
        let ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let id = match DocId::from_hex(did) {
            Some(i) => i,
            None => return fail("doc_id must be 32 hex characters"),
        };
        let doc = match ctx.node.doc(&id) {
            Some(d) => d,
            None => return fail("no such document"),
        };
        let base = json!({
            "id": id.to_hex(),
            "type": doc.ty().as_str(),
            "title": doc.title(),
        });
        let mut obj = base.as_object().cloned().unwrap_or_default();
        match doc.ty() {
            DocType::Checklist => {
                let items: Vec<_> = doc
                    .iter_items()
                    .into_iter()
                    .map(|i| json!({"id": i.id, "text": i.text, "done": i.done}))
                    .collect();
                obj.insert("items".into(), json!(items));
            }
            DocType::SupplyForm => {
                let fields: Vec<_> = doc
                    .iter_fields()
                    .into_iter()
                    .map(|f| json!({"id": f.id, "value": f.value, "unit": f.unit}))
                    .collect();
                obj.insert("fields".into(), json!(fields));
            }
            DocType::GeoPoint => {
                let g = doc.location();
                obj.insert("lat".into(), json!(g.lat));
                obj.insert("lon".into(), json!(g.lon));
                obj.insert("label".into(), json!(g.label));
                obj.insert("kind".into(), json!(g.kind));
            }
            DocType::Note => {
                obj.insert("text".into(), json!(doc.text()));
            }
        }
        ok(serde_json::Value::Object(obj))
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_doc_create(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    ty: *const c_char,
    title: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws, ty, title) = match (
            req(store, "store"),
            req(pass, "pass"),
            req(ws, "ws"),
            req(ty, "type"),
            req(title, "title"),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e)) => (a, b, c, d, e),
            _ => return fail("store, pass, ws, type and title are required"),
        };
        let dtype = match DocType::parse(ty) {
            Some(t) => t,
            None => return fail("type must be checklist|supply_form|geo_point|note"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        match ctx.node.create_doc(dtype, title) {
            Ok(id) => {
                if let Err(e) = ctx.save() {
                    return fail(e);
                }
                ok(json!({"doc_id": id.to_hex()}))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_checklist_add(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    doc_id: *const c_char,
    text: *const c_char,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws, did, text) = match (
            req(store, "store"),
            req(pass, "pass"),
            req(ws, "ws"),
            req(doc_id, "doc_id"),
            req(text, "text"),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e)) => (a, b, c, d, e),
            _ => return fail("store, pass, ws, doc_id and text are required"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let id = match DocId::from_hex(did) {
            Some(i) => i,
            None => return fail("doc_id must be 32 hex characters"),
        };
        match ctx.node.add_checklist_item_id(id, text) {
            Ok((_, item)) => {
                if let Err(e) = ctx.save() {
                    return fail(e);
                }
                ok(json!({"item_id": item}))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_checklist_toggle(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    doc_id: *const c_char,
    item_id: *const c_char,
    done: i32,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws, did, iid) = match (
            req(store, "store"),
            req(pass, "pass"),
            req(ws, "ws"),
            req(doc_id, "doc_id"),
            req(item_id, "item_id"),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e)) => (a, b, c, d, e),
            _ => return fail("store, pass, ws, doc_id and item_id are required"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let id = match DocId::from_hex(did) {
            Some(i) => i,
            None => return fail("doc_id must be 32 hex characters"),
        };
        match ctx.node.toggle_checklist_item(id, iid, done != 0) {
            Ok(_) => {
                if let Err(e) = ctx.save() {
                    return fail(e);
                }
                ok(json!({"ok": true}))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn meshdoc_sync_pull(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    host: *const c_char,
    port: u16,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws, host) = match (
            req(store, "store"),
            req(pass, "pass"),
            req(ws, "ws"),
            req(host, "host"),
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            _ => return fail("store, pass, ws and host are required"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let addr = match format!("{host}:{port}").parse() {
            Ok(a) => a,
            Err(_) => return fail(format!("invalid peer address {host}:{port}")),
        };
        let mut stream = match TcpMesh::connect(addr) {
            Ok(s) => s,
            Err(e) => return fail(format!("cannot reach {host}:{port}: {e}")),
        };
        let mut reader = match stream.try_clone() {
            Ok(r) => r,
            Err(e) => return fail(e.to_string()),
        };
        match sync::pull(&mut reader, &mut stream, &mut ctx.node) {
            Ok(stats) => {
                if let Err(e) = ctx.save() {
                    return fail(e);
                }
                ok(json!({
                    "received": stats.received,
                    "applied": stats.applied,
                    "pages": stats.pages,
                }))
            }
            Err(e) => fail(e.to_string()),
        }
    })
}

#[no_mangle]
pub extern "C" fn meshdoc_sync_serve(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    port: u16,
    max_sessions: i32,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws) = match (req(store, "store"), req(pass, "pass"), req(ws, "ws")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return fail("store, pass and ws are required"),
        };
        let mut ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let mesh = match TcpMesh::bind(port) {
            Ok(m) => m,
            Err(e) => return fail(format!("cannot listen on port {port}: {e}")),
        };
        let bound = mesh.local_addr().port();
        let mut served = 0i32;
        let mut sent = 0usize;
        loop {
            let stream = match mesh.accept_one() {
                Ok(s) => s,
                Err(e) => return fail(format!("accept failed: {e}")),
            };
            let mut reader = match stream.try_clone() {
                Ok(r) => r,
                Err(e) => return fail(e.to_string()),
            };
            let mut writer = stream;
            match sync::serve(&mut reader, &mut writer, &mut ctx.node) {
                Ok(stats) => {
                    served += 1;
                    sent += stats.sent;
                    if let Err(e) = ctx.save() {
                        return fail(e);
                    }
                }
                Err(_) => { /* peer dropped; keep serving */ }
            }
            if max_sessions > 0 && served >= max_sessions {
                break;
            }
        }
        ok(json!({"served": served, "sent": sent, "port": bound}))
    })
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The rotating BLE advertisement tag for this workspace at `unix_seconds`.
#[no_mangle]
pub extern "C" fn meshdoc_discovery_key(
    store: *const c_char,
    pass: *const c_char,
    ws: *const c_char,
    unix_seconds: u64,
) -> *mut c_char {
    guard(|| {
        let (store, pass, ws) = match (req(store, "store"), req(pass, "pass"), req(ws, "ws")) {
            (Ok(a), Ok(b), Ok(c)) => (a, b, c),
            _ => return fail("store, pass and ws are required"),
        };
        let ctx = match Ctx::open(store, pass, ws) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let epoch: Epoch = ctx.node.epoch();
        let key = match ctx.node.epoch_key(epoch) {
            Some(k) => k,
            None => return fail(format!("no workspace key for epoch {epoch}")),
        };
        // Fold the local change count into the state hint so peers can tell whether
        // there is anything to gain from connecting.
        let hint = ctx.node.change_count().to_le_bytes();
        let tag = ble::discovery_tag(&key, unix_seconds, Some(&hint));
        ok(json!({
            "uuid": tag.uuid_hex(),
            "bucket": tag.bucket,
            "payload": tag.payload_hex(),
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_a_json_string() {
        let p = meshdoc_version();
        assert!(!p.is_null());
        unsafe {
            let s = CStr::from_ptr(p).to_str().unwrap().to_string();
            meshdoc_free(p);
            assert!(s.starts_with('"') && s.ends_with('"'));
        }
    }

    #[test]
    fn null_arguments_produce_an_error_not_a_crash() {
        let p = meshdoc_state(std::ptr::null(), std::ptr::null(), std::ptr::null());
        assert!(p.is_null());
        let e = meshdoc_last_error();
        unsafe {
            let msg = CStr::from_ptr(e).to_str().unwrap().to_string();
            meshdoc_free(e);
            assert!(!msg.is_empty());
        }
    }
}
