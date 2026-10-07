#!/usr/bin/env bash
# An Xcode too old for the phone's iOS is its own blocker (#126).
#
# On a phone running a newer iOS than the selected Xcode SDK (seen with an
# iOS 27.2 beta under Xcode 27.0), the runner builds, installs and launches,
# then testmanagerd refuses the IDE channel and it exits with code 74. Setup
# reported the generic `wda` blocker, which sent the operator chasing trust,
# automation and locks, and KeepAlive relaunched the runner on the phone every
# few seconds for nothing.
#
# The production decision functions are extracted the same way the other
# setup-wda fixtures do, with xcrun and devicectl stubbed. No phone, no Xcode
# build, no launchd.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-xcode-too-old.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"
trap 'rm -rf "$TMP_ROOT"' EXIT

pass_count=0
pass() { pass_count=$((pass_count + 1)); printf 'ok %d - %s\n' "$pass_count" "$1"; }
fail_test() { printf 'not ok - %s\n' "$1" >&2; exit 1; }

extract() {
    local fn
    for fn in "$@"; do
        awk -v name="$fn" '
            $0 ~ "^" name "\\(\\) \\{" { copying=1 }
            copying { print }
            copying && /^}/ { exit }
        ' "$SETUP"
    done
}
extract _valid_os_version _version_lt _ios_sdk_version _device_ios_version \
    _os_major_minor _runner_log_shows_ide_refusal _runner_failure_is_xcode_too_old \
    _report_xcode_too_old > "$TMP_ROOT/helpers.sh"
for fn in _device_ios_version _runner_failure_is_xcode_too_old _report_xcode_too_old; do
    grep -q "^$fn() {" "$TMP_ROOT/helpers.sh" || fail_test "could not extract $fn from setup-wda.sh"
done

# Stubs. SDK_VERSION and DEVICE_VERSION drive xcrun and devicectl; an empty
# DEVICE_VERSION makes devicectl fail without writing its JSON.
SDK_VERSION=""
DEVICE_VERSION=""
xcrun() { [ -n "$SDK_VERSION" ] && printf '%s\n' "$SDK_VERSION"; }
_devicectl_t() {
    local out="" previous=""
    for arg in "$@"; do
        [ "$previous" = "-j" ] && out="$arg"
        previous="$arg"
    done
    [ -n "$DEVICE_VERSION" ] || return 1
    # devicectl pretty-prints its JSON with one key per line.
    cat > "$out" <<JSON
{
  "result" : {
    "deviceProperties" : {
      "osBuildUpdate" : "24B5089g",
      "osVersionNumber" : "$DEVICE_VERSION"
    }
  }
}
JSON
}
STATUS_OUT="$TMP_ROOT/status.out"
_setstatus() { printf '%s|%s|%s\n' "$1" "$2" "$3" > "$STATUS_OUT"; }
die() { printf '%s\n' "$*" > "$TMP_ROOT/die.out"; exit 1; }
WDA_UDID="00008110-001C18203AD2401E"
RUN_LOG="$TMP_ROOT/wda-runner.log"
KEEPALIVE_FAILURE_KIND="generic"
XCODE_TOO_OLD_MESSAGE=""
# shellcheck source=/dev/null
. "$TMP_ROOT/helpers.sh"

REFUSED='iPhoneUse-Runner[1719] [DTXConnection] Connection peer refused channel request for "dtxproxy:XCTestDriverInterface:XCTestManager_IDEInterface"; channel canceled
Testing failed:
  iPhoneUse-Runner (1719) encountered an error (Early unexpected exit, operation never finished bootstrapping - no restart will be attempted. (Underlying Error: The test runner exited with code 74 before establishing connection.))'
PREPARING='Testing failed:
  iPhoneUse-Runner (1719) encountered an error (The test runner exited with code 74 while preparing to run tests.)'
AUTOMATION='The test runner failed to initialize for UI testing. (Underlying Error: Timed out while enabling automation mode.)'

verdict() {  # verdict <sdk> <device> <log text>
    SDK_VERSION="$1"; DEVICE_VERSION="$2"
    printf '%s\n' "$3" > "$RUN_LOG"
    if _runner_failure_is_xcode_too_old "$RUN_LOG"; then echo too_old; else echo other; fi
}

# 1. The reported case: a beta iOS one minor ahead of the SDK, refused channel.
SDK_VERSION=27.0; DEVICE_VERSION=27.2; printf '%s\n' "$REFUSED" > "$RUN_LOG"
_runner_failure_is_xcode_too_old "$RUN_LOG" || fail_test "iOS 27.2 under SDK 27.0 with code 74 was not xcode_too_old"
case "$XCODE_TOO_OLD_MESSAGE" in
    *"iOS 27.2"*"iOS 27.0"*"supports iOS 27.2"*) ;;
    *) fail_test "message does not name both versions: $XCODE_TOO_OLD_MESSAGE" ;;
esac
pass "a newer phone iOS plus the code-74 refusal is xcode_too_old, naming both versions"

