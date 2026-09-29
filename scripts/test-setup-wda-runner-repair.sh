#!/usr/bin/env bash
# #75: KeepAlive rounds destroyed their own evidence and carried a poisoned
# runner into the next round. Deterministic checks on the two guards that
# stop that; no iPhone, no xcodebuild.
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

# Same extraction the icon test uses: production helpers, no command body.
awk '
    /^_cleanup_wda_icon_work_dir\(\)/ { copying=1 }
    copying && /^if \[ "\$COMMAND" = "setup" \]; then/ { exit }
    copying { print }
' "$SETUP" > "$TMP_ROOT/transaction.sh"
awk '
    /^_repair_runner_if_invalid\(\)/ { copying=1 }
    copying && /^_ensure_launchable_runner\(\)/ { exit }
    copying { print }
' "$SETUP" > "$TMP_ROOT/repair.sh"
[ -s "$TMP_ROOT/transaction.sh" ] && [ -s "$TMP_ROOT/repair.sh" ] \
    || fail_test "could not isolate the production helpers"
grep -q '^_discard_injected_runner()' "$TMP_ROOT/transaction.sh" \
    || fail_test "_discard_injected_runner is outside the extracted transaction block"
grep -q '^_discard_previous_injection()' "$TMP_ROOT/transaction.sh" \
    || fail_test "_discard_previous_injection is outside the extracted transaction block"
# The selection block calls the discard helper; it must also be defined
# (dc6dd28 shipped the call without the function after a revert).
for fn in _discard_injected_runner _discard_previous_injection; do
    grep -q "^$fn()" "$SETUP" || fail_test "$fn is called but not defined in setup-wda.sh"
done

# ── 1. repair rebuild keeps the failed build's log ──────────────────────────
source "$TMP_ROOT/repair.sh"
PRODUCTS="$TMP_ROOT/DerivedData/Build/Products/Debug-iphoneos"
APP="$PRODUCTS/WebDriverAgentRunner-Runner.app"
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

# ── 2. discarding an injected runner is guarded ─────────────────────────────
source "$TMP_ROOT/transaction.sh"
mkdir -p "$APP"; touch "$APP/Info.plist"
WDA_ICON_PRODUCTS_DIR="$PRODUCTS"; WDA_ICON_APP_PATH="$APP"

WDA_RUNNER_ICON_INJECTED=0
_discard_injected_runner || fail_test "not-injected discard must be a successful no-op"
[ -d "$APP" ] || fail_test "not-injected discard removed the runner"
pass "discard is a no-op when nothing was injected"

WDA_RUNNER_ICON_INJECTED=1
WDA_ICON_PRODUCTS_DIR="$TMP_ROOT/elsewhere"
if _discard_injected_runner; then fail_test "discard accepted a products dir outside Build/Products"; fi
[ -d "$APP" ] || fail_test "out-of-tree discard removed the runner anyway"
pass "discard refuses a products dir outside Build/Products"

WDA_ICON_PRODUCTS_DIR="$PRODUCTS"
WDA_ICON_APP_PATH="$PRODUCTS/NotARunner.app"
if _discard_injected_runner; then fail_test "discard accepted an app not named *-Runner.app"; fi
pass "discard refuses an app that is not the runner product"

ln -s "$TMP_ROOT" "$PRODUCTS/Link-Runner.app"
WDA_ICON_APP_PATH="$PRODUCTS/Link-Runner.app"
if _discard_injected_runner; then fail_test "discard followed a symlinked runner"; fi
[ -d "$TMP_ROOT/DerivedData" ] || fail_test "symlinked discard deleted through the link"
pass "discard refuses a symlinked runner"

WDA_ICON_APP_PATH="$APP"; WDA_RUNNER_ICON_INJECTED=1
_discard_injected_runner || fail_test "discard refused a valid injected runner"
[ ! -e "$APP" ] || fail_test "discard left the injected runner in place"
[ "$WDA_RUNNER_ICON_INJECTED" = "0" ] || fail_test "discard left the injected flag set"
pass "discard removes a valid injected runner and clears the injected flag"

# ── 3. the prebuild starts from a pristine baseline ─────────────────────────
mkdir -p "$APP"; touch "$APP/Info.plist"
WDA_RUNNER_ICON_INJECTED=0
_discard_previous_injection "$PRODUCTS" "$APP" \
    || fail_test "a never-injected product must be left for the incremental build"
[ -d "$APP" ] || fail_test "a never-injected product was removed"
pass "prebuild keeps a product that was never injected"

touch "$APP/AppIcon60x60@2x.png"
_discard_previous_injection "$PRODUCTS" "$APP" \
    || fail_test "the previous round's injected runner was not discarded"
[ ! -e "$APP" ] || fail_test "prebuild would have built over the previous round's injected runner"
[ "$WDA_RUNNER_ICON_INJECTED" = "0" ] || fail_test "discarding the previous injection left the flag set"
pass "prebuild drops the previous round's injected runner first"

rm -f "$PRODUCTS/Link-Runner.app"
mkdir -p "$TMP_ROOT/target.app"; touch "$TMP_ROOT/target.app/AppIcon60x60@2x.png"
ln -s "$TMP_ROOT/target.app" "$PRODUCTS/Link-Runner.app"
if _discard_previous_injection "$PRODUCTS" "$PRODUCTS/Link-Runner.app"; then
    fail_test "prebuild baseline followed a symlinked runner"
fi
[ -e "$TMP_ROOT/target.app/AppIcon60x60@2x.png" ] || fail_test "baseline discard deleted through the link"
pass "prebuild baseline refuses a symlinked runner"

# ── 4. main flow: record before launch, evict on product failure ─────────────
record_line="$(grep -n '^if \[ -n "\${WDA_XCTESTRUN:-}" \] && \[ "\$WDA_RUNNER_FROM_CACHE" != "1" \]; then' "$SETUP" | cut -d: -f1)"
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
