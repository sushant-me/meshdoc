//! Client for the MeshDoc relay's HTTP/1.1 API.
//!
//! The relay is a deliberately dumb store-and-forward mailbox: it keeps opaque,
//! content-addressed blobs per workspace, assigns each one a per-workspace
//! monotonic sequence number, and hands them back on demand (see
//! `relay/README.md`). This module is the client half — a small HTTP/1.1 client
//! built directly on [`std::net::TcpStream`] so the library does not pull in an
//! async runtime, TLS stack or HTTP crate just to talk to a mailbox. The
//! transport is plaintext HTTP; run the relay behind a TLS terminator when the
//! network path is untrusted.
//!
//! # Behaviour
//!
//! * Request framing is explicit: `Host`, `Content-Length`, `Connection: close`
//!   and, for uploads, `Content-Type: application/octet-stream`.
//! * Responses are read robustly: the status line is parsed, headers are matched
//!   case-insensitively, and the body is delimited by `Content-Length`, by
//!   `Transfer-Encoding: chunked`, or (last resort, when the server closes the
//!   connection) read to EOF. Bodies split across multiple TCP reads are
//!   reassembled. Responses with ambiguous framing are rejected.
//! * Every socket read and write is bounded by the configured timeout, so a
//!   wedged or hostile relay surfaces as [`RelayError::Timeout`] instead of
//!   hanging a caller's thread.
//! * [`RelayClient::fetch_since`] follows `more` pages automatically and returns
//!   [`RelayError::Protocol`] if a relay claims more data without advancing the
//!   cursor, so a buggy server cannot spin the client forever.
//! * Non-2xx responses are surfaced as [`RelayError::Status`] with the status
//!   code and (bounded) error body.
//!
//! Blob *contents* are never logged or embedded in error messages, and the
//! optional bearer token never appears in [`std::fmt::Debug`] output.
//!
//! # Example
//!
//! ```no_run
//! use meshdoc::ids::WorkspaceId;
//! use meshdoc::relay::RelayClient;
//!
//! # fn main() -> Result<(), meshdoc::relay::RelayError> {
//! let client = RelayClient::new("http://127.0.0.1:8080")?;
//! let health = client.health()?;
//! let ws = WorkspaceId::random();
//! let sent = client.push_changes(ws, &[vec![1u8, 2, 3]])?;
//! println!(
//!     "relay {} v{} holds {} blob(s); pushed {}",
//!     health.status, health.version, health.blobs, sent
//! );
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Deserialize;

use crate::ids::WorkspaceId;

/// Timeout applied to every socket read and write unless overridden with
/// [`RelayClient::with_timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest response head (status line plus headers) this client will buffer.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// Largest response body this client will buffer.
///
/// The relay caps a blob at 8 MiB and a listing page at 256 blobs, so a
/// legitimate response is far below this. The bound exists so a hostile or
/// broken relay cannot make the client allocate without limit.
const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;

/// Maximum number of bytes of an error response body copied into
/// [`RelayError::Status`]. Error bodies are small JSON; the cap only stops a
/// misbehaving server from attaching an arbitrarily large string to an error.
const MAX_ERROR_BODY_BYTES: usize = 4096;

/// Everything that can go wrong talking to a relay.
#[derive(Debug)]
pub enum RelayError {
    /// The socket could not be opened, written to or read from. The string is
    /// the underlying `std::io::Error` rendered as text.
    Io(String),
    /// The relay's answer was malformed or internally contradictory: bad status
    /// line, bad headers, truncated body, invalid JSON, or a listing that claims
    /// more pages without advancing.
    Protocol(String),
    /// The relay answered with a non-2xx status.
    Status { code: u16, body: String },
    /// A blob's `data` field was not valid standard base64.
    Base64,
    /// A connect, read or write exceeded the socket timeout.
    Timeout,
    /// The base URL could not be parsed into an `http://host:port` endpoint.
    BadUrl,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RelayError::Io(message) => write!(f, "relay i/o error: {message}"),
            RelayError::Protocol(message) => write!(f, "relay protocol error: {message}"),
            RelayError::Status { code, body } if body.is_empty() => {
                write!(f, "relay returned HTTP {code}")
            }
            RelayError::Status { code, body } => {
                write!(f, "relay returned HTTP {code}: {body}")
            }
            RelayError::Base64 => write!(f, "relay returned blob data that is not valid base64"),
            RelayError::Timeout => write!(f, "relay request timed out"),
            RelayError::BadUrl => write!(f, "invalid relay base URL (expected http://host:port)"),
        }
    }
}

