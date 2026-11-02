#!/usr/bin/env bash
# two-device-demo.sh — run two independent MeshDoc clients that sync over TCP.
#
# This is the recordable version of the demo: two separate processes, each with its
# own encrypted store, converging over a real socket. On one machine they talk over
# loopback; on two machines on the same Wi-Fi, point SYNC_HOST at the other device.
#
# Usage:
#   ./scripts/two-device-demo.sh
#   SYNC_HOST=192.168.1.42 ./scripts/two-device-demo.sh     # two machines
#
# Requires the binaries: cargo build --release
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${ROOT}/target/release/meshdoc"
PORT="${PORT:-7791}"
HOST="${SYNC_HOST:-127.0.0.1}"

if [[ ! -x "$BIN" ]]; then
  echo "building release binaries…"
  (cd "$ROOT" && cargo build --release --bin meshdoc)
fi

export MESHDOC_PASSPHRASE="${MESHDOC_PASSPHRASE:-demo-passphrase-not-for-real-use}"

WORK="$(mktemp -d)"
FIELD="${WORK}/field-phone"
HQ="${WORK}/hq-phone"
trap 'rm -rf "$WORK"' EXIT

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
note() { printf '   %s\n' "$1"; }

step "Two independent devices, two independent encrypted stores"
"$BIN" --dir "$FIELD" init >/dev/null
"$BIN" --dir "$HQ" init >/dev/null
HQ_PUB="$("$BIN" --dir "$HQ" id --hex | awk '/public key/{print $4}')"
note "field phone store : ${FIELD}"
note "HQ store          : ${HQ}"
note "HQ public identity: ${HQ_PUB:0:32}…"

step "Field phone creates a workspace and invites HQ"
"$BIN" --dir "$FIELD" workspace create --name "Quake Response" >/dev/null
TOKEN="$("$BIN" --dir "$FIELD" workspace invite --for "$HQ_PUB")"
note "invite token is ${#TOKEN} bytes (QR-sized, contains a key grant sealed to HQ)"

"$BIN" --dir "$HQ" workspace join --token "$TOKEN" >/dev/null
note "HQ joined. No server was involved."

step "Field phone records what it finds, then goes offline"
DOC="$("$BIN" --dir "$FIELD" doc create --type checklist --title 'Trapped persons')"
"$BIN" --dir "$FIELD" checklist add "$DOC" "Ward 3 — two adults, one child" >/dev/null
note "field phone: 1 item"
note "HQ: 0 documents (it has not synced yet)"

step "HQ comes into range and pulls"
"$BIN" --dir "$FIELD" sync serve --port "$PORT" --max-sessions 1 >/dev/null 2>&1 &
SERVER=$!
sleep 1
"$BIN" --dir "$HQ" sync pull "${HOST}:${PORT}"
wait "$SERVER" 2>/dev/null || true

step "Both edit while disconnected — a partition"
ITEM="$("$BIN" --dir "$FIELD" checklist add "$DOC" "Water tanker requested")"
"$BIN" --dir "$HQ" checklist add "$DOC" "Generator fuel — 20L" >/dev/null
note "field phone items: $("$BIN" --dir "$FIELD" doc list | grep -c checklist || true)"
echo "   field phone:"
"$BIN" --dir "$FIELD" render | sed 's/^/     /'
echo "   HQ:"
"$BIN" --dir "$HQ" render | sed 's/^/     /'

step "They meet again — both directions reconcile"
"$BIN" --dir "$HQ" sync serve --port "$((PORT + 1))" --max-sessions 1 >/dev/null 2>&1 &
SERVER=$!
sleep 1
"$BIN" --dir "$FIELD" sync pull "${HOST}:$((PORT + 1))"
wait "$SERVER" 2>/dev/null || true
# second contact so HQ also receives what the field phone learned
"$BIN" --dir "$FIELD" sync serve --port "$((PORT + 2))" --max-sessions 1 >/dev/null 2>&1 &
SERVER=$!
sleep 1
"$BIN" --dir "$HQ" sync pull "${HOST}:$((PORT + 2))"
wait "$SERVER" 2>/dev/null || true

step "Result"
"$BIN" --dir "$FIELD" render > "${WORK}/a.txt"
"$BIN" --dir "$HQ" render > "${WORK}/b.txt"
if diff -q "${WORK}/a.txt" "${WORK}/b.txt" >/dev/null; then
  printf '\033[32m   CONVERGED — both devices hold byte-identical state\033[0m\n'
else
  printf '\033[31m   DIVERGED\033[0m\n'; diff "${WORK}/a.txt" "${WORK}/b.txt" || true; exit 1
fi
sed 's/^/     /' "${WORK}/a.txt"
printf '\n   (item %s tracked on both sides)\n' "${ITEM:0:8}"
