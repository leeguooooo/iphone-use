#!/usr/bin/env bash
# The device-runner build path in setup-wda.sh, with every external tool
# stubbed: source discovery and validation, the source hash that keys the
# product cache, and the build/launch selection (cache hit, build, lock,
# signing failures). No iPhone, no xcodebuild, nothing under the real $HOME.
# shellcheck disable=SC1091,SC2034,SC2329
set -euo pipefail
umask 077
unset WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID IPU_RUNNER_SRC

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-runner-build-test.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

pass_count=0
pass() { pass_count=$((pass_count + 1)); printf 'ok %d - %s\n' "$pass_count" "$*"; }
fail_test() { printf 'not ok - %s\n' "$*" >&2; exit 1; }

extract() {  # extract <start-regex> <stop-regex> <out>
    awk -v start="$1" -v stop="$2" '
        $0 ~ start { copying=1 }
        copying && $0 ~ stop { exit }
        copying { print }
    ' "$SETUP" > "$3"
    [ -s "$3" ] || fail_test "could not isolate '$1' from setup-wda.sh"
}

extract '^# BEGIN runner source helpers\.' '^# END runner source helpers\.' "$TMP_ROOT/source.sh"
extract '^_runner_repo_src[(][)]' '^RUNNER_SRC=' "$TMP_ROOT/repo-src.sh"
extract '^WDA_XCTESTRUN=""$' '^# Keep `RUNNER_COMMAND=`' "$TMP_ROOT/launch.sh"

make_runner_tree() {  # make_runner_tree <dir>
    mkdir -p "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj" \
        "$1/IPhoneUseRunner/IPhoneUseRunnerUITests" \
        "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/xcuserdata/leo.xcuserdatad"
    printf '// !$*UTF8*$!\n{ }\n' > "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj"
    printf 'import XCTest\n' > "$1/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift"
    printf 'per-user\n' > "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/xcuserdata/leo.xcuserdatad/state"
    chmod -R go-w "$1"
}

# ── 1. runner source validation and hash ────────────────────────────────────
UID_NUM="$(id -u)"
. "$TMP_ROOT/source.sh"
RUNNER_DEFAULT_SRC="$TMP_ROOT/home/.iphone-use/runner"
set_src() { RUNNER_SRC="$1"; RUNNER_PROJECT="$RUNNER_SRC/IPhoneUseRunner/IPhoneUseRunner.xcodeproj"; }

set_src "$TMP_ROOT/src"
make_runner_tree "$RUNNER_SRC"
_runner_source_valid || fail_test "a valid runner tree was refused: $RUNNER_SOURCE_ERROR"
pass "a private runner source tree is accepted"

hash_a="$(_runner_source_hash)"
[ "${#hash_a}" = 64 ] || fail_test "source hash is not a sha256: '$hash_a'"
printf 'changed per-user state\n' > "$RUNNER_SRC/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/xcuserdata/leo.xcuserdatad/state"
touch "$RUNNER_SRC/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift"
[ "$(_runner_source_hash)" = "$hash_a" ] \
    || fail_test "the hash changed for Xcode per-user state or a bare mtime change"
printf 'import XCTest // edited\n' > "$RUNNER_SRC/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift"
[ "$(_runner_source_hash)" != "$hash_a" ] || fail_test "the hash missed a source edit"
pass "the source hash follows source content only"

set_src "$TMP_ROOT/missing"
if _runner_source_valid; then fail_test "missing sources were accepted"; fi
case "$RUNNER_SOURCE_ERROR" in
    *"Rerun the installer"*) ;;
    *) fail_test "missing-source error does not say how to fix it: $RUNNER_SOURCE_ERROR" ;;
esac
pass "missing sources fail with the reinstall hint"

set_src "relative/runner"
if _runner_source_valid; then fail_test "a relative source path was accepted"; fi
set_src "$TMP_ROOT/with space"
make_runner_tree "$RUNNER_SRC"
if _runner_source_valid; then fail_test "a source path with whitespace was accepted"; fi
pass "relative or whitespace source paths are refused"

set_src "$TMP_ROOT/shared"
make_runner_tree "$RUNNER_SRC"
chmod o+w "$RUNNER_SRC/IPhoneUseRunner"
if _runner_source_valid; then fail_test "a world-writable source tree was accepted"; fi
chmod o-w "$RUNNER_SRC/IPhoneUseRunner"
chmod g+w "$RUNNER_SRC"
if _runner_source_valid; then fail_test "a group-writable source tree was accepted"; fi
pass "sources other users can change are refused"

ln -s "$TMP_ROOT/src" "$TMP_ROOT/linked"
set_src "$TMP_ROOT/linked"
if _runner_source_valid; then fail_test "a symlinked source directory was accepted"; fi
pass "a symlinked source directory is refused"

# ── 2. the repo-adjacent source is used only from a repo checkout ───────────
. "$TMP_ROOT/repo-src.sh"
HOME="$TMP_ROOT/home"
mkdir -p "$TMP_ROOT/repo/scripts" "$TMP_ROOT/repo/crates/server" "$HOME/.iphone-use"
make_runner_tree "$TMP_ROOT/repo/runner"
# $0 drives the lookup; emulate both locations by running with an explicit $0.
repo_result="$(HOME="$HOME" bash -c '. "$1"; _runner_repo_src' "$TMP_ROOT/repo/scripts/setup-wda.sh" "$TMP_ROOT/repo-src.sh" || true)"
[ "$repo_result" = "$TMP_ROOT/repo/runner" ] \
    || fail_test "a repo checkout did not find its own runner/ (got '$repo_result')"