impl std::error::Error for RelayError {}

impl From<io::Error> for RelayError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            // `WouldBlock` is what a timed-out SO_RCVTIMEO/SO_SNDTIMEO reports on
            // Unix; `TimedOut` is the cross-platform kind.
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => RelayError::Timeout,
            io::ErrorKind::UnexpectedEof => RelayError::Protocol(
                "relay closed the connection before the response was complete".to_string(),
            ),
            _ => RelayError::Io(error.to_string()),
        }
    }
}

/// Relay liveness and bookkeeping, from `GET /v1/health`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// `"ok"` when the relay is serving.
    pub status: String,
    /// Relay crate version string.
    pub version: String,
    /// Total number of stored blobs across all workspaces.
    pub blobs: u64,
}

/// Result of storing one blob, from `POST /v1/ws/{wsid}/blobs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posted {
    /// Per-workspace sequence number assigned (or the original one, if the
    /// relay already held identical bytes).
    pub seq: u64,
    /// Content id: first 32 hex characters of `blake3(body)`.
    pub id: String,
}

/// One stored blob, from a listing page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    /// Per-workspace sequence number.
    pub seq: u64,
    /// Content id: first 32 hex characters of `blake3(body)`.
    pub id: String,
    /// The exact bytes that were stored (opaque, client-encrypted).
    pub data: Vec<u8>,
}

/// A client for one relay endpoint.
///
/// A `RelayClient` is cheap: it holds a host, a port, a timeout and an optional
/// bearer token, and opens a fresh connection per request (the relay answers
/// with `Connection: close`). It can be shared across threads behind an `Arc`.
#[derive(Clone)]
pub struct RelayClient {
    /// Hostname or IP literal, without brackets.
    host: String,
    /// TCP port. Defaults to 80 when the base URL omits it.
    port: u16,
    /// Applied to connect, read and write; `Duration::ZERO` disables it.
    timeout: Duration,
    /// Sent as `Authorization: Bearer <token>` when set.
    auth_token: Option<String>,
}

