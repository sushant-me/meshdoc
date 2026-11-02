//! Integration tests for the MeshDoc relay.
//!
//! Each test starts a real relay on an ephemeral port in its own temp data
//! directory, then drives it over TCP with hand-written HTTP/1.1 requests. No
//! mocking: this exercises the actual server, storage and socket code.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::Value;

use meshdoc_relay::{Config, Relay};

const WSID_A: &str = "0123456789abcdef0123456789abcdef";
const WSID_B: &str = "fedcba9876543210fedcba9876543210";
const DEFAULT_MAX: usize = 8 * 1024 * 1024;

// Harness

struct Harness {
    relay: Option<Relay>,
    addr: SocketAddr,
    dir: PathBuf,
    max_blob_bytes: usize,
}

impl Harness {
    fn new() -> Harness {
        Harness::with_max(DEFAULT_MAX)
    }

    fn with_max(max_blob_bytes: usize) -> Harness {
        let dir = unique_dir();
        Harness::from_dir(dir, max_blob_bytes)
    }

    fn from_dir(dir: PathBuf, max_blob_bytes: usize) -> Harness {
        let mut harness = Harness {
            relay: None,
            addr: "127.0.0.1:0".parse().unwrap(),
            dir,
            max_blob_bytes,
        };
        harness.start_relay();
        harness
    }

    fn start_relay(&mut self) {
        let relay = Relay::start(Config {
            data_dir: self.dir.clone(),
            port: 0,
            bind: "127.0.0.1".to_string(),
            max_blob_bytes: self.max_blob_bytes,
            ttl_hours: None,
        })
        .expect("relay should start");
        self.addr = relay.addr();
        self.relay = Some(relay);
    }

