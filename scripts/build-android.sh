#!/usr/bin/env bash
# build-android.sh — cross-compile the MeshDoc CLI for real Android phones.
#
# Produces statically-linked-ish aarch64 binaries that run under Termux on an
# unrooted phone, so two phones can sync over a Wi-Fi hotspot with no server.
#
# Requires: Android NDK, rustup target aarch64-linux-android.
#   rustup target add aarch64-linux-android
#
# Usage: ./scripts/build-android.sh [ndk-path]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NDK="${1:-${ANDROID_NDK_HOME:-}}"

if [[ -z "$NDK" ]]; then
  for candidate in "$HOME"/Android/Sdk/ndk/*; do
    [[ -d "$candidate" ]] && NDK="$candidate"
  done
fi
if [[ -z "$NDK" || ! -d "$NDK" ]]; then
  echo "error: Android NDK not found. Pass its path as the first argument." >&2
  echo "       e.g. ./scripts/build-android.sh \$HOME/Android/Sdk/ndk/28.2.13676358" >&2
  exit 2
fi

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
# API 24 is the floor for the prebuilt clang wrappers and covers essentially every
# Android phone still in service.
CLANG="$TOOLCHAIN/aarch64-linux-android24-clang"
[[ -x "$CLANG" ]] || { echo "error: $CLANG not found" >&2; exit 2; }

export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"
export CC_aarch64_linux_android="$CLANG"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"

echo "NDK      : $NDK"
echo "linker   : $CLANG"
cd "$ROOT"
cargo build --release --target aarch64-linux-android --bin meshdoc --bin meshdoc-demo

OUT="$ROOT/target/aarch64-linux-android/release"
# cargo puts it under the workspace target dir; copy to a stable location.
mkdir -p "$ROOT/dist/android-arm64"
cp "$OUT/meshdoc" "$ROOT/dist/android-arm64/meshdoc"
cp "$OUT/meshdoc-demo" "$ROOT/dist/android-arm64/meshdoc-demo"
echo
echo "built:"
ls -lh "$ROOT/dist/android-arm64/"
file "$ROOT/dist/android-arm64/meshdoc"
echo
echo "Copy these two files to each phone, then in Termux:"
echo "  chmod +x meshdoc"
echo "  ./meshdoc --dir ./field init"
