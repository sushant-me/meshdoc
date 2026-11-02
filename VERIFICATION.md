# Verification record

Evidence that the prototype does what the README says. All commands were run in this
workspace on 2026-11-02 (Asia/Kathmandu).

## Test suite — 111 tests, all passing

```
cargo test --workspace
```

| Suite | Tests | Result |
|---|---|---|
| `src/` unit tests (crypto, ids, membership, docs, change, sync, transport, store, node) | 58 | pass |
| `tests/convergence.rs` — partition/merge fuzzing | 7 | pass |
| `tests/relay_client.rs` — relay HTTP client | 30 | pass |
| `relay/` unit tests | 4 | pass |
| `relay/tests/relay.rs` — relay server over real TCP | 11 | pass |
| doc-tests | 1 | pass |
| **total** | **111** | **0 failed** |

## Convergence fuzzing — 20 consecutive runs

The fuzzer generates seeded random edit/partition schedules across 3–6 replicas,
heals the network, and asserts every replica converges to identical state, that no
replica holds a change it could not apply, and that nothing was lost.

```
SEEDS=40 cargo test --test convergence -- --test-threads=1     × 20
TOTAL: PASS=20 FAIL=0 of 20
```

Raw log: [`demo/verification.log`](verification.log).

A full-length run (120 seeds, all seven convergence tests) also passes; it takes ~130 s.

## End-to-end demo — all assertions

```
cargo run --release --bin meshdoc-demo
All assertions passed
```

Seven stages, each assertion enforced by the program's exit status: onboarding by
invite, typed documents, partition, convergence, gossip via a third node, forgery
rejection, and relay store-and-forward.

## Two real processes over a real socket

```
./scripts/two-device-demo.sh
CONVERGED — both devices hold byte-identical state
```

Two independent processes, two independent passphrase-encrypted stores, one workspace,
syncing over TCP. Divergence before reconnection and byte-identical state after is
shown in the output. Recorded as `demo/two-device-sync.mp4` (42 s).

## Two Android devices syncing with each other

Two Android 15 (API 35, x86_64) devices ran the same `meshdoc` binary and synced
**directly with each other**, in both directions:

```
scripts/android-two-device-demo.sh
   A → B   pulled 2 changes (2 applied)
   B → A   pulled 1 changes (1 applied)
   CONVERGED — both Android devices hold byte-identical state
```

Device A created the workspace and invited B; A recorded a checklist item; B pulled it;
B added a second item while the devices were apart; A pulled it back. Both `render`
outputs were byte-identical. Recorded as `demo/two-android-devices.mp4` (37 s).

This is the demo criterion met on Android itself. adb is used only as the transport
tunnel between the two devices — the identity, invite, encryption, CRDT and sync code
are the same code that would run over a Wi-Fi hotspot between two phones.

## Executed on Android — device syncing with a Linux host

The CLI was cross-compiled for Android, installed on an **Android 15 (API 35) x86_64
emulator**, and synced with a Linux host over adb forwarding:

```
scripts/android-sync-demo.sh
...
   Android → host   pulled 2 changes (2 applied)
   host → Android   pulled 1 changes (1 applied)
   CONVERGED — Android device and Linux host hold byte-identical state
```

The Android side created the workspace and minted the invite; the host joined; the
Android device pushed a checklist; the host edited while the phone was offline; the
phone pulled. Final `render` output was byte-identical on both sides. Recorded as
`demo/android-sync.mp4` (37 s).

This is the same binary and the same code paths that would run on two phones; only the
link differs (adb forwarding here, Wi-Fi/hotspot between two phones in the field).

## Android build

```
./scripts/build-android.sh
dist/android-arm64/meshdoc: ELF 64-bit LSB pie executable, ARM aarch64,
  dynamically linked, interpreter /system/bin/linker64, for Android 24,
  built by NDK r28c
```

Correct Android arm64 executable, suitable for Termux on two phones over a hotspot.
**Not executed on a physical phone** in this workspace — none was attached.

## Bugs found and fixed by this verification

The fuzzing and repeated runs were not ceremony; they found real defects:

1. **yrs transaction deadlock** — container lookups were performed while a write
   transaction was open. Every CRDT operation that touched a document hung.
2. **Non-convergent checklist ordering** — concurrent appends could produce the same
   fractional position, and the sort had no deterministic tie-break, so replicas
   ordered items differently. Tie-break is now `(pos, id)`.
3. **Order-dependent membership evaluation** — removals were applied in `BTreeMap`
   order, so a removal could precede its own add. Evaluation now sorts by
   `(epoch, additions-before-removals, content address)`.
4. **Membership entries dropped on import** — a child entry arriving before its parent
   failed silently, so a replica could reject an authorised author's changes forever.
   Import now retries to a fixpoint.
5. **Cross-workspace sync was accepted** — `sync_with` did not verify the workspace id.
6. **Container-creation race** — a receiver created document containers before the
   origin's update arrived, producing two competing Yjs objects whose visibility
   depended on client-id ordering. Containers are now created by the origin only.
7. **Note type was the wrong CRDT** — a `YText` was used for whole-body replacement,
   which merged concurrent replacements by concatenation. A note body is now a
   last-writer-wins register, which is the correct type for that semantic.
8. **Lost operations from delta-based changes** — emitting deltas could skip an
   operation the replica knew but had not yet transmitted. Changes now carry full
   document state; this is the conservative choice and is documented as a known
   bandwidth trade-off.

## What is *not* verified

- BLE and Wi-Fi Direct transports — not implemented.
- The Android binary on physical hardware.
- Anything at scale beyond 6 replicas / ~200 operations per schedule.
- The relay is plaintext HTTP; TLS termination is deployment work, not done here.
