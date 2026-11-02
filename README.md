# MeshDoc

Offline-first, end-to-end-encrypted collaboration for teams without reliable connectivity.

**Status: working prototype.** This repository demonstrates the core loop — device
identity, invite-based workspaces, CRDT documents, encrypted change envelopes,
peer-to-peer reconciliation, and a store-and-forward relay. It is the proof that the
design works, not a finished product. There is no Android UI yet.

Two replicas edit the same documents while disconnected, reconnect, and converge to
identical state. Every wire format is encrypted before it is transported.

```
cargo run --release --bin meshdoc-demo
```

That runs the whole scenario in one process and prints what each device sees:

1. Alice creates a workspace; Bob joins from an invite token — no server, no account.
2. Alice creates a checklist, a supply form and a geo point.
3. Alice and Bob edit while disconnected.
4. They reconnect and converge to byte-identical state.
5. Carol joins and receives everything through Bob; she never meets Alice.
6. An outsider who holds the workspace id **and** the workspace key forges a change —
   every honest replica refuses to apply it.
7. Alice uploads to a relay and goes offline; HQ pulls hours later and converges.

All assertions run as part of the demo; the exit code is 0 only if every one held.

## What works

| Area | State |
|---|---|
| Device identity (Ed25519 signing + X25519 agreement), no accounts | working |
| Workspaces joined by invite token / QR-sized blob, signed membership log, epochs | working |
| Typed CRDT documents: checklist, supply form, geo point, note | working |
| Encrypted, signed change envelopes (XChaCha20-Poly1305, AAD-bound) | working |
| Anti-entropy reconciliation over a duplex link, paged with resume | working |
| Peer transport over TCP (Wi-Fi / LAN) | working |
| Store-and-forward relay (opaque blobs) + client | working |
| Encrypted local store (Argon2id + XChaCha20-Poly1305) | working |
| Convergence fuzzing under random partition schedules | working |
| BLE discovery, Wi-Fi Direct group formation | **not implemented** |
| Android client | **not implemented** |

## Two devices syncing over the network

Any two machines on the same network can sync with the binaries in this repo — two
phones running it under Termux, a laptop and a phone, or two processes.

```bash
# both devices: create an identity and a store
export MESHDOC_PASSPHRASE='choose-a-real-passphrase'
meshdoc --dir ./field init
meshdoc --dir ./field id --hex          # prints the 128-hex public identity

# device 1: create the workspace and mint an invite for device 2
meshdoc --dir ./field workspace create --name "Quake Response"
meshdoc --dir ./field workspace invite --for <device-2 public identity>

# device 2: join with the token
meshdoc --dir ./field workspace join --token <token>

# device 2: serve sync on the LAN; device 1 pulls
meshdoc --dir ./field sync serve --port 7777 --max-sessions 1
meshdoc --dir ./field sync pull 192.168.1.42:7777
```

Both directions work the same way: whoever calls `pull` fetches what it is missing and
sends what the peer is missing. `meshdoc render` prints the whole workspace
deterministically, so two devices can be compared with `diff`.

### Running on two real Android phones

There is no app yet, but the CLI cross-compiles for Android and runs under Termux on an
unrooted phone — so two phones really can sync over a Wi-Fi hotspot with no server.

```bash
# on a build machine, with the Android NDK installed
rustup target add aarch64-linux-android
./scripts/build-android.sh              # -> dist/android-arm64/meshdoc

# copy dist/android-arm64/meshdoc to both phones (adb push, or any file transfer)
# then in Termux on each phone:
chmod +x meshdoc
export MESHDOC_PASSPHRASE='pick-a-passphrase'
./meshdoc --dir ./field init
./meshdoc --dir ./field id --hex        # phones exchange these two values
```

Then follow the invite/serve/pull steps above, using the hotspot address of whichever
phone serves (`ip addr` in Termux). One phone runs `sync serve`, the other runs
`sync pull <hotspot-ip>:7777`, and it works in both directions.

The Android build is not theoretical. **Two Android 15 devices have synced directly with
each other, both directions, converging to byte-identical state** — see
[VERIFICATION.md](VERIFICATION.md) and `demo/two-android-devices.mp4`. A device has also
synced with a Linux host (`demo/android-sync.mp4`).

