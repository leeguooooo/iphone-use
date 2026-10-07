#!/usr/bin/env bash
# The reconnect hot path: what an idle-released phone pays before it is
# drivable again.
#
# A reconnect used to re-run every prerequisite (xcodebuild -version, a
# PlistBuddy sweep of every instance plist, three devicectl calls), then wait
# for the ServerURLHere line with a 3 s poll — while xcodebuild delivers that
# line through a block buffer, seconds after the runner is serving. These
# checks pin the pieces that took that time out, and the rollback fix that
# the faster reconnect exposed. No phone, no Xcode, no launchd.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-fast-reconnect.XXXXXX")"
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
extract _device_field _xcode_version_cached _instance_bindings_stamp \
    _instance_check_bindings_cached _wait_tcp_listening > "$TMP_ROOT/helpers.sh"
for fn in _device_field _xcode_version_cached _instance_check_bindings_cached _wait_tcp_listening; do
    grep -q "^$fn() {" "$TMP_ROOT/helpers.sh" || fail_test "could not extract $fn from setup-wda.sh"
done
# shellcheck source=/dev/null
. "$TMP_ROOT/helpers.sh"

# ── 1. Device facts come from `iphone-use device …` JSON ─────────────────────
DEVICE_QUERY_JSON='{"build_version":"24A435","connection":"usb","mounted":true,"name":"Leo, the phone","ok":true,"product_version":"27.0"}'
[ "$(_device_field product_version)" = 27.0 ] || fail_test "product_version was not read"
[ "$(_device_field connection)" = usb ] || fail_test "connection was not read"
[ "$(_device_field mounted)" = true ] || fail_test "a boolean field was not read"
[ "$(_device_field name)" = "Leo, the phone" ] || fail_test "a name with a comma was cut short"
[ -z "$(_device_field missing)" ] || fail_test "an absent field produced a value"
pass "device JSON fields (strings, booleans, commas) are read without python"

# ── 2. The Xcode version is cached per developer directory ───────────────────
STATE_DIR="$TMP_ROOT/state"
mkdir -p "$STATE_DIR" "$TMP_ROOT/Xcode.app/Contents/Developer"
VERSION_PLIST="$TMP_ROOT/Xcode.app/Contents/version.plist"
printf 'v1\n' > "$VERSION_PLIST"
xcode-select() { printf '%s\n' "$TMP_ROOT/Xcode.app/Contents/Developer"; }
XCODEBUILD_CALLS="$TMP_ROOT/xcodebuild.calls"
: > "$XCODEBUILD_CALLS"
fake_xcodebuild="$TMP_ROOT/xcodebuild"
printf '#!/bin/bash\necho call >> "%s"\nprintf "Xcode 27.0\\nBuild version 27A266a\\n"\n' "$XCODEBUILD_CALLS" > "$fake_xcodebuild"
chmod +x "$fake_xcodebuild"
[ "$(_xcode_version_cached "$fake_xcodebuild")" = "Xcode 27.0" ] || fail_test "first read did not return the version"
[ "$(_xcode_version_cached "$fake_xcodebuild")" = "Xcode 27.0" ] || fail_test "cached read did not return the version"
[ "$(wc -l < "$XCODEBUILD_CALLS" | tr -d ' ')" = 1 ] || fail_test "xcodebuild ran again on a cache hit"
# An Xcode update rewrites version.plist: the cache must not survive it.
touch -t 203001010000 "$VERSION_PLIST"
_xcode_version_cached "$fake_xcodebuild" >/dev/null
[ "$(wc -l < "$XCODEBUILD_CALLS" | tr -d ' ')" = 2 ] || fail_test "an updated Xcode reused the cached version"
# A corrupt cache entry is re-read, never echoed.
printf 'garbage\n' > "$STATE_DIR/.xcode-version.cache"
[ "$(_xcode_version_cached "$fake_xcodebuild")" = "Xcode 27.0" ] || fail_test "a corrupt cache leaked out"
pass "xcodebuild -version runs once per Xcode, and again after an update"

