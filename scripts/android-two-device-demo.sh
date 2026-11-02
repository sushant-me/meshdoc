#!/usr/bin/env bash
# android-two-device-demo.sh — two Android devices syncing with each other, both ways.
#
# Runs the same `meshdoc` binary on two Android targets (two emulators, or an
# emulator and a USB device) and makes them converge. adb is used purely as a
# transport tunnel; the sync protocol, crypto and CRDT code are identical to what
# runs between two phones on a Wi-Fi hotspot.
#
# Requirements: Android SDK, NDK, two online devices/emulators (adb devices), and a
# rustup target matching their ABI.
#
# Usage:
#   ./scripts/android-two-device-demo.sh                  # uses the first two devices
#   ./scripts/android-two-device-demo.sh emulator-5554 emulator-5556
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$ANDROID_HOME/platform-tools/adb"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | tail -1)}"
PASS="android-demo-pass"

mapfile -t DEVS < <("$ADB" devices | awk '/\tdevice$/{print $1}')
A="${1:-${DEVS[0]:-}}"
B="${2:-${DEVS[1]:-}}"
[[ -n "$A" && -n "$B" ]] || { echo "need two online devices; see: adb devices" >&2; exit 2; }

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
note() { printf '   %s\n' "$1"; }

ABI="$("$ADB" -s "$A" shell getprop ro.product.cpu.abi | tr -d '\r')"
case "$ABI" in
  x86_64*) TARGET="x86_64-linux-android"; CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android24-clang" ;;
  arm64*)  TARGET="aarch64-linux-android"; CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang" ;;
  *) echo "unsupported ABI: $ABI" >&2; exit 2 ;;
esac
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$CLANG"
export CC_aarch64_linux_android="$CLANG" CC_x86_64_linux_android="$CLANG"

step "Two Android devices"
for d in "$A" "$B"; do
  note "$d: Android $("$ADB" -s "$d" shell getprop ro.build.version.release | tr -d '\r') / $ABI"
done

step "Building meshdoc for $TARGET"
( cd "$ROOT" && rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release --target "$TARGET" --bin meshdoc >/dev/null )
BIN="$ROOT/target/$TARGET/release/meshdoc"

DA_DIR="/data/local/tmp/phoneA-$$"; DB_DIR="/data/local/tmp/phoneB-$$"
trap '"$ADB" -s "$A" shell rm -rf "$DA_DIR" >/dev/null 2>&1 || true
      "$ADB" -s "$B" shell rm -rf "$DB_DIR" >/dev/null 2>&1 || true' EXIT

for d in "$A" "$B"; do
  "$ADB" -s "$d" push "$BIN" /data/local/tmp/meshdoc >/dev/null
  "$ADB" -s "$d" shell chmod 755 /data/local/tmp/meshdoc
done
DA() { "$ADB" -s "$A" shell "cd /data/local/tmp && MESHDOC_PASSPHRASE=$PASS ./meshdoc --dir '$DA_DIR' $*" | tr -d '\r'; }
DB() { "$ADB" -s "$B" shell "cd /data/local/tmp && MESHDOC_PASSPHRASE=$PASS ./meshdoc --dir '$DB_DIR' $*" | tr -d '\r'; }

step "Both devices create identities"
DA init >/dev/null; DB init >/dev/null
note "A: $(DA id --hex | cut -c1-24)…"
note "B: $(DB id --hex | cut -c1-24)…"

step "Device A creates the workspace and invites Device B"
DA "workspace create --name 'Field Ops'" | sed 's/^/   /'
TOKEN="$(DA "workspace invite --for $(DB id --hex)")"
note "invite token: ${#TOKEN} bytes"
DB "workspace join --token '$TOKEN'" | sed 's/^/   /'

step "Device A records a checklist item"
DOC="$(DA "doc create --type checklist --title 'Trapped persons'")"
DA "checklist add '$DOC' 'Ward 3 — two adults, one child'" >/dev/null
note "B documents before sync: $(DB 'doc list' | wc -l)"

step "A → B over the tunnel"
"$ADB" -s "$A" forward tcp:7790 tcp:7790 >/dev/null
"$ADB" -s "$B" reverse tcp:7790 tcp:7790 >/dev/null
"$ADB" -s "$A" shell "cd /data/local/tmp && MESHDOC_PASSPHRASE=$PASS ./meshdoc --dir '$DA_DIR' sync serve --port 7790 --max-sessions 1" >/dev/null 2>&1 &
SERVE=$!; sleep 3
DB "sync pull 127.0.0.1:7790" | sed 's/^/   /'
wait $SERVE 2>/dev/null || true

step "Device B edits while the phones are apart"
DB "checklist add '$DOC' 'Generator fuel — 20L'" >/dev/null

step "B → A over the tunnel"
"$ADB" -s "$B" forward tcp:7791 tcp:7791 >/dev/null
"$ADB" -s "$A" reverse tcp:7791 tcp:7791 >/dev/null
"$ADB" -s "$B" shell "cd /data/local/tmp && MESHDOC_PASSPHRASE=$PASS ./meshdoc --dir '$DB_DIR' sync serve --port 7791 --max-sessions 1" >/dev/null 2>&1 &
SERVE=$!; sleep 3
DA "sync pull 127.0.0.1:7791" | sed 's/^/   /'
wait $SERVE 2>/dev/null || true

step "Convergence"
DA "render" > /tmp/phoneA.render
DB "render" > /tmp/phoneB.render
if diff -q /tmp/phoneA.render /tmp/phoneB.render >/dev/null; then
  printf '\033[32m   CONVERGED — both Android devices hold byte-identical state\033[0m\n'
else
  printf '\033[31m   DIVERGED\033[0m\n'; diff /tmp/phoneA.render /tmp/phoneB.render || true; exit 1
fi
sed 's/^/     /' /tmp/phoneA.render