    /// Stop the server and start a fresh one on the same data directory.
    fn restart(&mut self) {
        if let Some(mut relay) = self.relay.take() {
            relay.stop();
        }
        self.start_relay();
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn data_dir(&self) -> &PathBuf {
        &self.dir
    }

    fn get(&self, path: &str) -> HttpResponse {
        http_request(self.addr, "GET", path, None)
    }

    fn delete(&self, path: &str) -> HttpResponse {
        http_request(self.addr, "DELETE", path, None)
    }

    fn post(&self, wsid: &str, body: &[u8]) -> HttpResponse {
        http_request(
            self.addr,
            "POST",
            &format!("/v1/ws/{}/blobs", wsid),
            Some(body),
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(mut relay) = self.relay.take() {
            relay.stop();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_dir() -> PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "meshdoc-relay-test-{}-{}-{}",
        std::process::id(),
        n,
        nanos
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// Minimal blocking HTTP/1.1 client (no extra dependencies)

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(&self) -> Value {
        let content_type = self
            .headers
            .get("content-type")
            .cloned()
            .unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "expected application/json, got {:?}",
            content_type
        );
        serde_json::from_slice(&self.body).unwrap_or_else(|err| {
            panic!(
                "response was not valid JSON: {err}; body={:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

fn http_request(addr: SocketAddr, method: &str, path: &str, body: Option<&[u8]>) -> HttpResponse {
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(10)).expect("connect to relay");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("set read timeout");
    let _ = stream.set_nodelay(true);

    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(data) = body {
        head.push_str("Content-Type: application/octet-stream\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", data.len()));
    } else if method != "GET" {
        head.push_str("Content-Length: 0\r\n");
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .expect("write request head");
    if let Some(data) = body {
        stream.write_all(data).expect("write request body");
    }
    stream.flush().expect("flush request");

    read_response(&mut stream)
}

fn read_response(stream: &mut TcpStream) -> HttpResponse {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];

    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).expect("read response headers");
        if read == 0 {
            panic!("connection closed before response headers completed");
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("numeric status code");

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let mut body = buffer[header_end + 4..].to_vec();
    if let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    {
        while body.len() < length {
            let read = stream.read(&mut chunk).expect("read response body");
            if read == 0 {
                panic!(
                    "connection closed mid-body (got {} of {})",
                    body.len(),
                    length
                );
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
    } else {
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => body.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
    }

    HttpResponse {
        status,
        headers,
        body,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn expected_id(data: &[u8]) -> String {
    blake3::hash(data).to_hex().as_str()[..32].to_string()
}

fn b64_decode(value: &str) -> Vec<u8> {
    BASE64.decode(value).expect("valid base64 in response")
}

fn health(h: &Harness) -> Value {
    let response = h.get("/v1/health");
    assert_eq!(response.status, 200);
    response.json()
}

fn head_of(h: &Harness, wsid: &str) -> Value {
    let response = h.get(&format!("/v1/ws/{}/head", wsid));
    assert_eq!(response.status, 200);
    response.json()
}

fn blobs_since(h: &Harness, wsid: &str, since: u64) -> Value {
    let response = h.get(&format!("/v1/ws/{}/blobs?since={}", wsid, since));
    assert_eq!(response.status, 200);
    response.json()
}

// (a) health

#[test]
fn health_returns_ok() {
    let harness = Harness::new();
    let body = health(&harness);

    assert_eq!(body["status"], "ok");
    assert_eq!(
        body["version"],
        env!("CARGO_PKG_VERSION"),
        "version must be the crate version"
    );
    assert_eq!(body["blobs"], 0);
}

// (b) POST then GET since=0, base64 round-trips byte-for-byte

#[test]
fn post_then_get_round_trips_binary() {
    let harness = Harness::new();
    let payload: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();

    let post = harness.post(WSID_A, &payload);
    assert_eq!(post.status, 200);
    let post_json = post.json();
    assert_eq!(post_json["seq"], 1);
    assert_eq!(post_json["id"], expected_id(&payload));

    let list = blobs_since(&harness, WSID_A, 0);
    assert_eq!(list["cursor"], 1);
    assert_eq!(list["more"], false);
    let blobs = list["blobs"].as_array().expect("blobs array");
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0]["seq"], 1);
    assert_eq!(blobs[0]["id"], expected_id(&payload));

    let decoded = b64_decode(blobs[0]["data"].as_str().expect("data string"));
    assert_eq!(decoded, payload, "blob must round-trip byte-for-byte");

    // Cross-check the id against a recomputed blake3 hash.
    assert_eq!(decoded.len(), 10_000);

    // And the workspace head/count reflect it.
    let head = head_of(&harness, WSID_A);
    assert_eq!(head["head"], 1);
    assert_eq!(head["count"], 1);
}

// (c) duplicate POST is idempotent

#[test]
fn duplicate_post_is_idempotent() {
    let harness = Harness::new();
    let payload = b"same encrypted envelope every time".to_vec();

    let first = harness.post(WSID_A, &payload).json();
    let second = harness.post(WSID_A, &payload).json();
    let third = harness.post(WSID_A, &payload).json();

    assert_eq!(first["seq"], 1);
    assert_eq!(second["seq"], 1, "duplicate must return the existing seq");
    assert_eq!(third["seq"], 1);
    assert_eq!(first["id"], second["id"]);

    assert_eq!(health(&harness)["blobs"], 1, "count must not grow");
    assert_eq!(head_of(&harness, WSID_A)["count"], 1);

    let list = blobs_since(&harness, WSID_A, 0);
    assert_eq!(list["blobs"].as_array().unwrap().len(), 1);

    // Same bytes in a *different* workspace are independent.
    let other = harness.post(WSID_B, &payload).json();
    assert_eq!(other["seq"], 1);
    assert_eq!(health(&harness)["blobs"], 2);
}

// (d) GET since=<seq> excludes earlier blobs

#[test]
fn get_since_excludes_earlier_blobs() {
    let harness = Harness::new();
    harness.post(WSID_A, b"first");
    harness.post(WSID_A, b"second");
    harness.post(WSID_A, b"third");

    let all = blobs_since(&harness, WSID_A, 0);
    let seqs: Vec<u64> = all["blobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|blob| blob["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(all["cursor"], 3);
    assert_eq!(all["more"], false);

    let after_one = blobs_since(&harness, WSID_A, 1);
    let seqs: Vec<u64> = after_one["blobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|blob| blob["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![2, 3], "seq 1 must be excluded");
    assert_eq!(after_one["cursor"], 3);

    let after_all = blobs_since(&harness, WSID_A, 3);
    assert_eq!(after_all["blobs"].as_array().unwrap().len(), 0);
    assert_eq!(after_all["cursor"], 3, "cursor is `since` when empty");
    assert_eq!(after_all["more"], false);

    // Unknown workspace: empty, not an error.
    let unknown = blobs_since(&harness, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 0);
    assert_eq!(unknown["blobs"].as_array().unwrap().len(), 0);
    assert_eq!(unknown["cursor"], 0);
}

// (e) restart on the same data dir preserves blobs

#[test]
fn restart_preserves_blobs() {
    let mut harness = Harness::new();
    let payloads: Vec<Vec<u8>> = vec![
        b"envelope one".to_vec(),
        b"envelope two, a bit longer".to_vec(),
        (0..300u16).map(|value| (value % 256) as u8).collect(),
    ];
    for payload in &payloads {
        let response = harness.post(WSID_A, payload);
        assert_eq!(response.status, 200);
    }
    let before = blobs_since(&harness, WSID_A, 0);
    assert_eq!(before["cursor"], 3);

    harness.restart();

    let after = blobs_since(&harness, WSID_A, 0);
    assert_eq!(after["cursor"], 3);
    let blobs = after["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 3);
    for (index, payload) in payloads.iter().enumerate() {
        let seq = (index + 1) as u64;
        assert_eq!(blobs[index]["seq"], seq);
        assert_eq!(blobs[index]["id"], expected_id(payload));
        assert_eq!(b64_decode(blobs[index]["data"].as_str().unwrap()), *payload);
    }

    // Health count survives too, and new posts continue after the restored max.
    assert_eq!(health(&harness)["blobs"], 3);
    let next = harness.post(WSID_A, b"after restart").json();
    assert_eq!(next["seq"], 4);
    assert_eq!(head_of(&harness, WSID_A)["head"], 4);

    // Restarting twice must not renumber anything.
    harness.restart();
    let after_restart = blobs_since(&harness, WSID_A, 3);
    assert_eq!(after_restart["cursor"], 4);
    assert_eq!(after_restart["blobs"].as_array().unwrap().len(), 1);
    assert_eq!(
        b64_decode(after_restart["blobs"][0]["data"].as_str().unwrap()),
        b"after restart".to_vec()
    );

    // Durable on disk: one .blob file per blob plus the seq counter.
    let workspace_dir = harness.data_dir().join(WSID_A);
    let names: Vec<String> = fs::read_dir(&workspace_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let blob_files = names.iter().filter(|name| name.ends_with(".blob")).count();
    assert_eq!(blob_files, 4);
    assert!(names.contains(&"seq".to_string()));
    assert_eq!(
        fs::read_to_string(workspace_dir.join("seq"))
            .unwrap()
            .trim(),
        "4"
    );
}

// (f) malformed wsid -> 401/400, and no path traversal

#[test]
fn malformed_wsid_rejected() {
    let harness = Harness::new();

    let bad_wsids = [
        "not-a-valid-workspace-id",
        "0123456789ABCDEF0123456789abcdef", // uppercase is invalid
        "0123456789abcdef0123456789abcde",  // 31 chars
        "0123456789abcdef0123456789abcdef0", // 33 chars
        "0123456789abcdef0123456789abcdeg", // non-hex
        "..",
        ".",
    ];

    for wsid in bad_wsids {
        let response = harness.post(wsid, b"body");
        assert!(
            response.status == 401 || response.status == 400,
            "malformed wsid {:?} should be rejected with 401/400, got {}",
            wsid,
            response.status
        );
        let json = response.json();
        assert!(json["error"].is_string());

        // Read endpoints must reject the same way.
        let head = harness.get(&format!("/v1/ws/{}/head", wsid));
        assert!(
            head.status == 401 || head.status == 400,
            "malformed wsid {:?} on /head got {}",
            wsid,
            head.status
        );
    }

    // Path traversal attempt: encoded / unencoded dot segments must never reach
    // the filesystem. A traversal path is not routable at all (404), and no
    // workspace directory is ever created outside the data dir.
    let traversal = harness.get("/v1/ws/../../mesh-doc-escape/head");
    // Extra segments are not routable at all; anything 4xx is fine here.
    let deep_traversal = harness.get("/v1/ws/../../../../etc/blobs?since=0");
    assert!(
        (400..500).contains(&deep_traversal.status),
        "deep traversal attempt should be rejected, got {}",
        deep_traversal.status
    );
    assert!(
        traversal.status == 404 || traversal.status == 401 || traversal.status == 400,
        "traversal attempt should be rejected, got {}",
        traversal.status
    );

    // Data dir contains no workspace dirs after only-invalid traffic.
    let entries: Vec<String> = fs::read_dir(harness.data_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.is_empty(),
        "invalid requests must not create storage, found {:?}",
        entries
    );
    assert!(!harness
        .data_dir()
        .parent()
        .unwrap()
        .join("mesh-doc-escape")
        .exists());
}

// (g) oversize body -> 413

#[test]
fn oversize_body_rejected() {
    let harness = Harness::with_max(1024);
    // Exactly one byte over the limit: the whole body is consumed (bounded
    // read of max+1 bytes), so the 413 response is delivered cleanly.
    let oversize = vec![0xAAu8; 1025];
    let response = harness.post(WSID_A, &oversize);
    assert_eq!(response.status, 413, "body > max_blob_bytes must be 413");
    assert!(response.json()["error"].is_string());
    assert_eq!(health(&harness)["blobs"], 0, "rejected body is not stored");

    // Exactly at the limit is accepted.
    let at_limit = vec![0xBBu8; 1024];
    let response = harness.post(WSID_A, &at_limit);
    assert_eq!(response.status, 200);
    assert_eq!(health(&harness)["blobs"], 1);
}

// (h) concurrency: 8 threads x 25 blobs, no duplicate seqs

#[test]
fn concurrent_posts_get_distinct_seqs() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 25;
    let harness = Harness::new();
    let addr = harness.addr();

    let mut handles = Vec::new();
    for thread_index in 0..THREADS {
        handles.push(thread::spawn(move || {
            let mut assigned = Vec::with_capacity(PER_THREAD);
            for blob_index in 0..PER_THREAD {
                let payload = format!("thread-{thread_index}-blob-{blob_index}").into_bytes();
                let response = http_request(
                    addr,
                    "POST",
                    &format!("/v1/ws/{}/blobs", WSID_A),
                    Some(&payload),
                );
                assert_eq!(
                    response.status, 200,
                    "concurrent POST failed: {:?}",
                    response.body
                );
                let json = response.json();
                assigned.push(json["seq"].as_u64().expect("seq is a number"));
                assert_eq!(json["id"], expected_id(&payload));
            }
            assigned
        }));
    }

    let mut all_seqs = Vec::new();
    for handle in handles {
        all_seqs.extend(handle.join().expect("worker thread must not panic"));
    }

    assert_eq!(all_seqs.len(), THREADS * PER_THREAD);
    let distinct: HashSet<u64> = all_seqs.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        THREADS * PER_THREAD,
        "every concurrent POST must get a distinct seq"
    );
    assert_eq!(
        *distinct.iter().min().unwrap(),
        1,
        "sequence numbers start at 1"
    );
    assert_eq!(
        *distinct.iter().max().unwrap(),
        (THREADS * PER_THREAD) as u64,
        "sequence numbers must be contiguous with no gaps"
    );

    assert_eq!(health(&harness)["blobs"], THREADS * PER_THREAD);

    // GET returns exactly 200 blobs, all decodable and unique.
    let list = blobs_since(&harness, WSID_A, 0);
    let blobs = list["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 200);
    assert_eq!(list["cursor"], 200);
    assert_eq!(list["more"], false, "exactly one page, not truncated");

    let mut decoded_payloads = HashSet::new();
    let mut seqs_in_order = Vec::new();
    for blob in blobs {
        seqs_in_order.push(blob["seq"].as_u64().unwrap());
        let bytes = b64_decode(blob["data"].as_str().unwrap());
        decoded_payloads.insert(bytes);
    }
    assert_eq!(decoded_payloads.len(), 200, "all payloads are distinct");
    let mut sorted = seqs_in_order.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (1..=200).collect::<Vec<u64>>(), "ordered ascending");
}

// Extra: 256-per-page limit and "more"

#[test]
fn pagination_limits_to_256_and_sets_more() {
    let harness = Harness::new();
    const TOTAL: u64 = 300;
    for index in 0..TOTAL {
        let payload = format!("page-blob-{index}");
        let response = harness.post(WSID_A, payload.as_bytes());
        assert_eq!(response.status, 200);
    }

    let first_page = blobs_since(&harness, WSID_A, 0);
    assert_eq!(first_page["blobs"].as_array().unwrap().len(), 256);
    assert_eq!(first_page["cursor"], 256);
    assert_eq!(first_page["more"], true);

    let second_page = blobs_since(&harness, WSID_A, 256);
    assert_eq!(second_page["blobs"].as_array().unwrap().len(), 44);
    assert_eq!(second_page["cursor"], 300);
    assert_eq!(second_page["more"], false);

    // Walk the whole workspace the way a client would and verify no gaps.
    let mut seen = Vec::new();
    let mut since = 0;
    loop {
        let page = blobs_since(&harness, WSID_A, since);
        for blob in page["blobs"].as_array().unwrap() {
            seen.push(blob["seq"].as_u64().unwrap());
        }
        since = page["cursor"].as_u64().unwrap();
        if !page["more"].as_bool().unwrap() {
            break;
        }
    }
    assert_eq!(seen, (1..=TOTAL).collect::<Vec<u64>>());
}

// Extra: delete + head semantics

#[test]
fn delete_blob_and_head() {
    let mut harness = Harness::new();
    let first = harness.post(WSID_A, b"delete me").json();
    let second = harness.post(WSID_A, b"keep me").json();
    let first_id = first["id"].as_str().unwrap().to_string();

    let head = head_of(&harness, WSID_A);
    assert_eq!(head["head"], 2);
    assert_eq!(head["count"], 2);

    let deleted = harness.delete(&format!("/v1/ws/{}/blobs/{}", WSID_A, first_id));
    assert_eq!(deleted.status, 200);
    assert_eq!(deleted.json()["deleted"], true);

    // Idempotency does not apply to DELETE: gone means 404.
    let again = harness.delete(&format!("/v1/ws/{}/blobs/{}", WSID_A, first_id));
    assert_eq!(again.status, 404);

    let head = head_of(&harness, WSID_A);
    assert_eq!(head["count"], 1);
    assert_eq!(head["head"], 2, "head is the max live seq");

    let list = blobs_since(&harness, WSID_A, 0);
    let blobs = list["blobs"].as_array().unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0]["id"], second["id"]);

    // Sequence numbers never rewind after a delete + restart.
    harness.restart();
    let after_restart = harness.post(WSID_A, b"newest").json();
    assert_eq!(after_restart["seq"], 3);

    // Unknown blob id in a valid shape -> 404; malformed id -> 400.
    let missing = "0123456789abcdef0123456789abcdef";
    let unknown = harness.delete(&format!("/v1/ws/{}/blobs/{}", WSID_A, missing));
    assert_eq!(unknown.status, 404);
    let malformed = harness.delete(&format!("/v1/ws/{}/blobs/not-hex", WSID_A));
    assert_eq!(malformed.status, 400);
}

// Extra: unknown workspace head is zero, and non-hex/uppercase ids rejected

#[test]
fn empty_workspace_and_id_validation() {
    let harness = Harness::new();
    let head = head_of(&harness, WSID_B);
    assert_eq!(head["head"], 0);
    assert_eq!(head["count"], 0);

    let uppercase = "0123456789ABCDEF0123456789ABCDEF";
    let response = harness.delete(&format!("/v1/ws/{}/blobs/{}", WSID_A, uppercase));
    assert_eq!(response.status, 400);
}
