#!/usr/bin/env bash
# Builds and signs the Sonora arm64 APK from this repository.
# Usage: android/build-apk.sh [repo root] [out.apk]
# Requires JAVA_HOME, ANDROID_SDK_ROOT (platform 35, build-tools 35, NDK 27.2.12479018),
# and KEYSTORE pointing at a keystore whose sibling .txt lists:
#   store password: ...
#   alias: ...
# The keystore and that text file stay out of git.
set -euo pipefail
W=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$W/.." && pwd)
REPO=${1:-$ROOT}
OUT=${2:-$ROOT/Sonora-mobile.apk}
SDK=${ANDROID_SDK_ROOT:-${ANDROID_HOME:-/home/box/android-sdk}}
export ANDROID_NDK_HOME=${ANDROID_NDK_HOME:-$SDK/ndk/27.2.12479018}
NDK=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64
BT=$SDK/build-tools/35.0.0
JAR=$SDK/platforms/android-35/android.jar
export JAVA_HOME=${JAVA_HOME:-/home/box/jdk17}
export PATH=$JAVA_HOME/bin:${HOME:-}/.cargo/bin:$PATH
# A host mold or other linker flag must not leak into the Android build.
export RUSTFLAGS=""

if [ -z "${KEYSTORE:-}" ]; then
  echo "Set KEYSTORE to a keystore path (the password and alias are read from KEYSTORE.txt beside it)." >&2
  exit 1
fi
KS=$KEYSTORE
if [ ! -f "$KS" ]; then
  echo "KEYSTORE is not a file: $KS" >&2
  exit 1
fi
KSPW=$(sed -n 's/^store password: //p' "$KS.txt")
ALIAS=$(sed -n 's/^alias: //p' "$KS.txt")
if [ -z "$KSPW" ] || [ -z "$ALIAS" ]; then
  echo "Could not read 'store password:' and 'alias:' from $KS.txt" >&2
  exit 1
fi

B=$(mktemp -d)
trap 'rm -rf "$B"' EXIT

# java wrapper -> classes.dex
mkdir -p "$B/classes" "$B/dex"
javac --release 11 -cp "$JAR" -d "$B/classes" "$W"/java/io/github/sonorahq/sonora/*.java
jar cf "$B/activity.jar" -C "$B/classes" .
"$BT/d8" --min-api 26 --lib "$JAR" --output "$B/dex" "$B/activity.jar" "$W/deps/rustls-pv.jar" "$W/deps/kotlin-stdlib.jar"
"$BT/aapt2" compile --dir "$W/res" -o "$B/res.zip"

# rust lib, with the vendored gpui/android-activity patched in only for this build
cd "$REPO"
cp Cargo.toml "$B/Cargo.toml.bak"
cp Cargo.lock "$B/Cargo.lock.bak"
restore_cargo() { cp "$B/Cargo.toml.bak" "$REPO/Cargo.toml"; cp "$B/Cargo.lock.bak" "$REPO/Cargo.lock"; }
trap 'restore_cargo; rm -rf "$B"' EXIT
python3 - <<'PY'
from pathlib import Path
p = Path("Cargo.toml")
s = p.read_text()
s = s.replace("[patch.crates-io]\n", "[patch.crates-io]\nandroid-activity = { path = \"vendor/android-activity\" }\n", 1)
s += "\n[patch.\"https://github.com/sonorahq/gpui\"]\ngpui = { path = \"vendor/gpui/crates/gpui\" }\ngpui_platform = { path = \"vendor/gpui/crates/gpui_platform\" }\n"
p.write_text(s)
PY
export CC_aarch64_linux_android=$NDK/bin/aarch64-linux-android26-clang
export CXX_aarch64_linux_android=$NDK/bin/aarch64-linux-android26-clang++
export AR_aarch64_linux_android=$NDK/bin/llvm-ar
# fdk-aac-sys includes AOSP-only <log/log.h> on Android and builds its C++ with the C driver
export CFLAGS_aarch64_linux_android="-I$W/deps/fdk-stub"
export CXXFLAGS_aarch64_linux_android="-I$W/deps/fdk-stub"
export CMAKE_TOOLCHAIN_FILE_aarch64_linux_android=$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$NDK/bin/aarch64-linux-android26-clang
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-L$NDK/sysroot/usr/lib/aarch64-linux-android/26 -C link-arg=-landroid -C link-arg=-llog -C link-arg=-lc++_shared"
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android="--sysroot=$NDK/sysroot -isystem $NDK/sysroot/usr/include/aarch64-linux-android -isystem $NDK/sysroot/usr/include"
TD=${CARGO_TARGET_DIR:-$REPO/target}
cargo build --target aarch64-linux-android --package sonora --lib --release

# package, align, sign
mkdir -p "$B/apk/lib/arm64-v8a"
cp "$TD/aarch64-linux-android/release/libsonora.so" "$B/apk/lib/arm64-v8a/"
cp "$NDK/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so" "$B/apk/lib/arm64-v8a/"
cp "$B/dex/classes.dex" "$B/apk/"
"$BT/aapt2" link -I "$JAR" --manifest "$W/AndroidManifest.xml" -R "$B/res.zip" --auto-add-overlay -o "$B/base.apk"
python3 - "$B" <<'PY'
import sys, zipfile, os
b = sys.argv[1]
with zipfile.ZipFile(f"{b}/base.apk", "a", zipfile.ZIP_STORED) as z:
    for root, _, files in os.walk(f"{b}/apk"):
        for f in files:
            full = os.path.join(root, f)
            z.write(full, os.path.relpath(full, f"{b}/apk"))
PY
"$BT/zipalign" -f -p 4 "$B/base.apk" "$B/aligned.apk"
"$BT/apksigner" sign --ks "$KS" --ks-key-alias "$ALIAS" --ks-pass "pass:$KSPW" --key-pass "pass:$KSPW" --out "$OUT" "$B/aligned.apk"
"$BT/apksigner" verify "$OUT" && ls -lh "$OUT"
