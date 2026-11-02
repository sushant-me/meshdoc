#!/usr/bin/env bash
# android-sync-demo.sh — sync a real Android device with a Linux host, both ways.
#
# Uses an Android emulator (any x86_64 or arm64 AVD) over adb. Both sides run the
# same `meshdoc` binary and the same code paths as two phones on a hotspot; only the
# link differs (adb forwarding instead of Wi-Fi).
#
# Requirements: Android SDK + NDK, an AVD, rustup target for the AVD's ABI.
#
# Usage:
#   ./scripts/android-sync-demo.sh                 # uses a running emulator
#   ./scripts/android-sync-demo.sh --boot renthub  # boots that AVD headless first
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
ADB="$ANDROID_HOME/platform-tools/adb"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | tail -1)}"
PASS="${MESHDOC_PASSPHRASE:-android-demo-pass}"
DEVICE_DIR="/data/local/tmp/meshdoc-demo-$$"
trap '"$ADB" shell rm -rf "$DEVICE_DIR" >/dev/null 2>&1 || true; rm -rf "$HOST_DIR"' EXIT
HOST_DIR="$(mktemp -d)"
TARGET="x86_64-linux-android"

if [[ "${1:-}" == "--boot" ]]; then
  AVD="${2:?usage: --boot <avd-name>}"
  echo "booting $AVD headless…"
  "$ANDROID_HOME/emulator/emulator" -avd "$AVD" -no-window -no-audio -no-boot-anim \
    -gpu swiftshader_indirect -no-snapshot >/tmp/meshdoc-emulator.log 2>&1 &
  for _ in $(seq 1 60); do
    [[ "$("$ADB" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]] && break
    sleep 3
  done
fi

ABI="$("$ADB" shell getprop ro.product.cpu.abi 2>/dev/null | tr -d '\r')"
case "$ABI" in
  x86_64*) TARGET="x86_64-linux-android"; CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android24-clang" ;;
  arm64*)  TARGET="aarch64-linux-android"; CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android24-clang" ;;
  *) echo "unsupported device ABI: $ABI" >&2; exit 2 ;;
esac

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
note() { printf '   %s\n' "$1"; }

step "Building meshdoc for the device ($ABI)"
rustup target add "$TARGET" >/dev/null 2>&1 || true
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$CLANG"
export CC_aarch64_linux_android="$CLANG" CC_x86_64_linux_android="$CLANG"
( cd "$ROOT" && cargo build --release --target "$TARGET" --bin meshdoc >/dev/null )
BIN="$ROOT/target/$TARGET/release/meshdoc"
note "$(file -b "$BIN" | cut -c1-70)…"

step "Installing on the device"
"$ADB" push "$BIN" "$DEVICE_DIR/meshdoc" >/dev/null
"$ADB" shell chmod 755 "$DEVICE_DIR/meshdoc"
note "installed at $DEVICE_DIR/meshdoc"

D() { "$ADB" shell "cd $DEVICE_DIR && MESHDOC_PASSPHRASE=$PASS ./meshdoc $*" | tr -d '\r'; }
H() { "$ROOT/target/release/meshdoc" --dir "$HOST_DIR" "$@"; }
export MESHDOC_PASSPHRASE="$PASS"

step "Host identity"
H init >/dev/null
HOSTPUB="$(H id --hex)"
note "host: ${HOSTPUB:0:32}…"

step "Android creates the workspace and invites the host"
D "workspace create --name 'Field Ops'" | sed 's/^/   /'
TOKEN="$(D "workspace invite --for $HOSTPUB")"
note "invite token: ${#TOKEN} bytes"

step "Host joins"
H workspace join --token "$TOKEN" | sed 's/^/   /'

step "Android records a checklist item"
DOC="$(D "doc create --type checklist --title 'Trapped persons'")"
D "checklist add $DOC 'Ward 3 — two adults, one child'" >/dev/null
note "host documents before sync: $(H doc list | wc -l)"

step "Android → host"
D "sync serve --port 7778 --max-sessions 1" >/dev/null 2>&1 &
sleep 2
"$ADB" forward tcp:7778 tcp:7778 >/dev/null
H sync pull 127.0.0.1:7778 | sed 's/^/   /'

step "Host edits while the phone is offline"
H checklist add "$DOC" "Generator fuel — 20L" >/dev/null

step "Host → Android"
"$ADB" reverse tcp:7779 tcp:7779 >/dev/null
"$ROOT/target/release/meshdoc" --dir "$HOST_DIR" sync serve --port 7779 --max-sessions 1 >/dev/null 2>&1 &
sleep 2
D "sync pull 127.0.0.1:7779" | sed 's/^/   /'

step "Convergence"
H render > "$HOST_DIR/host.render" 2>/dev/null || true
D "render" > "$HOST_DIR/android.render"
if diff -q "$HOST_DIR/host.render" "$HOST_DIR/android.render" >/dev/null; then
  printf '\033[32m   CONVERGED — Android device and Linux host hold byte-identical state\033[0m\n'
else
  printf '\033[31m   DIVERGED\033[0m\n'; diff "$HOST_DIR/host.render" "$HOST_DIR/android.render" || true; exit 1
fi
sed 's/^/     /' "$HOST_DIR/host.render"