impl fmt::Debug for RelayClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the token: Debug output tends to end up in logs.
        f.debug_struct("RelayClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("timeout", &self.timeout)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl RelayClient {
    /// Parse a relay base URL and build a client.
    ///
    /// Accepts `http://host:port`, `http://host:port/`, and the scheme-less
    /// `host:port` form. The port defaults to 80 when omitted. `https`, any
    /// other scheme, a non-empty path, userinfo, a query string and a port of 0
    /// are rejected with [`RelayError::BadUrl`] — this client speaks plaintext
    /// HTTP/1.1 only, so it refuses to pretend otherwise.
    pub fn new(base_url: &str) -> Result<Self, RelayError> {
        let (host, port) = parse_base_url(base_url)?;
        Ok(RelayClient {
            host,
            port,
            timeout: DEFAULT_TIMEOUT,
            auth_token: None,
        })
    }

    /// Override the socket timeout (default [`DEFAULT_TIMEOUT`]).
    ///
    /// The timeout bounds connecting, every read and every write. A zero
    /// duration means "no timeout", matching `TcpStream::set_read_timeout(None)`;
    /// prefer the default over disabling it.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send `Authorization: Bearer <token>` with every request.
    ///
    /// This is for relays fronted by an authenticating proxy; the relay itself
    /// has no auth. An empty token means "no token". A token containing CR/LF
    /// (or other ASCII control characters) is refused at request time with
    /// [`RelayError::BadUrl`] rather than risking header injection.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.auth_token = if token.is_empty() { None } else { Some(token) };
        self
    }

    /// Value for the `Host` request header: `host:port`, or `[v6]:port`.
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn connect(&self) -> Result<TcpStream, RelayError> {
        let addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|error| RelayError::Io(format!("could not resolve {}: {error}", self.host)))?;

        let mut last_error: Option<io::Error> = None;
        for addr in addrs {
            let attempt = if self.timeout.is_zero() {
                TcpStream::connect(addr)
            } else {
                TcpStream::connect_timeout(&addr, self.timeout)
            };
            match attempt {
                Ok(stream) => return configure_socket(stream, self.timeout),
                Err(error) => last_error = Some(error),
            }
        }

        Err(match last_error {
            Some(error) => error.into(),
            None => RelayError::Io(format!(
                "{}:{} did not resolve to any address",
                self.host, self.port
            )),
        })
    }

    /// Perform one request/response exchange over a fresh connection.
    fn request(
        &self,
        method: &str,
        target: &str,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, RelayError> {
        let token = match self.auth_token.as_deref() {
            None | Some("") => None,
            Some(token) => {
                if token.bytes().any(|byte| byte.is_ascii_control()) {
                    // Refuse to build a request that could inject headers.
                    return Err(RelayError::BadUrl);
                }
                Some(token)
            }
        };

        let mut stream = self.connect()?;

        let mut head = String::with_capacity(256);
        head.push_str(method);
        head.push(' ');
        head.push_str(target);
        head.push_str(" HTTP/1.1\r\nHost: ");
        head.push_str(&self.host_header());
        head.push_str("\r\nUser-Agent: meshdoc-relay-client/");
        head.push_str(crate::VERSION);
        head.push_str("\r\nAccept: application/json\r\nConnection: close\r\n");
        if let Some(token) = token {
            head.push_str("Authorization: Bearer ");
            head.push_str(token);
            head.push_str("\r\n");
        }
        match body {
            Some(bytes) => {
                head.push_str("Content-Type: application/octet-stream\r\nContent-Length: ");
                head.push_str(&bytes.len().to_string());
                head.push_str("\r\n");
            }
            None => head.push_str("Content-Length: 0\r\n"),
        }
        head.push_str("\r\n");

        stream.write_all(head.as_bytes())?;
        if let Some(bytes) = body {
            stream.write_all(bytes)?;
        }
        stream.flush()?;

        read_response(&mut BufReader::new(stream))
    }

    /// `GET /v1/health`.
    pub fn health(&self) -> Result<Health, RelayError> {
        let response = self.request("GET", "/v1/health", None)?.into_success()?;
        let wire: HealthWire = parse_json(&response.body, "health")?;
        Ok(Health {
            status: wire.status,
            version: wire.version,
            blobs: wire.blobs,
        })
    }

    /// `POST /v1/ws/{wsid}/blobs` with raw opaque bytes.
    ///
    /// The relay is idempotent for identical bytes: re-posting a blob returns
    /// its original sequence number instead of storing a duplicate.
    pub fn post_blob(&self, ws: WorkspaceId, blob: &[u8]) -> Result<Posted, RelayError> {
        let target = format!("/v1/ws/{}/blobs", ws.to_hex());
        let response = self.request("POST", &target, Some(blob))?.into_success()?;
        let wire: PostedWire = parse_json(&response.body, "post blob")?;
        Ok(Posted {
            seq: wire.seq,
            id: wire.id,
        })
    }

    /// All blobs with `seq > since`, following `more` pages automatically.
    ///
    /// The relay returns at most 256 blobs per page. This keeps asking with
    /// `since = last seq returned` until `more` is false, and enforces the wire
    /// contract on every page: a page must only contain sequence numbers greater
    /// than the `since` it was asked for, and a page claiming `more` must
    /// actually advance the cursor. Anything else means the relay is buggy or
    /// hostile, and [`RelayError::Protocol`] is returned instead of the client
    /// spinning forever or replaying old blobs.
    pub fn fetch_since(&self, ws: WorkspaceId, since: u64) -> Result<Vec<Blob>, RelayError> {
        let mut blobs = Vec::new();
        let mut cursor = since;

        loop {
            let page = self.fetch_page(ws, cursor)?;
            let page_length = page.blobs.len();

            let mut highest_seq = cursor;
            for listed in page.blobs {
                if listed.seq <= cursor {
                    return Err(RelayError::Protocol(format!(
                        "relay returned seq {} for since={cursor}; a listing may only \
                         contain blobs newer than the cursor",
                        listed.seq
                    )));
                }
                let data = BASE64
                    .decode(listed.data.as_bytes())
                    .map_err(|_| RelayError::Base64)?;
                if listed.seq > highest_seq {
                    highest_seq = listed.seq;
                }
                blobs.push(Blob {
                    seq: listed.seq,
                    id: listed.id,
                    data,
                });
            }

            if !page.more {
                return Ok(blobs);
            }

            // Never trust a bare `more: true`: require forward progress made of
            // actual blobs. Both the server-reported cursor and the highest
            // sequence seen count, so neither field alone can stall the client.
            if page_length == 0 {
                return Err(RelayError::Protocol(format!(
                    "relay reported more pages for since={cursor} but returned no blobs"
                )));
            }
            let next = page.cursor.max(highest_seq);
            if next <= cursor {
                return Err(RelayError::Protocol(format!(
                    "relay reported more pages for since={cursor} but did not advance \
                     (cursor={}, {} blob(s) returned)",
                    page.cursor, page_length
                )));
            }
            cursor = next;
        }
    }

    fn fetch_page(&self, ws: WorkspaceId, since: u64) -> Result<ListPage, RelayError> {
        let target = format!("/v1/ws/{}/blobs?since={}", ws.to_hex(), since);
        let response = self.request("GET", &target, None)?.into_success()?;
        parse_json(&response.body, "blob listing")
    }

    /// `GET /v1/ws/{wsid}/head` — the highest live sequence number (0 when the
    /// workspace is empty on the relay).
    pub fn head(&self, ws: WorkspaceId) -> Result<u64, RelayError> {
        let target = format!("/v1/ws/{}/head", ws.to_hex());
        let response = self.request("GET", &target, None)?.into_success()?;
        let wire: HeadWire = parse_json(&response.body, "workspace head")?;
        Ok(wire.head)
    }

    /// `DELETE /v1/ws/{wsid}/blobs/{id}`.
    ///
    /// Returns `Ok(true)` when the relay deleted the blob, `Ok(false)` when it
    /// answered 404 (no such blob). Other non-2xx statuses are
    /// [`RelayError::Status`].
    ///
    /// The id is percent-encoded before being placed in the path, so a caller
    /// cannot smuggle extra path segments or a query string into the request.
    pub fn delete_blob(&self, ws: WorkspaceId, id: &str) -> Result<bool, RelayError> {
        let target = format!("/v1/ws/{}/blobs/{}", ws.to_hex(), encode_path_segment(id));
        let response = self.request("DELETE", &target, None)?;
        if response.status == 404 {
            return Ok(false);
        }
        let response = response.into_success()?;
        let wire: DeletedWire = parse_json(&response.body, "delete blob")?;
        Ok(wire.deleted)
    }

    /// Push every change the relay does not already have; returns how many were
    /// sent.
    ///
    /// This asks `head` once. An empty workspace is filled with plain POSTs. A
    /// non-empty workspace is listed once (`fetch_since(0)`), and only changes
    /// whose content id is absent are POSTed — the relay has no ids-only listing
    /// endpoint, so one listing pass is the cheapest way to avoid re-uploading.
    /// Blobs are content-addressed, so the comparison is against
    /// `blake3(body)[..32]`, exactly the id the relay assigns. If the relay ever
    /// derived ids differently the comparison would simply miss and everything
    /// would be re-POSTed, which the relay's idempotency makes harmless.
    ///
    /// Identical changes within one call are uploaded once.
    pub fn push_changes(&self, ws: WorkspaceId, changes: &[Vec<u8>]) -> Result<usize, RelayError> {
        if changes.is_empty() {
            return Ok(0);
        }

        let head = self.head(ws)?;
        let mut present: HashSet<String> = if head == 0 {
            HashSet::new()
        } else {
            self.fetch_since(ws, 0)?
                .into_iter()
                .map(|blob| blob.id)
                .collect()
        };

        let mut sent = 0;
        for change in changes {
            let id = content_id(change);
            if present.contains(&id) {
                continue;
            }
            let posted = self.post_blob(ws, change)?;
            // Track both the id we compared and the one the relay echoed, so
            // duplicate changes in this batch are uploaded once even if a relay
            // derives ids differently than expected.
            present.insert(id);
            present.insert(posted.id);
            sent += 1;
        }
        Ok(sent)
    }
}

