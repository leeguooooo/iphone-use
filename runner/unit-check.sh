#!/bin/bash
# Mac-side unit check of the runner's device-independent Swift logic (HTTP parsing, WDA locator
# strategies, W3C pointer actions, screenshot sizing). Compiles those sources with a stub
# IPURBridge for macOS and runs them; needs no device and no signing.
#   --bench   also measure full PNG vs scaled JPEG bytes/time on the repo's phone screenshots.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCES="$SCRIPT_DIR/IPhoneUseRunner/IPhoneUseRunnerUITests"
OUT="${TMPDIR:-/tmp}/ipu-runner-unit-check.$$"
mkdir -p "$OUT"
trap 'rm -rf "$OUT"' EXIT

clang -fobjc-arc -c "$SCRIPT_DIR/unit-check/IPURBridgeStub.m" -o "$OUT/stub.o"
swiftc -O -import-objc-header "$SCRIPT_DIR/unit-check/IPURBridgeStub.h" \
    "$SOURCES/RunnerHTTP.swift" "$SOURCES/RunnerActions.swift" "$SOURCES/RunnerElements.swift" \
    "$SOURCES/RunnerH264.swift" \
    "$SCRIPT_DIR/unit-check/main.swift" "$OUT/stub.o" -o "$OUT/unit-check"
if [ "${1:-}" = "--bench" ]; then
    ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    export IPU_BENCH=1
    IPU_BENCH_IMAGES="$ROOT/assets/demo-l2-cjk.png"
    for image in "$ROOT"/apps/ios/IPhoneUseRemote/Resources/Demo/demo-*.jpg; do
        IPU_BENCH_IMAGES="$IPU_BENCH_IMAGES:$image"
    done
    export IPU_BENCH_IMAGES
fi
"$OUT/unit-check"
