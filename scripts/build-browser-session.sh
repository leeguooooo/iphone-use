#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${IPU_BROWSER_TARGET_DIR:-$ROOT/target-browser-session}"
CARGO_BUILD=(cargo)
if [ -x "$HOME/.local/bin/heavy-cargo" ]; then CARGO_BUILD=("$HOME/.local/bin/heavy-cargo"); fi
case "${1:-device}" in
  device) TRIPLE=aarch64-apple-ios; PLATFORM=iphoneos ;;
  simulator) TRIPLE=aarch64-apple-ios-sim; PLATFORM=iphonesimulator ;;
  *) echo "Usage: build-browser-session.sh device|simulator" >&2; exit 2 ;;
esac
rustup target add "$TRIPLE" >/dev/null 2>&1
IPHONEOS_DEPLOYMENT_TARGET=15.0 CARGO_TARGET_DIR="$TARGET_DIR" "${CARGO_BUILD[@]}" build --manifest-path "$ROOT/Cargo.toml" -p browser-session --lib --release --target "$TRIPLE"
mkdir -p "$ROOT/apps/ios/SessionLibraries/$PLATFORM"
cp "$TARGET_DIR/$TRIPLE/release/libipu_browser_session.a" "$ROOT/apps/ios/SessionLibraries/$PLATFORM/"