// ---------------------------------------------------------------------------
// URL parsing
// ---------------------------------------------------------------------------

/// Parse `http://host:port`, `http://host:port/` or `host:port` (port defaults
/// to 80) into a host and port. Bracketed IPv6 literals are supported.
fn parse_base_url(input: &str) -> Result<(String, u16), RelayError> {
    let raw = input.trim();
    if raw.is_empty() || raw.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(RelayError::BadUrl);
    }

    let without_scheme = match raw.split_once("://") {
        Some((scheme, rest)) => {
            // This client only speaks plaintext HTTP/1.1. Refuse to silently
            // downgrade an https:// URL.
            if !scheme.eq_ignore_ascii_case("http") {
                return Err(RelayError::BadUrl);
            }
            rest
        }
        None => raw,
    };

    if without_scheme.contains('?') || without_scheme.contains('#') {
        return Err(RelayError::BadUrl);
    }

    let (authority, path) = match without_scheme.find('/') {
        Some(index) => (&without_scheme[..index], &without_scheme[index..]),
        None => (without_scheme, ""),
    };
    if !(path.is_empty() || path == "/") {
        return Err(RelayError::BadUrl);
    }
    if authority.is_empty() || authority.contains('@') {
        return Err(RelayError::BadUrl);
    }

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after_bracket) = rest.split_once(']').ok_or(RelayError::BadUrl)?;
        let port = match after_bracket {
            "" => 80,
            remainder => remainder
                .strip_prefix(':')
                .ok_or(RelayError::BadUrl)?
                .parse::<u16>()
                .map_err(|_| RelayError::BadUrl)?,
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                (host, port.parse::<u16>().map_err(|_| RelayError::BadUrl)?)
            }
            Some(_) => return Err(RelayError::BadUrl), // unbracketed IPv6: ambiguous
            None => (authority, 80),
        }
    };

    if host.is_empty() {
        return Err(RelayError::BadUrl);
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b':'))
    {
        return Err(RelayError::BadUrl);
    }
    if port == 0 {
        return Err(RelayError::BadUrl);
    }

    Ok((host.to_string(), port))
}