cp -R "$TMP_ROOT/repo/runner" "$HOME/runner"
mkdir -p "$HOME/crates/server"
installed_result="$(HOME="$HOME" bash -c '. "$1"; _runner_repo_src' "$HOME/.iphone-use/setup-wda.sh" "$TMP_ROOT/repo-src.sh" || true)"
[ -z "$installed_result" ] \
    || fail_test "the installed copy looked next to ~/.iphone-use for sources: '$installed_result'"
pass "only a repo checkout uses its adjacent runner/; the installed copy never does"

# ── 3. build/launch selection ───────────────────────────────────────────────
STATE_DIR="$TMP_ROOT/state"
mkdir -p "$STATE_DIR"
STATUS_LOG="$TMP_ROOT/status.log"
run_launch() {  # run_launch <scenario...>; prints the event trace
    (
        set -eu
        EVENTS=""
        event() { EVENTS="$EVENTS $*"; printf '%s\n' "$*" >> "$TMP_ROOT/events"; }
        ok() { event "ok:$1"; }
        warn() { event "warn:$1"; }
        die() { event "die:$1"; exit 3; }
        _setstatus() { printf '%s|%s|%s\n' "$1" "${2:-}" "${3:-}" >> "$STATUS_LOG"; }
        _runner_cache_read() { [ "$CACHE" = hit ] && { WDA_RUNNER_FROM_CACHE=1; WDA_XCTESTRUN=/x/cached.xctestrun; event cache-hit; }; }
        _runner_cache_write() { event cache-write; }
        _ensure_launchable_runner() {
            event build
            case "$BUILD" in
                ok) WDA_XCTESTRUN=/x/built.xctestrun; return 0 ;;
                locked) RUNNER_BUILD_LOCKED=1 ;;
                account) printf 'No Accounts: none\n' > "$STATE_DIR/wda-runner-product-build.log" ;;
                profile) printf 'error: No profiles for com.example were found\n' > "$STATE_DIR/wda-runner-product-build.log" ;;
                *) : ;;
            esac
            WDA_RUNNER_VALIDATION_ERROR="build-for-testing failed (log: x)"
            return 1
        }
        _prepare_locked_retry() { event locked-retry; }
        _report_missing_xcode_account() { event account-blocker; exit 4; }
        WDA_RUNNER_FROM_CACHE=0
        RUNNER_BUILD_LOCKED=0
        _BUILD_BLOCKER=""
        rm -f "$STATE_DIR/wda-runner-product-build.log"
        . "$TMP_ROOT/launch.sh"
        event "launch:$WDA_XCTESTRUN"
    ) || printf 'exit:%s\n' "$?" >> "$TMP_ROOT/events"
    cat "$TMP_ROOT/events"
    rm -f "$TMP_ROOT/events"
}

trace="$(CACHE=hit BUILD=ok WDA_RUNNER_REBUILD=0 run_launch)"
if printf '%s\n' "$trace" | grep -qx -e build -e cache-write; then
    fail_test "a cache hit rebuilt or re-recorded: $trace"
fi
case "$trace" in *"launch:/x/cached.xctestrun"*) ;; *) fail_test "a cache hit did not launch the cached product: $trace" ;; esac
pass "a matching recorded product launches without building"

trace="$(CACHE=hit BUILD=ok WDA_RUNNER_REBUILD=1 run_launch)"
case "$trace" in *cache-hit*) fail_test "WDA_RUNNER_REBUILD=1 still used the record: $trace" ;; esac
case "$trace" in *build*cache-write*"launch:/x/built.xctestrun"*) ;; *) fail_test "forced rebuild did not build, record and launch: $trace" ;; esac
pass "WDA_RUNNER_REBUILD=1 forces a build"

trace="$(CACHE=miss BUILD=ok run_launch)"
case "$trace" in *build*cache-write*"launch:/x/built.xctestrun"*) ;; *) fail_test "a cache miss did not build, record, then launch: $trace" ;; esac
pass "a cache miss builds, records the product before launch, and launches it"

trace="$(CACHE=miss BUILD=locked WDA_KEEPALIVE=1 run_launch)"
case "$trace" in *locked-retry*exit:1*) ;; *) fail_test "a locked build under KeepAlive did not take the lock backoff: $trace" ;; esac
case "$trace" in *launch:*) fail_test "a locked build launched anyway: $trace" ;; esac
trace="$(CACHE=miss BUILD=locked WDA_KEEPALIVE=0 run_launch)"
case "$trace" in *"die:the phone is locked"*) ;; *) fail_test "an interactive locked build did not say the phone is locked: $trace" ;; esac
pass "a lock screen during the build is a lock retry, not a build failure"

trace="$(CACHE=miss BUILD=account run_launch)"
case "$trace" in *account-blocker*) ;; *) fail_test "a signed-out Xcode was not reported as the account blocker: $trace" ;; esac
pass "a signed-out Xcode during the build reports the account blocker"

: > "$STATUS_LOG"
trace="$(CACHE=miss BUILD=profile run_launch)"
case "$trace" in *"could not find or create a development provisioning"*) ;; *) fail_test "the profile failure lost the phrase the daemon maps to 'account': $trace" ;; esac
grep -q '^signing-fail|account|' "$STATUS_LOG" || fail_test "the profile failure did not publish the account blocker"
pass "a provisioning failure keeps the daemon's account phrase and blocker"

: > "$STATUS_LOG"
trace="$(CACHE=miss BUILD=broken run_launch)"
case "$trace" in *"die:device runner product is not launchable"*) ;; *) fail_test "a broken build did not fail closed: $trace" ;; esac
grep -q '^building-fail|wda|' "$STATUS_LOG" || fail_test "a broken build did not publish the wda blocker"
pass "any other build failure fails closed with the wda blocker"

printf '1..%d\n' "$pass_count"
