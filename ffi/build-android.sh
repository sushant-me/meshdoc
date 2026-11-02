#!/usr/bin/env bash
# build-android.sh — build libmeshdoc.so for Android and drop it where Flutter looks.
#
#   ffi/build-android.sh [ndk-path]
#
# Output: ffi/dist/android/<abi>/libmeshdoc.so
#         app/android/app/src/main/jniLibs/<abi>/libmeshdoc.so
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NDK="${1:-${ANDROID_NDK_HOME:-}}"
if [[ -z "$NDK" ]]; then
  for c in "$HOME"/Android/Sdk/ndk/*; do [[ -d "$c" ]] && NDK="$c"; done
fi
[[ -n "$NDK" && -d "$NDK" ]] || { echo "Android NDK not found; pass its path" >&2; exit 2; }
TC="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
API=24

build() {
  local abi="$1" rusttarget="$2" clang="$3"
  echo "--- $abi ($rusttarget)"
  rustup target add "$rusttarget" >/dev/null 2>&1 || true
  local envname
  envname="$(echo "$rusttarget" | tr 'a-z-' 'A-Z_')"
  env "CARGO_TARGET_${envname}_LINKER=$clang" \
      "CC_${rusttarget//-/_}=$clang" \
      "AR_${rusttarget//-/_}=$TC/llvm-ar" \
      cargo build --release --target "$rusttarget" -p meshdoc-ffi
  local out="target/$rusttarget/release/libmeshdoc_ffi.so"
  [[ -f "$out" ]] || { echo "expected $out" >&2; exit 1; }
  mkdir -p "$ROOT/ffi/dist/android/$abi" "$ROOT/app/android/app/src/main/jniLibs/$abi"
  cp "$out" "$ROOT/ffi/dist/android/$abi/libmeshdoc.so"
  cp "$out" "$ROOT/app/android/app/src/main/jniLibs/$abi/libmeshdoc.so"
  file "$ROOT/ffi/dist/android/$abi/libmeshdoc.so"
}

build arm64-v8a   aarch64-linux-android "$TC/aarch64-linux-android${API}-clang"
build x86_64      x86_64-linux-android  "$TC/x86_64-linux-android${API}-clang"

echo
echo "bundled into the app:"
find "$ROOT/app/android/app/src/main/jniLibs" -name '*.so' -exec ls -lh {} \;
