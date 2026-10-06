#!/usr/bin/env bash
# install.sh's device-runner source transaction, in an isolated fake HOME:
# fresh install, upgrade, rollback on a later failure, and the release-archive
# checks (only runner/, no unsafe paths, no links). No network, no device.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/install.sh"
TMP_BASE="$(cd -P "${TMPDIR:-/tmp}" && pwd)"
TMP_ROOT="$(cd -P "$(mktemp -d "$TMP_BASE/iphone-use-runner-src-test.XXXXXX")" && pwd)"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

pass_count=0
pass() { pass_count=$((pass_count + 1)); printf 'ok %d - %s\n' "$pass_count" "$*"; }
fail_test() { printf 'not ok - %s\n' "$*" >&2; exit 1; }

new_home() {
    TEST_HOME="$TMP_ROOT/$1/home"
    mkdir -p "$TEST_HOME/.iphone-use"
    TEST_HOME="$(cd -P "$TEST_HOME" && pwd)"
    touch "$TEST_HOME/.iphone-use-installer-test-root"
}

make_tree() {  # make_tree <dir> <marker>
    mkdir -p "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/xcuserdata/me.xcuserdatad" \
        "$1/IPhoneUseRunner/IPhoneUseRunnerUITests"
    printf '// %s\n' "$2" > "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj"
    printf 'import XCTest // %s\n' "$2" > "$1/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerTests.swift"
    printf 'per-user\n' > "$1/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/xcuserdata/me.xcuserdatad/state"
    printf '# runner %s\n' "$2" > "$1/README.md"
}

run_hook() {  # run_hook <source-or-empty> <archive-or-empty> <fail 0|1>
    env HOME="$TEST_HOME" \
        IPHONE_USE_INTERNAL_TEST_RUNNER_ONLY=1 \
        IPHONE_USE_INTERNAL_TEST_RUNNER_SRC="${1:-}" \
        IPHONE_USE_INTERNAL_TEST_RUNNER_ARCHIVE="${2:-}" \
        IPHONE_USE_INTERNAL_TEST_RUNNER_FAIL="${3:-0}" \
        /bin/bash "$INSTALLER" >"$TEST_HOME/out" 2>&1
}

installed_marker() {
    sed -n 's#^// ##p' "$TEST_HOME/.iphone-use/runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj"
}

no_leftovers() {
    if find "$TEST_HOME/.iphone-use" -maxdepth 1 -name '.runner.*' | grep -q .; then
        fail_test "staging or backup directories were left behind: $(ls -A "$TEST_HOME/.iphone-use")"
    fi
}

# ── fresh install ────────────────────────────────────────────────────────────
new_home fresh
make_tree "$TEST_HOME/src-v1" v1
run_hook "$TEST_HOME/src-v1" "" 0 || fail_test "fresh install failed: $(cat "$TEST_HOME/out")"
[ "$(installed_marker)" = v1 ] || fail_test "fresh install did not lay down the sources"
[ -f "$TEST_HOME/.iphone-use/runner/README.md" ] || fail_test "the runner README was not copied"
if find "$TEST_HOME/.iphone-use/runner" -name xcuserdata | grep -q .; then
    fail_test "Xcode per-user state was copied into the shared sources"
fi
if find "$TEST_HOME/.iphone-use/runner" -perm -g+w -o -perm -o+w | grep -q .; then
    fail_test "installed sources are writable by group or others"
fi
no_leftovers
pass "a fresh install lays down private sources without Xcode per-user state"

# ── upgrade, then a failure after the swap ───────────────────────────────────
make_tree "$TEST_HOME/src-v2" v2
run_hook "$TEST_HOME/src-v2" "" 0 || fail_test "upgrade failed: $(cat "$TEST_HOME/out")"
[ "$(installed_marker)" = v2 ] || fail_test "upgrade did not replace the sources"
no_leftovers
pass "an upgrade replaces the sources and drops the backup on commit"

make_tree "$TEST_HOME/src-v3" v3
if run_hook "$TEST_HOME/src-v3" "" 1; then fail_test "the simulated failure did not fail"; fi
[ "$(installed_marker)" = v2 ] || fail_test "a failed install left the new sources in place"
no_leftovers
pass "a failure after the swap restores the previous sources"

mkdir -p "$TEST_HOME/not-runner"
if run_hook "$TEST_HOME/not-runner" "" 0; then fail_test "a tree without the project was accepted"; fi
[ "$(installed_marker)" = v2 ] || fail_test "a rejected source touched the installed sources"
pass "a source tree without the runner project is refused"

# ── release archive checks ───────────────────────────────────────────────────
make_archive() {  # make_archive <name> <setup-function>
    local work="$TEST_HOME/archive-$1"
    mkdir -p "$work"
    "$2" "$work"
    (cd "$work" && COPYFILE_DISABLE=1 /usr/bin/tar -czf "$TEST_HOME/$1.tar.gz" $(ls -A))
    printf '%s\n' "$TEST_HOME/$1.tar.gz"
}
good_layout() { make_tree "$1/runner" v4; }
extra_layout() { make_tree "$1/runner" v5; printf 'x\n' > "$1/evil.sh"; }
link_layout() { make_tree "$1/runner" v6; ln -s /etc/hosts "$1/runner/IPhoneUseRunner/hosts"; }

run_hook "" "$(make_archive good good_layout)" 0 || fail_test "a valid runner archive was refused: $(cat "$TEST_HOME/out")"
[ "$(installed_marker)" = v4 ] || fail_test "the archive's sources were not installed"
pass "a valid runner/ archive installs"

if run_hook "" "$(make_archive extra extra_layout)" 0; then fail_test "an archive with an extra top-level entry was accepted"; fi
grep -q "unexpected entry" "$TEST_HOME/out" || fail_test "extra-entry refusal reason missing"
if run_hook "" "$(make_archive link link_layout)" 0; then fail_test "an archive with a symlink was accepted"; fi
grep -q "contains a link" "$TEST_HOME/out" || fail_test "symlink refusal reason missing"
[ "$(installed_marker)" = v4 ] || fail_test "a refused archive touched the installed sources"
pass "archives with other top-level entries or links are refused"

printf '1..%d\n' "$pass_count"