/// Percent-encode anything that is not unreserved in a path segment (RFC 3986),
/// so a caller-supplied blob id can never alter the request's routing.
fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

/// First 32 hex characters of `blake3(body)` — the content id the relay assigns.
fn content_id(data: &[u8]) -> String {
    let hex = blake3::hash(data).to_hex();
    hex.as_str()[..32].to_string()
}

// ---------------------------------------------------------------------------
// Socket setup
// ---------------------------------------------------------------------------

fn configure_socket(stream: TcpStream, timeout: Duration) -> Result<TcpStream, RelayError> {
    // Latency matters more than packet count for tiny request/response pairs.
    let _ = stream.set_nodelay(true);
    let timeout = if timeout.is_zero() { None } else { Some(timeout) };
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    Ok(stream)
}

// ---------------------------------------------------------------------------
// Response reading
// ---------------------------------------------------------------------------

/// A parsed HTTP response: status and (fully reassembled) body.
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn into_success(self) -> Result<HttpResponse, RelayError> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(RelayError::Status {
                code: self.status,
                body: error_body(&self.body),
            })
        }
    }
}

/// Render a (bounded) error body for [`RelayError::Status`].
fn error_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_end();
    if trimmed.len() <= MAX_ERROR_BODY_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_ERROR_BODY_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} byte body truncated)", &trimmed[..end], body.len())
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8], what: &str) -> Result<T, RelayError> {
    serde_json::from_slice(body)
        .map_err(|error| RelayError::Protocol(format!("{what}: invalid JSON response: {error}")))
}

