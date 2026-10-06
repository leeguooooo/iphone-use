#!/usr/bin/env bash
# #75: KeepAlive rounds destroyed their own evidence and carried a poisoned
# runner into the next round. Deterministic checks on the guards that stop
# that; no iPhone, no xcodebuild.
# shellcheck disable=SC1091,SC2034,SC2329
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-repair-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

pass_count=0
pass() { pass_count=$((pass_count + 1)); printf 'ok %d - %s\n' "$pass_count" "$*"; }
fail_test() { printf 'not ok - %s\n' "$*" >&2; exit 1; }
info() { :; }; ok() { :; }; warn() { printf 'warn: %s\n' "$*"; }
_setstatus() { :; }

awk '
    /^_repair_runner_if_invalid\(\)/ { copying=1 }
    copying && /^_ensure_launchable_runner\(\)/ { exit }
    copying { print }
' "$SETUP" > "$TMP_ROOT/repair.sh"
[ -s "$TMP_ROOT/repair.sh" ] || fail_test "could not isolate the production repair helper"

# ── 1. repair rebuild keeps the failed build's log ──────────────────────────
source "$TMP_ROOT/repair.sh"
PRODUCTS="$TMP_ROOT/state/runner-build/Build/Products/Debug-iphoneos"
RUNNER_PRODUCTS_DIR="$PRODUCTS"
RUNNER_APP_NAME="iPhoneUse-Runner.app"
APP="$PRODUCTS/$RUNNER_APP_NAME"
mkdir -p "$APP"
LOG="$TMP_ROOT/wda-runner-product-build.log"
printf 'ProcessInfoPlistFile evidence from the first build\n' > "$LOG"

validate_calls=0
_validate_runner_bundle() {
    validate_calls=$((validate_calls + 1))
    if [ "$validate_calls" -eq 1 ]; then
        WDA_RUNNER_VALIDATION_ERROR="invalid Info.plist (plist or signature have been modified)"
        return 1
    fi
    WDA_RUNNER_VALIDATION_ERROR=""
}
prebuild_log_seen=""
_run_runner_prebuild() {
    prebuild_log_seen="$1"
    : > "$1"   # production truncates whatever path it is handed
    mkdir -p "$APP"
}
WDA_RUNNER_REPAIR_ATTEMPTED=0
_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG" \
    || fail_test "repair did not succeed after one rebuild"
[ "$prebuild_log_seen" = "${LOG%.log}.repair.log" ] \
    || fail_test "repair rebuild wrote to '$prebuild_log_seen', not a separate .repair.log"
grep -q 'evidence from the first build' "$LOG" \
    || fail_test "repair rebuild truncated the failed build's log"
pass "repair rebuild logs to *.repair.log and leaves the failed build's log intact"

# ── 2. repair only ever removes this instance's own runner app ──────────────
validate_calls=0
WDA_RUNNER_REPAIR_ATTEMPTED=0
mkdir -p "$TMP_ROOT/elsewhere/Build/Products/Debug-iphoneos/$RUNNER_APP_NAME"
if _repair_runner_if_invalid "$TMP_ROOT/elsewhere/Build/Products/Debug-iphoneos" \
    "$TMP_ROOT/elsewhere/Build/Products/Debug-iphoneos/$RUNNER_APP_NAME" "$LOG"; then
    fail_test "repair accepted another products directory"
fi
[ -d "$TMP_ROOT/elsewhere/Build/Products/Debug-iphoneos/$RUNNER_APP_NAME" ] \
    || fail_test "repair removed an app outside this instance's products"
pass "repair refuses a products dir that is not this instance's"

validate_calls=0
WDA_RUNNER_REPAIR_ATTEMPTED=0
mkdir -p "$TMP_ROOT/target.app"
ln -s "$TMP_ROOT/target.app" "$PRODUCTS/Link-Runner.app"
if _repair_runner_if_invalid "$PRODUCTS" "$PRODUCTS/Link-Runner.app" "$LOG"; then
    fail_test "repair followed a symlinked runner"
fi
[ -d "$TMP_ROOT/target.app" ] || fail_test "repair deleted through the link"
pass "repair refuses a symlinked or differently named runner"

# ── 3. the WebDriverAgent icon transaction is gone for good ─────────────────
for fn in _build_and_inject_runner_icon _discard_injected_runner \
    _discard_previous_injection _restore_wda_icon_app _runner_icon_fail; do
    if grep -q "$fn" "$SETUP"; then
        fail_test "$fn is still referenced; the device runner is never re-signed after Xcode signs it"
    fi
done
if grep -Eq 'codesign (-f|--force)' "$SETUP"; then
    fail_test "setup re-signs a built runner (that poisons installs with 0xe8008001)"
fi
pass "setup never re-signs the runner Xcode built"

# ── 4. main flow: record before launch, evict on product failure ─────────────
record_line="$(grep -n '^if \[ "\$WDA_RUNNER_FROM_CACHE" != "1" \]; then' "$SETUP" | cut -d: -f1)"
launch_line="$(grep -n '^RUNNER_COMMAND=' "$SETUP" | cut -d: -f1)"
[ -n "$record_line" ] && [ -n "$launch_line" ] && [ "$record_line" -lt "$launch_line" ] \
    || fail_test "the runner product is not recorded before its launch (a failed round rebuilds again)"
if grep -q 'WDA_RUNNER_FROM_CACHE" = "1" \] && _runner_log_shows_product_failure' "$SETUP"; then
    fail_test "a product failure only evicts cached products; a freshly recorded one would be reused"
fi
profile_line="$(grep -n 'requires a provisioning profile' "$SETUP" | tail -1 | cut -d: -f1)"
sed -n "${profile_line},$((profile_line + 8))p" "$SETUP" | grep -q '_runner_cache_drop' \
    || fail_test "a provisioning-profile failure leaves the recorded product for the next round"
pass "a verified product is recorded before launch and evicted on its own failure"

printf '1..%d\n' "$pass_count"
