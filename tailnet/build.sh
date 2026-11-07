#!/usr/bin/env bash
# Build a universal (arm64 + x86_64) darwin iphone-use-tailnet binary.
#   VERSION=0.29.0 OUT=dist/iphone-use-tailnet tailnet/build.sh
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
VERSION="${VERSION:-dev}"
OUT="${OUT:-$here/dist/iphone-use-tailnet}"
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
# The daemon targets macOS 14.0 (deploy/Info.plist LSMinimumSystemVersion).
# With CGO_ENABLED=0 Go's linker stamps its own, lower minimum (macOS 13.0 for
# current Go), so the binary runs wherever the daemon does; the variable is
# exported only for consistency with the rest of the build.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-14.0}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$(dirname "$OUT")"

cd "$here"
for arch in arm64 amd64; do
  CGO_ENABLED=0 GOOS=darwin GOARCH="$arch" go build -trimpath \
    -ldflags "-s -w -X main.version=$VERSION" -o "$tmp/$arch" .
done
lipo -create "$tmp/arm64" "$tmp/amd64" -output "$OUT"
lipo -info "$OUT"