/// Read one final response. Interim 1xx statuses (which we do not solicit with
/// `Expect: 100-continue`, but a proxy could inject) are skipped.
fn read_response<R: BufRead>(reader: &mut R) -> Result<HttpResponse, RelayError> {
    loop {
        let status_line = read_line_limited(reader, 1024, "status line")?;
        let status = parse_status_line(&status_line)?;
        let framing = read_headers(reader)?;

        if (100..200).contains(&status) {
            continue;
        }

        let body = read_body(reader, framing)?;
        return Ok(HttpResponse { status, body });
    }
}

fn parse_status_line(line: &str) -> Result<u16, RelayError> {
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(RelayError::Protocol(format!(
            "malformed status line: {line:?}"
        )));
    }
    let code = parts
        .next()
        .ok_or_else(|| RelayError::Protocol(format!("malformed status line: {line:?}")))?;
    let code: u16 = code
        .parse()
        .map_err(|_| RelayError::Protocol(format!("malformed status code in {line:?}")))?;
    if !(100..=599).contains(&code) {
        return Err(RelayError::Protocol(format!(
            "status code out of range in {line:?}"
        )));
    }
    Ok(code)
}

/// How the body length is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// `Content-Length: n`.
    Length(u64),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// No length and no chunking: the connection close marks the end.
    UntilClose,
}

fn read_headers<R: BufRead>(reader: &mut R) -> Result<Framing, RelayError> {
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    let mut header_bytes = 0usize;

    loop {
        let line = read_line_limited(reader, MAX_HEADER_BYTES, "response header")?;
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(RelayError::Protocol(format!(
                "response headers exceed {MAX_HEADER_BYTES} bytes"
            )));
        }
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| RelayError::Protocol(format!("malformed response header: {line:?}")))?;
        let name = name.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("content-length") {
            let length: u64 = value
                .parse()
                .map_err(|_| RelayError::Protocol(format!("invalid Content-Length: {value:?}")))?;
            if let Some(previous) = content_length {
                if previous != length {
                    return Err(RelayError::Protocol(
                        "conflicting Content-Length headers".to_string(),
                    ));
                }
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        {
            chunked = true;
        }
    }

    // Reject ambiguous framing rather than guessing: a response carrying both
    // headers is either broken or trying to smuggle a second message.
    match (chunked, content_length) {
        (true, Some(_)) => Err(RelayError::Protocol(
            "response specifies both Transfer-Encoding and Content-Length".to_string(),
        )),
        (true, None) => Ok(Framing::Chunked),
        (false, Some(length)) => Ok(Framing::Length(length)),
        (false, None) => Ok(Framing::UntilClose),
    }
}

