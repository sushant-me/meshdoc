//! Integration tests for the MeshDoc relay client.
//!
//! Most tests drive [`RelayClient`] against a fake relay that lives in this
//! file: a `std::net::TcpListener` on an ephemeral port that speaks canned
//! HTTP/1.1 responses. That keeps the suite deterministic and needs no new
//! dependency, no external process and no port assumptions.
//!
//! One further test exercises a real `meshdoc-relay` binary when one is
//! available (a release/debug build under `relay/target/`, or the path in
//! `MESHDOC_RELAY_BIN`). It is skipped, not failed, when no binary is present,
//! so a fresh checkout stays green.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use meshdoc::relay::{Health, Posted, RelayClient, RelayError};
use meshdoc::WorkspaceId;

const WSID_HEX: &str = "0123456789abcdef0123456789abcdef";
const WSID_B_HEX: &str = "fedcba9876543210fedcba9876543210";

fn ws() -> WorkspaceId {
    WorkspaceId::from_hex(WSID_HEX).expect("test workspace id is valid hex")
}

fn ws_b() -> WorkspaceId {
    WorkspaceId::from_hex(WSID_B_HEX).expect("test workspace id is valid hex")
}

/// The content id the relay assigns: first 32 hex chars of blake3(body).
fn id_of(data: &[u8]) -> String {
    let hex = blake3::hash(data).to_hex();
    hex.as_str()[..32].to_string()
}

/// Build a `GET /blobs?since=` response body.
fn page(cursor: u64, more: bool, blobs: Vec<(u64, Vec<u8>)>) -> Value {
    let blobs: Vec<Value> = blobs
        .into_iter()
        .map(|(seq, data)| {
            json!({
                "seq": seq,
                "id": id_of(&data),
                "data": BASE64.encode(&data),
            })
        })
        .collect();
    json!({ "cursor": cursor, "more": more, "blobs": blobs })
}

// Fake relay: a tiny HTTP/1.1 server with canned responses

/// A request as seen by the fake relay.
#[derive(Clone, Debug)]
struct Request {
    method: String,
    /// Path plus query string, exactly as it appeared on the request line.
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup (the client always sends canonical names,
    /// but a test may want to check framing headers regardless of case).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// First value of a query parameter in `target`.
    fn query(&self, name: &str) -> Option<&str> {
        self.target.split_once('?')?.1.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key == name {
                Some(value)
            } else {
                None
            }
        })
    }
}

/// How the fake relay delimits the response body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// Send `Content-Length` (optionally overridden by `Response::claim_length`).
    ContentLength,
    /// Send `Transfer-Encoding: chunked` and encode the body as chunks.
    Chunked,
    /// Send neither header and close the connection to mark the end.
    UntilClose,
}

/// A canned response.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    framing: Framing,
    /// Declare a different length than the body actually has (truncation tests).
    declared_length: Option<u64>,
    /// Write the body in several small TCP writes with pauses between them.
    dribble: bool,
    /// Render header names in lowercase.
    lowercase_headers: bool,
}

impl Response {
    fn json(status: u16, value: &Value) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: serde_json::to_vec(value).expect("canned JSON response serializes"),
            framing: Framing::ContentLength,
            declared_length: None,
            dribble: false,
            lowercase_headers: false,
        }
    }

    fn bytes(status: u16, body: Vec<u8>) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body,
            framing: Framing::ContentLength,
            declared_length: None,
            dribble: false,
            lowercase_headers: false,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn chunked(mut self) -> Response {
        self.framing = Framing::Chunked;
        self
    }

    fn close_delimited(mut self) -> Response {
        self.framing = Framing::UntilClose;
        self
    }

    fn claim_length(mut self, length: u64) -> Response {
        self.declared_length = Some(length);
        self
    }

    fn dribble(mut self) -> Response {
        self.dribble = true;
        self
    }

    fn lowercase(mut self) -> Response {
        self.lowercase_headers = true;
        self
    }
}

