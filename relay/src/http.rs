//! HTTP/1.1 surface: routing, request validation, JSON responses and logging.
//!
//! The relay deliberately never interprets blob bytes. Bodies are hashed,
//! stored and returned verbatim; nothing is parsed, sniffed, decompressed or
//! content-type-checked.

use std::io::Read;
use std::time::Instant;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Serialize;
use tiny_http::{Header, Request, Response};

use crate::store::{is_valid_blob_id, is_valid_wsid, Store};

/// Hard cap on blobs returned by a single GET (contract requirement).
pub const MAX_PAGE_SIZE: usize = 256;

fn json_response<T: Serialize>(status: u16, value: &T) -> (u16, String) {
    (
        status,
        serde_json::to_string(value).expect("response types are always serializable"),
    )
}

fn error_json(status: u16, message: &str) -> (u16, String) {
    json_response(status, &ErrorBody { error: message })
}

// Response bodies are serialized from structs, not maps, so the JSON key order
// matches the documented wire contract exactly (a client is written against it).

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

#[derive(Serialize)]
struct HealthBody<'a> {
    status: &'a str,
    version: &'a str,
    blobs: usize,
}

#[derive(Serialize)]
struct PutBody {
    seq: u64,
    id: String,
}

#[derive(Serialize)]
struct ListBlobBody {
    seq: u64,
    id: String,
    data: String,
}

#[derive(Serialize)]
struct ListBody {
    cursor: u64,
    more: bool,
    blobs: Vec<ListBlobBody>,
}

#[derive(Serialize)]
struct HeadBody {
    head: u64,
    count: usize,
}

#[derive(Serialize)]
struct DeletedBody {
    deleted: bool,
}

fn method_not_allowed() -> (u16, String) {
    error_json(405, "method not allowed")
}

/// Handle one request end to end: route it, respond, and log one line.
/// The body is never logged, only the response size.
pub fn handle(mut request: Request, store: &Store, max_blob_bytes: usize) {
    let started = Instant::now();
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();

    let (status, body) = route(&mut request, store, max_blob_bytes, &method, &url);
    let response_bytes = body.len();

    let content_type =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("static header");
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(content_type);
    if let Err(err) = request.respond(response) {
        eprintln!("relay: failed to send response: {}", err);
    }

    println!(
        "{} {} -> {} {}B {}us",
        method,
        url,
        status,
        response_bytes,
        started.elapsed().as_micros()
    );
}

fn route(
    request: &mut Request,
    store: &Store,
    max_blob_bytes: usize,
    method: &str,
    url: &str,
) -> (u16, String) {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url, ""),
    };
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    if segments.first() != Some(&"v1") {
        return error_json(404, "not found");
    }

    match segments.as_slice() {
        ["v1", "health"] => {
            if method != "GET" {
                return method_not_allowed();
            }
            json_response(
                200,
                &HealthBody {
                    status: "ok",
                    version: env!("CARGO_PKG_VERSION"),
                    blobs: store.total_blobs(),
                },
            )
        }
        ["v1", "ws", wsid, "head"] => {
            if method != "GET" {
                return method_not_allowed();
            }
            if !is_valid_wsid(wsid) {
                return error_json(401, "invalid workspace id");
            }
            let (head, count) = store.head(wsid);
            json_response(200, &HeadBody { head, count })
        }
        ["v1", "ws", wsid, "blobs"] => match method {
            "POST" => post_blob(request, store, wsid, max_blob_bytes),
            "GET" => list_blobs(store, wsid, query),
            _ => method_not_allowed(),
        },
        ["v1", "ws", wsid, "blobs", id] => {
            if method != "DELETE" {
                return method_not_allowed();
            }
            if !is_valid_wsid(wsid) {
                return error_json(401, "invalid workspace id");
            }
            if !is_valid_blob_id(id) {
                return error_json(400, "invalid blob id");
            }
            match store.delete(wsid, id) {
                Ok(Some(_seq)) => json_response(200, &DeletedBody { deleted: true }),
                Ok(None) => error_json(404, "blob not found"),
                Err(err) => {
                    eprintln!("relay: delete failed: {}", err);
                    error_json(500, "storage error")
                }
            }
        }
        _ => error_json(404, "not found"),
    }
}

fn post_blob(
    request: &mut Request,
    store: &Store,
    wsid: &str,
    max_blob_bytes: usize,
) -> (u16, String) {
    // Validate the path component before it can ever be joined to a filesystem
    // path. Only ^[0-9a-f]{32}$ is accepted, so traversal is impossible.
    if !is_valid_wsid(wsid) {
        return error_json(401, "invalid workspace id");
    }
    // Read at most max+1 bytes: enough to decide 413 without ever buffering an
    // arbitrarily large body. This also drains the request fully in the common
    // just-over-the-limit case, keeping the connection clean.
    let mut body = Vec::new();
    let read_limit = max_blob_bytes as u64 + 1;
    if let Err(err) = request.as_reader().take(read_limit).read_to_end(&mut body) {
        eprintln!("relay: failed to read request body: {}", err);
        return error_json(400, "could not read request body");
    }
    if body.len() > max_blob_bytes {
        return error_json(413, "blob too large");
    }

    match store.put(wsid, &body) {
        Ok(outcome) => json_response(
            200,
            &PutBody {
                seq: outcome.seq,
                id: outcome.id,
            },
        ),
        Err(err) => {
            eprintln!("relay: failed to store blob: {}", err);
            error_json(500, "storage error")
        }
    }
}

fn list_blobs(store: &Store, wsid: &str, query: &str) -> (u16, String) {
    if !is_valid_wsid(wsid) {
        return error_json(401, "invalid workspace id");
    }
    let since = parse_u64_param(query, "since").unwrap_or(0);
    match store.list(wsid, since, MAX_PAGE_SIZE) {
        Ok(outcome) => {
            let blobs: Vec<ListBlobBody> = outcome
                .blobs
                .iter()
                .map(|blob| ListBlobBody {
                    seq: blob.seq,
                    id: blob.id.clone(),
                    data: BASE64.encode(&blob.data),
                })
                .collect();
            json_response(
                200,
                &ListBody {
                    cursor: outcome.cursor,
                    more: outcome.more,
                    blobs,
                },
            )
        }
        Err(err) => {
            eprintln!("relay: list failed: {}", err);
            error_json(500, "storage error")
        }
    }
}

fn parse_u64_param(query: &str, name: &str) -> Option<u64> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            value.parse::<u64>().ok()
        } else {
            None
        }
    })
}