# 2. The variant without the refused-channel line still counts.
[ "$(verdict 27.0 27.2 "$PREPARING")" = too_old ] \
    || fail_test "the 'code 74 while preparing to run tests' variant was missed"
pass "the code-74 variant without the refused-channel line is recognized"

# 3. A newer major is newer too.
[ "$(verdict 27.4 28.0 "$REFUSED")" = too_old ] || fail_test "iOS 28.0 under SDK 27.4 was not xcode_too_old"
pass "a newer major iOS is xcode_too_old"

# 4. Matching versions keep the existing classification.
[ "$(verdict 27.0 27.0 "$REFUSED")" = other ] || fail_test "equal versions with code 74 were called xcode_too_old"
[ "$(verdict 27.0 27.0.1 "$REFUSED")" = other ] || fail_test "a patch release ahead of the SDK was called xcode_too_old"
[ "$(verdict 27.2 27.0 "$REFUSED")" = other ] || fail_test "an older phone iOS was called xcode_too_old"
pass "equal, patch-only and older phone versions are not xcode_too_old"

# 5. The version gap alone never decides it: other failures stay what they are.
[ "$(verdict 27.0 27.2 "$AUTOMATION")" = other ] \
    || fail_test "an automation-mode failure on a newer phone was called xcode_too_old"
[ "$(verdict 27.0 27.2 'The test runner exited with code 740 before it started.')" = other ] \
    || fail_test "exit code 740 matched the code-74 signature"
pass "without the code-74 signature a newer phone is not xcode_too_old"

# 6. An unreadable version cannot prove anything.
[ "$(verdict 27.0 '' "$REFUSED")" = other ] || fail_test "a failed devicectl read produced xcode_too_old"
[ "$(verdict '' 27.2 "$REFUSED")" = other ] || fail_test "a missing SDK version produced xcode_too_old"
pass "an unreadable device or SDK version falls back to the existing classification"

# 7. Reporting publishes the blocker and selects the long KeepAlive backoff.
SDK_VERSION=27.0; DEVICE_VERSION=27.2; printf '%s\n' "$REFUSED" > "$RUN_LOG"
_runner_failure_is_xcode_too_old "$RUN_LOG"
( _report_xcode_too_old; echo "$KEEPALIVE_FAILURE_KIND" > "$TMP_ROOT/kind.out" ) || true
[ "$(cut -d'|' -f1-2 "$STATUS_OUT")" = "building-fail|xcode_too_old" ] \
    || fail_test "the report did not publish building-fail|xcode_too_old: $(cat "$STATUS_OUT")"
grep -q "cannot fix this" "$TMP_ROOT/die.out" || fail_test "the failure text does not say retrying cannot fix it"
awk '/^_report_xcode_too_old\(\)/,/^}/' "$SETUP" | grep -q 'KEEPALIVE_FAILURE_KIND="xcode_too_old"' \
    || fail_test "the report does not select the xcode_too_old KeepAlive backoff"
pass "the report publishes xcode_too_old and selects its KeepAlive backoff"

# 8. KeepAlive waits 15 minutes between attempts, not 5 s doubling to 5 min.
TEST_HOME="$TMP_ROOT/home"
STATE_DIR="$TEST_HOME/.iphone-use"
mkdir -p "$STATE_DIR" "$TEST_HOME/Library/LaunchAgents"
run_retry() {
    env HOME="$TEST_HOME" IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND="$1" \
        /bin/bash "$SETUP" doctor > "$TMP_ROOT/retry.out" 2>&1
}
retry_field() { sed -n "$1" "$STATE_DIR/wda-retry-state.v1"; }
assert_delay_near() {
    local delta=$(( $(retry_field '4s/^next_at=//p') - $(date +%s) ))
    [ "$delta" -ge $(($1 - 2)) ] && [ "$delta" -le $(($1 + 1)) ] \
        || fail_test "expected a ${1}s retry delay, found ${delta}s"
}
run_retry xcode_too_old || fail_test "the retry fixture rejected xcode_too_old: $(cat "$TMP_ROOT/retry.out")"
[ "$(retry_field '2s/^kind=//p')" = xcode_too_old ] || fail_test "the retry state did not keep the xcode_too_old kind"
assert_delay_near 900
run_retry xcode_too_old
[ "$(retry_field '3s/^attempt=//p')" = 2 ] || fail_test "a repeated xcode_too_old did not count attempts"
assert_delay_near 900
pass "KeepAlive waits 900 s between xcode_too_old attempts"

# 9. The status writer and the next pass both keep the blocker.
grep -q '"automation_mode_disabled", "xcode_too_old", "wda"}' "$SETUP" \
    || fail_test "the status writer drops xcode_too_old at the start of the next run"
grep -q 'trust|automation_mode_disabled|xcode_too_old) _BUILD_BLOCKER=' "$SETUP" \
    || fail_test "the next build pass does not keep xcode_too_old visible"
pass "the blocker stays visible across the next KeepAlive pass"

printf '%d checks passed\n' "$pass_count"