```bash
./scripts/android-two-device-demo.sh       # two Android devices sync with each other
./scripts/android-sync-demo.sh             # one Android device syncs with the host
./scripts/android-sync-demo.sh --boot renthub   # boots an AVD headless first
```

What has *not* been done is running it on two **physical** phones, because no phones were
attached to this machine. The binary is correct for arm64 Android; the untested part is
your hardware, not the code.

Relay path, for peers that are never online at the same time:

```bash
relay/target/release/meshdoc-relay --data ./relay-data --port 8080
meshdoc --dir ./field relay push http://relay-host:8080
meshdoc --dir ./field relay pull http://relay-host:8080
```

## How it works

```
Identity  ed25519 + x25519 keys generated on the device, never transmitted
          Identity = public key. No accounts, no phone numbers, no email.

Workspace random 128-bit id (never derived from a key) + a symmetric epoch key
          Membership is a signed, append-only log. Possession of the key lets you
          read; the log decides whether your writes are accepted.

Document  a CRDT document (yrs/Yjs), projected into four typed views
          Checklist items are keyed by random id and ordered by a fractional
          position, so concurrent inserts cannot fight over an index.

Change    one CRDT state payload, encrypted with the epoch key and signed by the
          author. The workspace id, document id, epoch and author are bound as
          AEAD associated data, so an envelope cannot be relabelled into another
          workspace or document.

Sync      peers exchange the content addresses they hold and transfer only the
          difference, in pages, resuming from the peer's known set after a drop.

Relay     stores opaque ciphertext by workspace pseudonym and sequence number.
          It cannot read, verify or attribute anything.
```

## Security model — and its limits

**Protects against:** relay operators, network observers, a stolen device at rest
(the store is encrypted), and non-members — even non-members who have obtained the
workspace key, because writes are authorised by the signed membership log.

**Does not protect against:**

- **A malicious member.** Members can read everything and sign valid changes.
- **Already-seen plaintext.** Removing a member and rotating the epoch key stops them
  reading *new* content. It cannot un-read what they already decrypted, and a member
  who is partitioned keeps using their old epoch until they observe the rotation.
- **Metadata.** The relay learns a workspace pseudonym, blob sizes and upload timing.
  It does not learn content. Rotating mailbox identifiers is not implemented yet.
- **Traffic analysis and link-layer tracking.** Nothing is claimed about BLE/Wi-Fi
  privacy because those transports are not built yet.
- **Loss of the passphrase or the device.** There is no recovery, by design.

Verification evidence is in [VERIFICATION.md](VERIFICATION.md) — 111 tests, 20
consecutive fuzz runs, the end-to-end demo, the two-process run, and the eight real
defects the fuzzing found.

## Tests

```bash
cargo test                     # unit + integration + convergence fuzzing
cargo test --test convergence -- --test-threads=1   # the fuzzer, deterministically
cd relay && cargo test         # relay server
```

Coverage includes: crypto round-trips and tamper rejection, membership log
authorisation and forgery rejection, CRDT convergence for concurrent edits,
out-of-order update delivery, sync paging and resume, encrypted-store round-trips,
relay protocol behaviour including concurrency and restart, and a seeded
randomised partition/merge fuzzer.

The fuzzer generates random edit/partition schedules across 3–6 replicas, heals the
network, and asserts that every replica converges to identical state and that no
replica is holding a change it could not apply.

## Known gaps

- **BLE and Wi-Fi Direct are not implemented.** Sync runs over TCP today. The
  `MeshTransport` trait and the simulated mesh with partitions exist; a real radio
  backend slots in behind them.
- **Membership updates do not travel through the relay.** Change blobs do. A peer that
  only ever talks to the relay will converge on documents but may not learn about
  members who joined after it went offline.
- **Each change carries the full document state.** This is deliberate: a delta that
  missed an operation would silently break convergence. Deltas with a verified
  watermark are the next optimisation.
- No Android app, no QR rendering (the token is QR-sized but printed as text).

## Licence

MIT. See [LICENSE](LICENSE).