/// A fake relay bound to an ephemeral loopback port.
///
/// Every accepted request is recorded (before the response is sent, so a test
/// that sees the response is guaranteed to see the recorded request too), then
/// answered by `handler`.
struct FakeRelay {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeRelay {
    fn spawn(handler: impl Fn(Request) -> Response + Send + 'static) -> FakeRelay {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake relay");
        listener
            .set_nonblocking(true)
            .expect("put fake relay listener in non-blocking mode");
        let addr = listener.local_addr().expect("fake relay local address");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let handler: Box<dyn Fn(Request) -> Response + Send> = Box::new(handler);

        let worker = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // A client that vanishes mid-request just drops the
                        // connection; nothing for the fake relay to clean up.
                        let _ = serve(stream, handler.as_ref(), &recorded);
                    }
                    Err(ref error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        FakeRelay {
            addr,
            stop,
            worker: Some(worker),
            requests,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn client(&self) -> RelayClient {
        RelayClient::new(&self.base_url()).expect("fake relay URL is valid")
    }

    fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .expect("recorded requests lock")
            .clone()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().expect("recorded requests lock").len()
    }

    fn last_request(&self) -> Request {
        self.requests()
            .pop()
            .expect("fake relay should have recorded a request")
    }
}

impl Drop for FakeRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    handler: &dyn Fn(Request) -> Response,
    recorded: &Mutex<Vec<Request>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = read_request(&mut reader)?;
    // Record before answering: the client cannot observe the response before
    // the request is visible to the test.
    recorded
        .lock()
        .expect("recorded requests lock")
        .push(request.clone());
    let response = handler(request);
    write_response(&mut stream, &response)
}

fn read_request<R: BufRead>(reader: &mut R) -> std::io::Result<Request> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "connection closed before a request line",
        ));
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    loop {
        let mut header_line = String::new();
        reader.read_line(&mut header_line)?;
        let header_line = header_line.trim_end();
        if header_line.is_empty() {
            break;
        }
        let (name, value) = header_line
            .split_once(':')
            .expect("fake relay received a header with a colon");
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;

    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: &Response) -> std::io::Result<()> {
    let mut head = String::new();
    head.push_str(&format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status)
    ));
    for (name, value) in &response.headers {
        if response.lowercase_headers {
            head.push_str(&format!("{}: {}\r\n", name.to_ascii_lowercase(), value));
        } else {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    match response.framing {
        Framing::ContentLength => {
            let length = response
                .declared_length
                .unwrap_or(response.body.len() as u64);
            if response.lowercase_headers {
                head.push_str(&format!("content-length: {length}\r\n"));
            } else {
                head.push_str(&format!("Content-Length: {length}\r\n"));
            }
        }
        Framing::Chunked => head.push_str("Transfer-Encoding: chunked\r\n"),
        Framing::UntilClose => {}
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;

    match response.framing {
        Framing::Chunked => write_chunks(stream, &response.body, response.dribble)?,
        _ => {
            if response.dribble {
                write_dribbled(stream, &response.body)?;
            } else {
                stream.write_all(&response.body)?;
            }
        }
    }
    stream.flush()
}

fn write_chunks(stream: &mut TcpStream, body: &[u8], dribble: bool) -> std::io::Result<()> {
    let parts: Vec<&[u8]> = if body.is_empty() {
        Vec::new()
    } else {
        body.chunks(7).collect()
    };
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            // Include a chunk extension: clients must ignore it.
            stream.write_all(format!("{:x};probe=1\r\n", part.len()).as_bytes())?;
        } else {
            stream.write_all(format!("{:x}\r\n", part.len()).as_bytes())?;
        }
        stream.write_all(part)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        if dribble {
            thread::sleep(Duration::from_millis(1));
        }
    }
    stream.write_all(b"0\r\n\r\n")?;
    Ok(())
}