# ── 3. Instance bindings: a passing verdict is cached, a refusal is not ──────
HOME="$TMP_ROOT/home"
mkdir -p "$HOME/Library/LaunchAgents"
INSTANCE_LABEL_PREFIX="com.leeguoo.iphone-use"
INSTANCE_NAME="i13"
WDA_UDID="00008110-0002346211A0401E"
printf '<plist/>\n' > "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX.plist"
CHECKS="$TMP_ROOT/bindings.calls"
: > "$CHECKS"
BINDINGS_VERDICT=0
_instance_check_bindings() { echo check >> "$CHECKS"; return "$BINDINGS_VERDICT"; }
_instance_check_bindings_cached 8538 9538 || fail_test "a passing check was refused"
_instance_check_bindings_cached 8538 9538 || fail_test "a cached pass was refused"
[ "$(wc -l < "$CHECKS" | tr -d ' ')" = 1 ] || fail_test "a cached pass re-ran the PlistBuddy sweep"
_instance_check_bindings_cached 8540 9538 >/dev/null || true
[ "$(wc -l < "$CHECKS" | tr -d ' ')" = 2 ] || fail_test "different ports reused another verdict"
# Another instance appears: its plist is new, so the verdict must be re-checked.
printf '<plist/>\n' > "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX.guouli.plist"
BINDINGS_VERDICT=1
if _instance_check_bindings_cached 8538 9538 2>/dev/null; then
    fail_test "a new instance plist did not re-run the check"
fi
if _instance_check_bindings_cached 8538 9538 2>/dev/null; then
    fail_test "a refusal was cached as a pass"
fi
[ "$(wc -l < "$CHECKS" | tr -d ' ')" = 4 ] || fail_test "a refusal was cached"
pass "instance bindings are re-checked when any instance plist or port changes, refusals never cached"

# ── 4. Relay listening is polled, not slept for ───────────────────────────────
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
if _wait_tcp_listening "$port" 1; then
    fail_test "a closed port counted as listening"
fi
python3 -c 'import socket,sys,time; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1",int(sys.argv[1]))); s.listen(); time.sleep(5)' "$port" &
listener=$!
started=$(date +%s)
_wait_tcp_listening "$port" 3 || { kill "$listener" 2>/dev/null; fail_test "a listening port was not seen"; }
kill "$listener" 2>/dev/null || true
[ $(( $(date +%s) - started )) -le 2 ] || fail_test "waiting for a listening port took the old fixed sleep"
pass "relay listening is polled every 50 ms instead of a fixed 1 s sleep"

# ── 5. Readiness: the runner port is asked directly, a new session counts ────
wait_block="$(awk '/^info "Waiting for ServerURLHere/,/^done$/' "$SETUP")"
printf '%s\n' "$wait_block" | grep -q '_device_query runner-status' \
    || fail_test "the readiness wait does not ask the runner port over USB"
printf '%s\n' "$wait_block" | grep -q '"$_probe_session" != "$RUNNER_PREVIOUS_SESSION"' \
    || fail_test "readiness does not require a session different from the previous runner's"
printf '%s\n' "$wait_block" | grep -q 'sleep 3' \
    && fail_test "the readiness wait still polls every 3 s"
launch_block="$(awk '/^RUNNER_PROBE=0$/,/^RUNNER_COMMAND=/' "$SETUP")"
printf '%s\n' "$launch_block" | grep -q 'RUNNER_PREVIOUS_SESSION="$(_device_field session_id)"' \
    || fail_test "the session answering before launch is not recorded"
pass "readiness is read from the runner port every 0.2 s and must be a new session"

# ── 6. A stop during an untouched daemon configuration leaves the daemon up ──
cleanup_block="$(awk '/restoring the prior runner supervisor file/,/DAEMON_STAGED_PLIST:-/' "$SETUP")"
printf '%s\n' "$cleanup_block" \
    | grep -q '\[ "\$DAEMON_TOUCHED" != "1" \]' \
    || fail_test "the daemon rollback is not gated on the daemon having been changed"
configure_block="$(awk '/info "Configuring the iphone-use daemon/,/daemon LaunchAgent job loaded/' "$SETUP")"
touched_before_mv="$(printf '%s\n' "$configure_block" | awk '/DAEMON_TOUCHED=1/{t=1} /mv -f "\$DAEMON_STAGED_PLIST"/{print t+0; exit}')"
touched_before_bootout="$(printf '%s\n' "$configure_block" | awk '/DAEMON_TOUCHED=1/{t++} /launchctl bootout "\$GUI_DOMAIN\/\$DAEMON_LABEL"/{print t+0; exit}')"
[ "$touched_before_mv" = 1 ] || fail_test "installing a changed daemon plist does not mark the daemon touched first"
[ "${touched_before_bootout:-0}" -ge 2 ] || fail_test "reloading the daemon does not mark it touched first"
pass "a stop that lands while setup verifies an unchanged daemon no longer restarts it"

printf '%d checks passed\n' "$pass_count"
