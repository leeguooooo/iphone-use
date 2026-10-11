#!/usr/bin/env bash
# Build the on-phone daemon (crates/ios-embed) as a static library for iOS devices and print its
# path on the last line of stdout. Hand that path to runner/build.sh as IPU_EMBED_LIB.
#
#   IPU_EMBED_TARGET_DIR=dir   cargo target dir (default: target)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${IPU_EMBED_TARGET_DIR:-$ROOT/target}"
rustup target add aarch64-apple-ios >/dev/null 2>&1 || true
CARGO_BUILD=(cargo)
if [ -x "$HOME/.local/bin/heavy-cargo" ]; then
    CARGO_BUILD=("$HOME/.local/bin/heavy-cargo")
fi
# Match the runner's deployment target so the linker does not warn about every object.
IPHONEOS_DEPLOYMENT_TARGET=15.0 CARGO_TARGET_DIR="$TARGET_DIR" \
    "${CARGO_BUILD[@]}" build --manifest-path "$ROOT/Cargo.toml" -p ios-embed --release --target aarch64-apple-ios >&2
echo "$TARGET_DIR/aarch64-apple-ios/release/libipu_embed.a"