fn write_dribbled(stream: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let chunk_size = (body.len() / 4).max(1);
    let parts: Vec<&[u8]> = body.chunks(chunk_size).collect();
    for (index, part) in parts.iter().enumerate() {
        stream.write_all(part)?;
        stream.flush()?;
        if index + 1 < parts.len() {
            // Far larger than loopback latency, so the client's reads see only
            // part of the body at a time.
            thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// A listener that accepts connections and then never answers them, to exercise
/// the client's read timeout.
struct SilentServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SilentServer {
    fn spawn() -> SilentServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind silent server");
        listener
            .set_nonblocking(true)
            .expect("put silent listener in non-blocking mode");
        let addr = listener.local_addr().expect("silent server local address");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut held: Vec<TcpStream> = Vec::new();
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => held.push(stream),
                    Err(ref error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
            drop(held);
        });
        SilentServer {
            addr,
            stop,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for SilentServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// health

#[test]
fn health_parses_the_relay_response() {
    let server = FakeRelay::spawn(|_| {
        Response::json(
            200,
            &json!({"status": "ok", "version": "9.9.9", "blobs": 42}),
        )
    });

    let health = server.client().health().expect("health should parse");
    assert_eq!(
        health,
        Health {
            status: "ok".to_string(),
            version: "9.9.9".to_string(),
            blobs: 42,
        }
    );

    let request = server.last_request();
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/v1/health");
    let authority = server.addr.to_string();
    assert_eq!(request.header("host"), Some(authority.as_str()));
    assert_eq!(request.header("connection"), Some("close"));
    assert_eq!(request.header("content-length"), Some("0"));
}

#[test]
fn malformed_health_json_is_a_protocol_error() {
    let server = FakeRelay::spawn(|_| Response::bytes(200, b"<html>not json</html>".to_vec()));

    let error = server.client().health().expect_err("must not parse");
    assert!(
        matches!(error, RelayError::Protocol(_)),
        "unexpected error: {error:?}"
    );
}

#[test]
fn non_utf8_error_body_is_lossy_instead_of_panicking() {
    let server = FakeRelay::spawn(|_| Response::bytes(500, vec![0xff, 0xfe, b'a']));

    let error = server.client().health().expect_err("must surface status");
    match error {
        RelayError::Status { code, body } => {
            assert_eq!(code, 500);
            assert!(body.contains('a'), "lossy body should keep ASCII: {body:?}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// POST

#[test]
fn post_blob_returns_seq_and_id_with_correct_framing() {
    let body = b"opaque-change";
    let server = FakeRelay::spawn(|_| {
        Response::json(
            200,
            &json!({"seq": 7, "id": "0123456789abcdef0123456789abcdef"}),
        )
    });

    let posted = server
        .client()
        .post_blob(ws(), body)
        .expect("post should succeed");
    assert_eq!(
        posted,
        Posted {
            seq: 7,
            id: "0123456789abcdef0123456789abcdef".to_string(),
        }
    );

    let request = server.last_request();
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, format!("/v1/ws/{WSID_HEX}/blobs"));
    let authority = server.addr.to_string();
    assert_eq!(request.header("host"), Some(authority.as_str()));
    assert_eq!(
        request.header("content-length"),
        Some(body.len().to_string().as_str())
    );
    assert_eq!(
        request.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(request.header("connection"), Some("close"));
    assert_eq!(request.body.as_slice(), &body[..]);
}

#[test]
fn post_blob_surfaces_non_2xx_as_status_error() {
    let server = FakeRelay::spawn(|_| Response::json(413, &json!({"error": "blob too large"})));

    let error = server
        .client()
        .post_blob(ws(), &[0u8; 16])
        .expect_err("413 must be an error");
    match error {
        RelayError::Status { code, body } => {
            assert_eq!(code, 413);
            assert!(
                body.contains("blob too large"),
                "error body should be preserved: {body:?}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// fetch_since

#[test]
fn fetch_since_returns_a_single_page_and_round_trips_bytes() {
    let payloads: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0u8],
        (0u8..=255).collect(),
        vec![0xab; 4096],
    ];
    let expected: Vec<(u64, Vec<u8>)> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| (index as u64 + 1, payload.clone()))
        .collect();
    let response = page(4, false, expected);
    let server = FakeRelay::spawn(move |_| Response::json(200, &response));

    let fetched = server
        .client()
        .fetch_since(ws(), 0)
        .expect("fetch should succeed");
    assert_eq!(fetched.len(), payloads.len());
    for (blob, payload) in fetched.iter().zip(&payloads) {
        assert_eq!(&blob.data, payload, "blob bytes must round-trip exactly");
        assert_eq!(blob.id, id_of(payload));
    }
    assert_eq!(
        fetched.iter().map(|blob| blob.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(
        requests[0].target,
        format!("/v1/ws/{WSID_HEX}/blobs?since=0")
    );
}

#[test]
fn fetch_since_follows_more_pages_until_exhausted() {
    let first_page = page(2, true, vec![(1, b"one".to_vec()), (2, b"two".to_vec())]);
    let second_page = page(3, false, vec![(3, b"three".to_vec())]);
    let server = FakeRelay::spawn(move |request| match request.query("since") {
        Some("0") => Response::json(200, &first_page),
        Some("2") => Response::json(200, &second_page),
        other => Response::json(
            500,
            &json!({"error": format!("unexpected since {other:?}")}),
        ),
    });

    let fetched = server
        .client()
        .fetch_since(ws(), 0)
        .expect("fetch should paginate");
    assert_eq!(
        fetched
            .iter()
            .map(|blob| (blob.seq, blob.data.clone()))
            .collect::<Vec<_>>(),
        vec![
            (1, b"one".to_vec()),
            (2, b"two".to_vec()),
            (3, b"three".to_vec()),
        ]
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "exactly two pages should be requested");
    assert_eq!(requests[0].query("since"), Some("0"));
    assert_eq!(requests[1].query("since"), Some("2"));
}

#[test]
fn fetch_since_starts_at_the_requested_cursor() {
    let response = page(9, false, vec![(9, b"nine".to_vec())]);
    let server = FakeRelay::spawn(move |_| Response::json(200, &response));

    let fetched = server.client().fetch_since(ws(), 8).expect("fetch");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].seq, 9);

    let requests = server.requests();
    assert_eq!(requests[0].query("since"), Some("8"));
}

#[test]
fn fetch_since_rejects_a_non_advancing_page_loop() {
    // The first page advances the cursor to 2; after that the relay keeps
    // claiming "more" while returning nothing new, so the client must give up
    // rather than spin forever.
    let first_page = page(2, true, vec![(1, b"one".to_vec()), (2, b"two".to_vec())]);
    let server = FakeRelay::spawn(move |request| match request.query("since") {
        Some("0") => Response::json(200, &first_page),
        Some(since) => {
            let since = since.parse::<u64>().unwrap_or(0);
            Response::json(200, &page(since, true, Vec::new()))
        }
        None => Response::json(400, &json!({"error": "missing since"})),
    });

    let error = server
        .client()
        .fetch_since(ws(), 0)
        .expect_err("must not loop forever");
    assert!(
        matches!(error, RelayError::Protocol(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(server.request_count(), 2);
}

#[test]
fn fetch_since_rejects_replayed_old_sequences() {
    // A relay that jumps the cursor forward but keeps replaying an old blob is
    // violating "seq > since" and would otherwise make the client loop.
    let first_page = page(5, true, vec![(1, b"one".to_vec())]);
    let replayed_page = page(9, true, vec![(1, b"one".to_vec())]);
    let server = FakeRelay::spawn(move |request| match request.query("since") {
        Some("0") => Response::json(200, &first_page),
        _ => Response::json(200, &replayed_page),
    });

    let error = server
        .client()
        .fetch_since(ws(), 0)
        .expect_err("must reject replayed sequence numbers");
    assert!(
        matches!(error, RelayError::Protocol(_)),
        "unexpected error: {error:?}"
    );
}

#[test]
fn fetch_since_rejects_invalid_base64() {
    let response = json!({
        "cursor": 1,
        "more": false,
        "blobs": [{"seq": 1, "id": "0123456789abcdef0123456789abcdef", "data": "!!! not base64 !!!"}],
    });
    let server = FakeRelay::spawn(move |_| Response::json(200, &response));

    let error = server
        .client()
        .fetch_since(ws(), 0)
        .expect_err("invalid base64 must fail");
    assert!(
        matches!(error, RelayError::Base64),
        "unexpected error: {error:?}"
    );
}

// head / delete

#[test]
fn head_returns_the_highest_sequence_number() {
    for (head, count) in [(2u64, 1u64), (0, 0)] {
        let server =
            FakeRelay::spawn(move |_| Response::json(200, &json!({"head": head, "count": count})));
        assert_eq!(server.client().head(ws()).expect("head"), head);
        let request = server.last_request();
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, format!("/v1/ws/{WSID_HEX}/head"));
    }
}

#[test]
fn delete_blob_maps_200_to_true_and_404_to_false() {
    let present = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let missing = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let server = FakeRelay::spawn(move |request| {
        if request.target == format!("/v1/ws/{WSID_HEX}/blobs/{present}") {
            Response::json(200, &json!({"deleted": true}))
        } else {
            Response::json(404, &json!({"error": "blob not found"}))
        }
    });

    let client = server.client();
    assert!(client.delete_blob(ws(), present).expect("delete present"));
    assert!(!client.delete_blob(ws(), missing).expect("delete missing"));

    // A caller-supplied id can never add path segments or a query string.
    assert!(!client
        .delete_blob(ws(), "not/hex?evil=1")
        .expect("delete malformed id"));

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "DELETE");
    assert_eq!(
        requests[2].target,
        format!("/v1/ws/{WSID_HEX}/blobs/not%2Fhex%3Fevil%3D1")
    );
}

// base URL handling

#[test]
fn malformed_base_urls_are_rejected() {
    let bad = [
        "",
        "   ",
        "http://",
        "http://:8080",
        "https://127.0.0.1:1",
        "ftp://127.0.0.1:1",
        "127.0.0.1:not-a-port",
        "127.0.0.1:99999",
        "127.0.0.1:0",
        "http://127.0.0.1:1/path",
        "http://user:pass@127.0.0.1:1",
        "http://[::1",
        "http://127.0.0.1:1?x=1",
        "http://127.0.0.1:1#fragment",
        "::1",
    ];
    for url in bad {
        let error = RelayClient::new(url).expect_err("URL should be rejected");
        assert!(
            matches!(error, RelayError::BadUrl),
            "{url:?} produced unexpected error: {error:?}"
        );
    }
}

#[test]
fn accepted_base_url_forms_all_reach_the_relay() {
    let server = FakeRelay::spawn(|_| {
        Response::json(200, &json!({"status": "ok", "version": "1", "blobs": 0}))
    });

    let urls = [
        format!("http://{}", server.addr),
        format!("http://{}/", server.addr),
        server.addr.to_string(),
    ];
    for url in urls {
        let client = RelayClient::new(&url).expect("URL should be accepted");
        assert_eq!(
            client.health().expect("health").status,
            "ok",
            "failed for {url:?}"
        );
    }
}

// transport robustness

#[test]
fn connection_refused_is_an_io_error() {
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
        listener.local_addr().expect("probe address")
        // Dropping the listener frees the port; nothing is listening on it.
    };

    let client = RelayClient::new(&format!("http://{addr}")).expect("valid URL");
    let error = client.health().expect_err("nothing is listening");
    assert!(
        matches!(error, RelayError::Io(_)),
        "unexpected error: {error:?}"
    );
}

#[test]
fn silent_relay_times_out() {
    let server = SilentServer::spawn();
    let client = RelayClient::new(&server.base_url())
        .expect("valid URL")
        .with_timeout(Duration::from_millis(150));

    let started = Instant::now();
    let error = client.health().expect_err("relay never answers");
    let elapsed = started.elapsed();

    assert!(
        matches!(error, RelayError::Timeout),
        "unexpected error: {error:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout should bound the wait, took {elapsed:?}"
    );
}

#[test]
fn body_split_across_tcp_reads_is_reassembled() {
    let payload: Vec<u8> = (0..64 * 1024).map(|index| (index % 251) as u8).collect();
    let response = page(1, false, vec![(1, payload.clone())]);
    let server = FakeRelay::spawn(move |_| Response::json(200, &response).dribble());

    let fetched = server.client().fetch_since(ws(), 0).expect("fetch");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].data, payload);
}

#[test]
fn headers_are_matched_case_insensitively() {
    let server = FakeRelay::spawn(|_| {
        Response::json(
            200,
            &json!({"status": "ok", "version": "lowercase", "blobs": 5}),
        )
        .lowercase()
    });

    let health = server.client().health().expect("health");
    assert_eq!(health.version, "lowercase");
    assert_eq!(health.blobs, 5);
}

#[test]
fn response_without_content_length_is_read_to_eof() {
    let server = FakeRelay::spawn(|_| {
        Response::json(200, &json!({"head": 4, "count": 2})).close_delimited()
    });

    assert_eq!(server.client().head(ws()).expect("head"), 4);
}

#[test]
fn chunked_responses_are_decoded() {
    let server = FakeRelay::spawn(|_| {
        Response::json(
            200,
            &json!({"status": "ok", "version": "chunky", "blobs": 3}),
        )
        .chunked()
        .dribble()
    });

    let health = server.client().health().expect("chunked health");
    assert_eq!(health.version, "chunky");
    assert_eq!(health.blobs, 3);
}

#[test]
fn ambiguous_framing_is_a_protocol_error() {
    let server = FakeRelay::spawn(|_| {
        Response::bytes(200, b"{}".to_vec()).header("Transfer-Encoding", "chunked")
    });

    let error = server.client().health().expect_err("must reject ambiguity");
    assert!(
        matches!(error, RelayError::Protocol(_)),
        "unexpected error: {error:?}"
    );
}

#[test]
fn truncated_response_body_is_a_protocol_error() {
    let server = FakeRelay::spawn(|_| Response::bytes(200, b"{}".to_vec()).claim_length(100_000));

    let error = server
        .client()
        .health()
        .expect_err("must detect truncation");
    assert!(
        matches!(error, RelayError::Protocol(_)),
        "unexpected error: {error:?}"
    );
}

// auth token

#[test]
fn bearer_token_is_sent_but_never_debug_printed() {
    let server = FakeRelay::spawn(|_| {
        Response::json(200, &json!({"status": "ok", "version": "1", "blobs": 0}))
    });

    let client = server.client().with_auth_token("s3cret-relay-token");
    let debug = format!("{client:?}");
    assert!(
        !debug.contains("s3cret-relay-token"),
        "token leaked in Debug output: {debug}"
    );

    client.health().expect("health with token");
    assert_eq!(
        server.last_request().header("authorization"),
        Some("Bearer s3cret-relay-token")
    );

    // Without a token the header is absent.
    server.client().health().expect("health without token");
    assert_eq!(server.last_request().header("authorization"), None);
}

#[test]
fn auth_token_with_control_characters_is_refused() {
    let server = FakeRelay::spawn(|_| {
        Response::json(200, &json!({"status": "ok", "version": "1", "blobs": 0}))
    });

    let client = server.client().with_auth_token("token\r\nX-Injected: yes");
    let error = client
        .health()
        .expect_err("header injection must be refused");
    assert!(
        matches!(error, RelayError::BadUrl),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        server.request_count(),
        0,
        "no request should be put on the wire"
    );
}

// push_changes

/// Handler for a relay that starts empty and assigns sequences 1, 2, 3, ...
fn empty_relay_handler() -> impl Fn(Request) -> Response + Send + 'static {
    let next_seq = Arc::new(Mutex::new(0u64));
    move |request: Request| {
        if request.method == "GET" && request.target.ends_with("/head") {
            Response::json(200, &json!({"head": 0, "count": 0}))
        } else if request.method == "POST" && request.target.ends_with("/blobs") {
            let mut seq = next_seq.lock().expect("sequence counter");
            *seq += 1;
            Response::json(200, &json!({"seq": *seq, "id": id_of(&request.body)}))
        } else {
            Response::bytes(
                400,
                format!("unexpected request: {} {}", request.method, request.target).into_bytes(),
            )
        }
    }
}

#[test]
fn push_changes_posts_everything_to_an_empty_relay() {
    let server = FakeRelay::spawn(empty_relay_handler());
    let changes = vec![b"alpha".to_vec(), b"beta".to_vec()];

    assert_eq!(
        server.client().push_changes(ws(), &changes).expect("push"),
        2
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one head call plus two posts");
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].target.ends_with("/head"));
    let posted: Vec<&[u8]> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| request.body.as_slice())
        .collect();
    assert_eq!(posted, vec![changes[0].as_slice(), changes[1].as_slice()]);
}

#[test]
fn push_changes_skips_blobs_the_relay_already_has() {
    let known = [b"alpha".to_vec(), b"gamma".to_vec()];
    let listing = page(2, false, vec![(1, known[0].clone()), (2, known[1].clone())]);
    let next_seq = Arc::new(Mutex::new(2u64));
    let server = FakeRelay::spawn(move |request: Request| {
        if request.method == "GET" && request.target.ends_with("/head") {
            Response::json(200, &json!({"head": 2, "count": 2}))
        } else if request.method == "GET" && request.target.contains("/blobs?since=0") {
            Response::json(200, &listing)
        } else if request.method == "POST" {
            let mut seq = next_seq.lock().expect("sequence counter");
            *seq += 1;
            Response::json(200, &json!({"seq": *seq, "id": id_of(&request.body)}))
        } else {
            Response::bytes(400, b"unexpected request".to_vec())
        }
    });

    let changes = vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()];
    assert_eq!(
        server.client().push_changes(ws(), &changes).expect("push"),
        1,
        "only beta is missing on the relay"
    );

    let requests = server.requests();
    let posted: Vec<&[u8]> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| request.body.as_slice())
        .collect();
    assert_eq!(posted, vec![b"beta".as_slice()]);
}

#[test]
fn push_changes_dedupes_identical_blobs_in_one_batch() {
    let server = FakeRelay::spawn(empty_relay_handler());
    let changes = vec![b"same".to_vec(), b"same".to_vec(), b"other".to_vec()];

    assert_eq!(
        server.client().push_changes(ws(), &changes).expect("push"),
        2
    );

    let requests = server.requests();
    let posted: Vec<&[u8]> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| request.body.as_slice())
        .collect();
    assert_eq!(posted, vec![b"same".as_slice(), b"other".as_slice()]);
}

#[test]
fn push_changes_with_no_changes_makes_no_requests() {
    let server = FakeRelay::spawn(|_| Response::bytes(500, b"should not be called".to_vec()));

    assert_eq!(server.client().push_changes(ws(), &[]).expect("push"), 0);
    assert_eq!(server.request_count(), 0);
}

// against the real relay, when a prebuilt binary is available

/// Locate a prebuilt `meshdoc-relay` binary, if one exists.
fn relay_binary() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("MESHDOC_RELAY_BIN") {
        let path = PathBuf::from(configured);
        return path.is_file().then_some(path);
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for candidate in [
        "relay/target/release/meshdoc-relay",
        "relay/target/debug/meshdoc-relay",
    ] {
        let path = root.join(candidate);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// A spawned real relay, killed and cleaned up on drop.
struct RelayProcess {
    child: Child,
    data_dir: PathBuf,
    /// Kept open so the relay's request-log lines go into a pipe rather than
    /// into a closed fd; never read after startup.
    _stdout: Option<BufReader<ChildStdout>>,
}

impl RelayProcess {
    /// Spawn the relay on an ephemeral port and return it plus its base URL.
    fn start(binary: &Path) -> (RelayProcess, String) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let data_dir = std::env::temp_dir().join(format!(
            "meshdoc-relay-client-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&data_dir).expect("create relay data dir");

        let mut child = Command::new(binary)
            .arg("--data")
            .arg(&data_dir)
            .arg("--port")
            .arg("0")
            .arg("--bind")
            .arg("127.0.0.1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("could not spawn {}: {error}", binary.display()));

        let stdout = child.stdout.take().expect("relay stdout is piped");
        let mut reader = BufReader::new(stdout);
        let mut banner = String::new();
        reader
            .read_line(&mut banner)
            .expect("read relay startup banner");
        let url = banner
            .split("listening on ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("relay did not report its address; banner: {banner:?}"))
            .to_string();

        (
            RelayProcess {
                child,
                data_dir,
                _stdout: Some(reader),
            },
            url,
        )
    }
}

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

#[test]
fn live_relay_round_trip_when_binary_is_available() {
    let Some(binary) = relay_binary() else {
        eprintln!(
            "skipping live relay test: no prebuilt meshdoc-relay binary found \
             (build it with `cargo build --release` in relay/, or set MESHDOC_RELAY_BIN)"
        );
        return;
    };
    let (_relay, url) = RelayProcess::start(&binary);
    let client = RelayClient::new(&url).expect("relay URL is valid");

    // health
    let health = client.health().expect("health");
    assert_eq!(health.status, "ok");
    assert!(!health.version.is_empty());
    assert_eq!(health.blobs, 0);

    // post / idempotency / head
    let workspace = ws();
    let payloads: Vec<Vec<u8>> = vec![
        b"first-change".to_vec(),
        b"second-change".to_vec(),
        vec![0u8; 700],
    ];
    let first = client
        .post_blob(workspace, &payloads[0])
        .expect("post first blob");
    assert_eq!(first.seq, 1);
    assert_eq!(first.id, id_of(&payloads[0]));

    let duplicate = client
        .post_blob(workspace, &payloads[0])
        .expect("re-post identical blob");
    assert_eq!(
        duplicate, first,
        "relay must be idempotent for identical bytes"
    );

    let second = client
        .post_blob(workspace, &payloads[1])
        .expect("post second");
    assert_eq!(second.seq, 2);
    client
        .post_blob(workspace, &payloads[2])
        .expect("post third");
    assert_eq!(client.head(workspace).expect("head"), 3);

    // fetch: full and from a cursor
    let all = client.fetch_since(workspace, 0).expect("fetch all");
    assert_eq!(all.len(), 3);
    for (index, blob) in all.iter().enumerate() {
        assert_eq!(blob.seq, index as u64 + 1);
        assert_eq!(blob.data, payloads[index]);
        assert_eq!(blob.id, id_of(&payloads[index]));
    }
    let tail = client.fetch_since(workspace, 1).expect("fetch since 1");
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].seq, 2);

    // delete: present then absent
    assert!(client
        .delete_blob(workspace, &second.id)
        .expect("delete present blob"));
    assert!(!client
        .delete_blob(workspace, &second.id)
        .expect("delete missing blob"));
    assert_eq!(client.head(workspace).expect("head after delete"), 3);
    assert_eq!(client.fetch_since(workspace, 0).expect("fetch").len(), 2);

    // push_changes: fill an empty workspace, then only send what is missing
    let push_workspace = ws_b();
    let changes: Vec<Vec<u8>> = vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()];
    assert_eq!(
        client
            .push_changes(push_workspace, &changes)
            .expect("first push"),
        3
    );
    assert_eq!(
        client
            .push_changes(push_workspace, &changes)
            .expect("second push"),
        0,
        "nothing should be re-uploaded"
    );
    let more = vec![b"beta".to_vec(), b"delta".to_vec()];
    assert_eq!(
        client
            .push_changes(push_workspace, &more)
            .expect("incremental push"),
        1,
        "only delta is new"
    );
    let stored = client
        .fetch_since(push_workspace, 0)
        .expect("fetch pushed changes");
    assert_eq!(stored.len(), 4);
}