fn read_body<R: BufRead>(reader: &mut R, framing: Framing) -> Result<Vec<u8>, RelayError> {
    match framing {
        Framing::Chunked => read_chunked_body(reader),
        Framing::Length(length) => {
            if length > MAX_RESPONSE_BYTES as u64 {
                return Err(RelayError::Protocol(format!(
                    "response body of {length} bytes exceeds the {MAX_RESPONSE_BYTES} byte limit"
                )));
            }
            let mut body = Vec::with_capacity(length.min(64 * 1024) as usize);
            reader.by_ref().take(length).read_to_end(&mut body)?;
            if body.len() as u64 != length {
                return Err(RelayError::Protocol(format!(
                    "truncated response body: expected {length} bytes, received {}",
                    body.len()
                )));
            }
            Ok(body)
        }
        Framing::UntilClose => {
            let mut body = Vec::new();
            reader
                .by_ref()
                .take(MAX_RESPONSE_BYTES as u64 + 1)
                .read_to_end(&mut body)?;
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(RelayError::Protocol(format!(
                    "response body exceeds the {MAX_RESPONSE_BYTES} byte limit"
                )));
            }
            Ok(body)
        }
    }
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, RelayError> {
    let mut body = Vec::new();
    loop {
        let size_line = read_line_limited(reader, 1024, "chunk size")?;
        // Chunk extensions (`;name=value`) are legal and ignored.
        let size_text = size_line.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| RelayError::Protocol(format!("invalid chunk size: {size_text:?}")))?;

        if size == 0 {
            // Consume optional trailers, terminated by a blank line.
            loop {
                if read_line_limited(reader, MAX_HEADER_BYTES, "chunk trailer")?.is_empty() {
                    return Ok(body);
                }
            }
        }

        let projected = (body.len() as u64)
            .checked_add(size)
            .ok_or_else(|| RelayError::Protocol("chunked body length overflow".to_string()))?;
        if projected > MAX_RESPONSE_BYTES as u64 {
            return Err(RelayError::Protocol(format!(
                "chunked response body exceeds the {MAX_RESPONSE_BYTES} byte limit"
            )));
        }

        let mut chunk = vec![0u8; size as usize];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);

        let mut terminator = [0u8; 2];
        reader.read_exact(&mut terminator)?;
        if &terminator != b"\r\n" {
            return Err(RelayError::Protocol(format!(
                "malformed chunk terminator: {terminator:?}"
            )));
        }
    }
}

/// Read one CRLF-delimited line, with a hard byte cap so a hostile server cannot
/// make the client grow a line without bound.
fn read_line_limited<R: BufRead>(
    reader: &mut R,
    limit: usize,
    what: &str,
) -> Result<String, RelayError> {
    let mut buffer = Vec::new();
    let read = reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut buffer)?;
    if read == 0 {
        return Err(RelayError::Protocol(format!(
            "relay closed the connection while reading the {what}"
        )));
    }
    if read > limit {
        return Err(RelayError::Protocol(format!(
            "relay {what} exceeds {limit} bytes"
        )));
    }
    if !buffer.ends_with(b"\n") {
        return Err(RelayError::Protocol(format!(
            "relay {what} is not newline-terminated"
        )));
    }
    while matches!(buffer.last(), Some(b'\n') | Some(b'\r')) {
        buffer.pop();
    }
    String::from_utf8(buffer)
        .map_err(|_| RelayError::Protocol(format!("relay {what} is not valid UTF-8")))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HealthWire {
    status: String,
    version: String,
    blobs: u64,
}

#[derive(Deserialize)]
struct PostedWire {
    seq: u64,
    id: String,
}

#[derive(Deserialize)]
struct ListPage {
    /// Max sequence number included in the page, or `since` when empty.
    #[serde(default)]
    cursor: u64,
    /// True when the page was truncated and another page follows.
    #[serde(default)]
    more: bool,
    #[serde(default)]
    blobs: Vec<ListedBlob>,
}

#[derive(Deserialize)]
struct ListedBlob {
    seq: u64,
    id: String,
    /// Standard (not URL-safe) base64 of the stored bytes.
    data: String,
}

#[derive(Deserialize)]
struct HeadWire {
    head: u64,
}

#[derive(Deserialize)]
struct DeletedWire {
    deleted: bool,
}
