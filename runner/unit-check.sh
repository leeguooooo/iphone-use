#!/bin/bash
# Mac-side unit check of the runner's device-independent Swift logic (HTTP parsing, WDA locator
# strategies, W3C pointer actions). Compiles those sources with a stub IPURBridge for macOS and
# runs them; needs no device and no signing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCES="$SCRIPT_DIR/IPhoneUseRunner/IPhoneUseRunnerUITests"
OUT="${TMPDIR:-/tmp}/ipu-runner-unit-check.$$"
mkdir -p "$OUT"
trap 'rm -rf "$OUT"' EXIT

clang -fobjc-arc -c "$SCRIPT_DIR/unit-check/IPURBridgeStub.m" -o "$OUT/stub.o"
swiftc -O -import-objc-header "$SCRIPT_DIR/unit-check/IPURBridgeStub.h" \
    "$SOURCES/RunnerHTTP.swift" "$SOURCES/RunnerActions.swift" "$SOURCES/RunnerElements.swift" \
    "$SCRIPT_DIR/unit-check/main.swift" "$OUT/stub.o" -o "$OUT/unit-check"
"$OUT/unit-check"
