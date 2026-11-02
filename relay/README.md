# MeshDoc relay

A deliberately dumb, untrusted **store-and-forward mailbox** for MeshDoc. It
accepts opaque encrypted change envelopes, gives each one a per-workspace
sequence number, stores the bytes on disk, and hands them back on demand.

The relay **never inspects or interprets blob contents**. It does not parse,
decrypt, decompress, validate or log payloads. A body is only ever:

1. hashed with blake3 (to derive its id),
2. written to disk verbatim, and
3. base64-encoded when returned.

Confidentiality, authenticity and integrity are the clients' job (end-to-end
encryption and signatures). The relay is a dumb, replaceable pipe.

---

## Quick start

```bash
cd relay
cargo build --release
cargo test                       # 3 unit tests + 11 integration tests

# Run with defaults: ./relay-data, 127.0.0.1:8080
./target/release/meshdoc-relay

# Or:
./target/release/meshdoc-relay \
    --data /var/lib/meshdoc-relay \
    --bind 0.0.0.0 \
    --port 8080 \
    --max-blob-bytes 8388608
```

Startup prints the bound address (useful with `--port 0`):

```
meshdoc-relay 0.1.0 listening on http://127.0.0.1:8080 (data dir: ./relay-data, max blob: 8388608 bytes, ttl: disabled)
```

Stop it with `Ctrl-C` (SIGINT) or `SIGTERM`; the listener and worker threads are
torn down by the OS. The server is thread-based (`tiny_http`), no async runtime.

### Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `--data <dir>` | `./relay-data` | Root data directory (one subdirectory per workspace) |
| `--port <u16>` | `8080` | TCP port; `0` picks an ephemeral port |
| `--bind <ip>` | `127.0.0.1` | IP address to bind |
| `--max-blob-bytes <n>` | `8388608` | Maximum accepted blob size in bytes |
| `--ttl-hours <n>` | disabled | Evict blobs whose mtime is older than `n` hours |

---

## API

All responses are `application/json`. `{wsid}` is a workspace id: exactly 32
lowercase hex characters (`^[0-9a-f]{32}$`). Any other value is rejected with
`401` before it can touch the filesystem. Blob ids are also 32 lowercase hex
characters (`^[0-9a-f]{32}$`).

Every request is logged to stdout as
`METHOD PATH -> STATUS BYTES DURATION`, e.g.:

```
POST /v1/ws/0123456789abcdef0123456789abcdef/blobs -> 200 42B 317us
```

Only the URL and response size are logged — never request bodies.

### `GET /v1/health`

```json
{"status":"ok","version":"0.1.0","blobs":2}
```

`blobs` is the total number of stored blobs across all workspaces.

### `POST /v1/ws/{wsid}/blobs`

* Body: raw opaque bytes (`Content-Type: application/octet-stream`).
* Server assigns the next per-workspace `seq` (u64, starting at 1).
* `id` is the first 32 hex chars (16 bytes) of `blake3(body)`.
* **Idempotent:** posting identical bytes twice returns the original `seq` and
  does not create a second blob.
* Responses: `200 {"seq":7,"id":"<hex>"}`, `413` if the body exceeds
  `--max-blob-bytes`, `401` if the wsid is malformed.

```bash
curl -i --data-binary @envelope.bin \
  -H 'Content-Type: application/octet-stream' \
  http://127.0.0.1:8080/v1/ws/0123456789abcdef0123456789abcdef/blobs
```

### `GET /v1/ws/{wsid}/blobs?since={seq}`

Returns blobs with `seq > since`, ordered ascending, at most **256 per page**.

```json
{"cursor":2,"more":false,"blobs":[{"seq":1,"id":"...","data":"<base64 standard>"}]}
```

* `cursor` = max seq returned, or `since` if the page is empty.
* `more` = `true` when the page was truncated at 256.
* `data` is standard (not URL-safe) base64 of the exact stored bytes.
* To poll: keep `since = cursor` and repeat while `more` is true / new data arrives.

### `DELETE /v1/ws/{wsid}/blobs/{id}`

`200 {"deleted":true}` on success, `404` if no such blob. Deletion does not
rewind the sequence counter, so seq values are never reused.

### `GET /v1/ws/{wsid}/head`

```json
{"head":2,"count":1}
```

`head` is the highest seq currently stored in the workspace (0 if empty);
`count` is the number of live blobs.

---

## Storage layout

```text
<data>/
└── <wsid>/                              # one directory per workspace
    ├── seq                              # decimal: highest seq ever assigned
    ├── 000000000001-<blob id>.blob      # one opaque file per blob (raw bytes)
    ├── 000000000002-<blob id>.blob
    └── ...
```

