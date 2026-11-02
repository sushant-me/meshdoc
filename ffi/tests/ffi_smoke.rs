//! End-to-end test of the C ABI, exercised exactly as the Android client uses it:
//! through the exported functions, JSON in and JSON out.

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use meshdoc_ffi::*;

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Take ownership of a returned string, or panic with the last error.
fn take(ptr: *mut std::ffi::c_char) -> serde_json::Value {
    if ptr.is_null() {
        let e = meshdoc_last_error();
        let msg = unsafe {
            let m = CStr::from_ptr(e).to_str().unwrap().to_string();
            meshdoc_free(e);
            m
        };
        panic!("native call failed: {msg}");
    }
    let s = unsafe {
        let out = CStr::from_ptr(ptr).to_str().unwrap().to_string();
        meshdoc_free(ptr);
        out
    };
    serde_json::from_str(&s).unwrap_or_else(|e| panic!("bad json {s}: {e}"))
}

fn tmpdir(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!(
        "meshdoc-ffi-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p.to_string_lossy().to_string()
}

const PASS: &str = "ffi-test-passphrase";

#[test]
fn full_offline_sync_through_the_c_abi() {
    let a = tmpdir("a");
    let b = tmpdir("b");
    let (sa, sb, sp) = (cs(&a), cs(&b), cs(PASS));

    // 1. Both devices get identities.
    let ida = take(meshdoc_init(sa.as_ptr(), sp.as_ptr()));
    let idb = take(meshdoc_init(sb.as_ptr(), sp.as_ptr()));
    assert!(ida["created"].as_bool().unwrap());
    assert!(idb["created"].as_bool().unwrap());
    let b_public = idb["public"].as_str().unwrap().to_string();

    // 2. A creates a workspace and invites B.
    let ws = take(meshdoc_workspace_create(
        sa.as_ptr(),
        sp.as_ptr(),
        cs("Field Ops").as_ptr(),
    ));
    let wsid = ws["workspace_id"].as_str().unwrap().to_string();
    assert_eq!(ws["name"], "Field Ops");

    let tok = take(meshdoc_workspace_invite(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&b_public).as_ptr(),
    ));
    let token = tok["token"].as_str().unwrap().to_string();
    assert!(tok["bytes"].as_u64().unwrap() > 1000);

    let joined = take(meshdoc_workspace_join(
        sb.as_ptr(),
        sp.as_ptr(),
        cs(&token).as_ptr(),
    ));
    assert_eq!(joined["workspace_id"].as_str().unwrap(), wsid);
    assert_eq!(joined["members"].as_u64().unwrap(), 2);

    // 3. A records a checklist.
    let doc = take(meshdoc_doc_create(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs("checklist").as_ptr(),
        cs("Trapped persons").as_ptr(),
    ));
    let doc_id = doc["doc_id"].as_str().unwrap().to_string();
    let item = take(meshdoc_checklist_add(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&doc_id).as_ptr(),
        cs("Ward 3, two adults, one child").as_ptr(),
    ));
    let item_id = item["item_id"].as_str().unwrap().to_string();
    assert_eq!(item_id.len(), 12);

    // 4. B has nothing yet.
    let empty = take(meshdoc_state(sb.as_ptr(), sp.as_ptr(), cs(&wsid).as_ptr()));
    assert_eq!(empty["documents"].as_array().unwrap().len(), 0);

    // 5. A serves; B pulls. This is a real TCP session.
    let port: u16 = 17700 + (std::process::id() % 200) as u16;
    let (a_path, b_path) = (a.clone(), b.clone());
    let (tx, rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (sa, sp) = (cs(&a_path), cs(PASS));
        let ws = cs("");
        tx.send(()).unwrap();
        take(meshdoc_sync_serve(
            sa.as_ptr(),
            sp.as_ptr(),
            ws.as_ptr(),
            port,
            1,
        ))
    });
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::sleep(Duration::from_millis(300));

    let pulled = take(meshdoc_sync_pull(
        cs(&b_path).as_ptr(),
        cs(PASS).as_ptr(),
        cs(&wsid).as_ptr(),
        cs("127.0.0.1").as_ptr(),
        port,
    ));
    assert!(
        pulled["received"].as_u64().unwrap() > 0,
        "expected changes: {pulled}"
    );
    let served = server.join().unwrap();
    assert!(served["served"].as_u64().unwrap() >= 1);

    // 6. B now sees the document and the item.
    let bdoc = take(meshdoc_document(
        sb.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&doc_id).as_ptr(),
    ));
    let items = bdoc["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["text"], "Ward 3, two adults, one child");
    assert_eq!(items[0]["done"], false);

    // 7. B ticks it while apart; A pulls and converges.
    take(meshdoc_checklist_toggle(
        sb.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&doc_id).as_ptr(),
        cs(&item_id).as_ptr(),
        1,
    ));
    let port2 = port + 1;
    let (b2, a2) = (b.clone(), a.clone());
    let (tx2, rx2) = mpsc::channel();
    let server2 = thread::spawn(move || {
        let (sb, sp) = (cs(&b2), cs(PASS));
        let ws = cs("");
        tx2.send(()).unwrap();
        take(meshdoc_sync_serve(
            sb.as_ptr(),
            sp.as_ptr(),
            ws.as_ptr(),
            port2,
            1,
        ))
    });
    rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::sleep(Duration::from_millis(300));
    take(meshdoc_sync_pull(
        cs(&a2).as_ptr(),
        cs(PASS).as_ptr(),
        cs(&wsid).as_ptr(),
        cs("127.0.0.1").as_ptr(),
        port2,
    ));
    server2.join().unwrap();

    // 8. Both sides now agree.
    let adoc = take(meshdoc_document(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&doc_id).as_ptr(),
    ));
    let bdoc2 = take(meshdoc_document(
        sb.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs(&doc_id).as_ptr(),
    ));
    assert_eq!(adoc["items"], bdoc2["items"], "replicas must converge");
    assert_eq!(adoc["items"][0]["done"], true);

    // 9. Discovery tag rotates and is per-workspace.
    let t1 = take(meshdoc_discovery_key(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        1_700_000_000,
    ));
    let t2 = take(meshdoc_discovery_key(
        sa.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        1_700_000_000 + 15 * 60,
    ));
    assert_eq!(t1["uuid"].as_str().unwrap().len(), 32);
    assert_ne!(t1["uuid"], t2["uuid"], "the discovery UUID must rotate");

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn errors_are_reported_not_crashed() {
    // Missing store directory and missing passphrase both surface as errors.
    let bad = cs("/nonexistent/meshdoc/should/not/exist");
    let p = meshdoc_identity(bad.as_ptr(), cs(PASS).as_ptr());
    assert!(p.is_null());
    let e = meshdoc_last_error();
    unsafe {
        let msg = CStr::from_ptr(e).to_str().unwrap().to_string();
        meshdoc_free(e);
        assert!(!msg.is_empty(), "an error message must be set");
    }
}

#[test]
fn invalid_inputs_are_rejected() {
    let dir = tmpdir("bad");
    let (s, sp) = (cs(&dir), cs(PASS));
    take(meshdoc_init(s.as_ptr(), sp.as_ptr()));

    // Malformed joiner key.
    let ws = take(meshdoc_workspace_create(
        s.as_ptr(),
        sp.as_ptr(),
        cs("w").as_ptr(),
    ));
    let wsid = ws["workspace_id"].as_str().unwrap().to_string();
    let p = meshdoc_workspace_invite(
        s.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs("not-hex").as_ptr(),
    );
    assert!(p.is_null());

    // Malformed document type.
    let p = meshdoc_doc_create(
        s.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs("bogus").as_ptr(),
        cs("t").as_ptr(),
    );
    assert!(p.is_null());

    // Malformed document id.
    let p = meshdoc_document(
        s.as_ptr(),
        sp.as_ptr(),
        cs(&wsid).as_ptr(),
        cs("zz").as_ptr(),
    );
    assert!(p.is_null());

    let _ = std::fs::remove_dir_all(&dir);
}
