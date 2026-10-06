#!/bin/bash
# CI gate for the iphone-use device runner (release-binaries.yml, pr-checks.yml):
#   1. the device-independent logic passes (runner/unit-check.sh);
#   2. the UI-test bundle compiles for iOS, unsigned (CODE_SIGNING_ALLOWED=NO);
#   3. the names setup-wda.sh and uninstall.sh rely on match the project.
# Needs Xcode; never signs, installs or touches a device.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
fail() { printf 'runner ci-check: %s\n' "$*" >&2; exit 1; }

xcodebuild -version | sed -n 1p

# ── 3 first: cheap coherence checks ─────────────────────────────────────────
PROJECT=runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj
[ -f "$PROJECT/project.pbxproj" ] || fail "missing $PROJECT"
[ -f "$PROJECT/xcshareddata/xcschemes/IPhoneUseRunner.xcscheme" ] \
    || fail "the shared IPhoneUseRunner scheme setup-wda.sh builds is missing"
grep -q '^RUNNER_SCHEME="IPhoneUseRunner"$' scripts/setup-wda.sh \
    || fail "setup-wda.sh does not build the IPhoneUseRunner scheme"
grep -q '^RUNNER_TEST_ID="IPhoneUseRunnerUITests/RunnerTests/testServe"$' scripts/setup-wda.sh \
    || fail "setup-wda.sh does not launch IPhoneUseRunnerUITests/RunnerTests/testServe"
grep -q 'func testServe()' runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift \
    || fail "RunnerTests.testServe, the method setup-wda.sh launches, is missing"
grep -q '^RUNNER_APP_NAME="iPhoneUse-Runner.app"$' scripts/setup-wda.sh \
    || fail "setup-wda.sh expects a different runner app name"
grep -q 'PRODUCT_NAME: iPhoneUse$' runner/IPhoneUseRunner/project.yml \
    || fail "project.yml no longer names the runner product iPhoneUse"
grep -q 'static let defaultPort: UInt16 = 8100' runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift \
    && grep -q 'static let defaultMJPEGPort: UInt16 = 9100' runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift \
    || fail "the runner no longer listens on the device ports the relays forward (8100/9100)"
grep -q 'ServerURLHere->' runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerHTTP.swift \
    || fail "the runner no longer prints the ServerURLHere marker setup-wda.sh waits for"
grep -q 'IPhoneUseRunner_\[\^ /\]+\\\.xctestrun -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe' uninstall.sh \
    || fail "uninstall.sh does not recognise the runner process setup-wda.sh starts"
echo "runner ci-check: names coherent across project, setup-wda.sh and uninstall.sh"

# ── 1. device-independent logic ─────────────────────────────────────────────
bash runner/unit-check.sh

# ── 2. unsigned iOS build ───────────────────────────────────────────────────
DERIVED="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/iphone-use-runner-ci.$$"
trap 'rm -rf "$DERIVED"' EXIT
IPU_RUNNER_DERIVED_DATA="$DERIVED/DerivedData" bash runner/build.sh --unsigned \
    || { tail -50 "$DERIVED/build-unsigned.log" 2>/dev/null || true; fail "unsigned build failed"; }
[ -d "$DERIVED/DerivedData/Build/Products/Debug-iphoneos/iPhoneUse-Runner.app/PlugIns/iPhoneUse.xctest" ] \
    || fail "the build did not produce iPhoneUse-Runner.app with its test bundle"
ls "$DERIVED"/DerivedData/Build/Products/IPhoneUseRunner_iphoneos*.xctestrun >/dev/null 2>&1 \
    || fail "the build did not produce the IPhoneUseRunner .xctestrun setup-wda.sh launches"
echo "runner ci-check: OK"