* File name: `{seq:012}-{id}.blob` (zero-padded 12-digit decimal seq).
* `seq` is a plain decimal counter persisted per workspace. It only moves
  forward; deletions never reuse a sequence number.
* **Atomic writes:** each blob is written to a temp file in the same directory,
  `fsync`ed, renamed onto its final name, and the directory is `fsync`ed. A
  crash mid-write leaves only a `.tmp-...` file, which is removed at the next
  startup and never becomes visible to clients.
* **Startup rebuild:** the in-memory index is reconstructed by scanning every
  `<wsid>` directory. The next sequence is `max(highest filename seq, seq file)`,
  so restarts (even after deleting the newest blob) cannot renumber anything.
* `--ttl-hours <n>` starts a background thread that sweeps every 60 s and
  deletes blobs whose file mtime is older than `n` hours. Expiry does not
  rewind `seq`.
* The index is a sharded `Arc<Mutex<...>>` map: one mutex for the workspace
  table, one per workspace. `seq` assignment and idempotency checks happen under
  the workspace lock; blob reads happen outside it. Concurrent POSTs to the same
  workspace are serialized for correctness and can never receive a duplicate
  seq.

---

## Threat model

1. **What the operator cannot learn: the content of blobs.** Blobs are opaque
   ciphertext (plus whatever the clients put inside). The relay never parses,
   decrypts, decompresses or logs payloads; it only hashes them to derive an id
   and returns them byte-for-byte. **If clients do not encrypt end-to-end and
   sign their envelopes, this guarantee disappears** — the relay will happily
   store and serve plaintext, and anyone with disk or API access can read it.
2. **What the operator CAN learn: workspace pseudonyms.** The 32-hex `wsid` is
   visible in every request, URL and directory name, as are the existence of
   workspaces, blob ids, per-workspace blob counts, and which workspace a blob
   belongs to. If a wsid is ever correlated with a real identity, membership or
   project, that link applies retroactively to everything stored under it.
3. **What the operator CAN learn: blob sizes, upload timing and counts.** File
   sizes, mtimes, sequence numbers and request logs reveal the size class of
   each encrypted change, when a client was active, upload/download cadence,
   message counts, and rough traffic volume. That is enough for traffic
   analysis even without plaintext: the relay is metadata-transparent.
4. **What the operator CAN do: deny, delay, delete, replay or equivocate.** The
   operator controls the disk and the process. It can drop or reorder
   deliveries, refuse service, delete or roll back blobs, and show different
   histories to different clients. It cannot forge blob *contents* that pass
   client-side signature/decryption checks (assuming clients verify), so
   integrity and authenticity must be enforced by the clients, not the relay.
5. **What the operator CAN learn and do: network and access metadata.**
   Connections expose client IPs, ports, TLS SNI (if terminated here), request
   timing and request patterns. The relay has **no authentication or
   authorization**: `wsid` is a bearer capability, so anyone who learns or
   guesses a workspace id, and any process that can read the data directory, can
   read, list and delete that workspace's blobs. There is no TLS in-process; run
   it behind a TLS terminator if the network path is untrusted. Disk holds
   ciphertext but metadata (workspace ids, sizes, timing, sequence numbers) in
   the clear.

---

## Tests

```bash
cd relay
cargo test
```

The integration suite (`tests/relay.rs`) starts a real relay on an ephemeral
port in a per-test temp directory and talks to it over `std::net::TcpStream`
(hand-rolled HTTP/1.1, no extra test dependencies). Covered:

* health endpoint
* POST → GET `since=0`, base64 round-trip byte-for-byte, id matches blake3
* duplicate POST idempotency (same seq, count unchanged)
* `since` filtering and cursor semantics
* restart on the same data dir preserves blobs and continues the sequence
* malformed wsid rejected (401/400), traversal rejected, nothing created on disk
* oversize body → 413, exact-limit body accepted
* 8 threads × 25 concurrent POSTs → 200 distinct, contiguous seqs; GET returns
  exactly 200 blobs
* 256-per-page pagination with `more`
* delete/head semantics and id validation

The `store` module also has unit tests for wsid validation, file-name parsing and
blob-id derivation.

---

## Scope / non-goals

* Single node, no replication, consensus or federation.
* No authentication, per-user quotas, rate limiting or backpressure.
* No in-process TLS — terminate TLS in front of the relay.
* The relay is not trusted by clients; it is expected to be untrusted. Clients
  must encrypt and authenticate their envelopes.
