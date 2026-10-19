#!/usr/bin/env bash
# Focused macOS mocks for uninstall PID identity and runner checkout ownership.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UNINSTALL="$ROOT/uninstall.sh"
TMP_ROOT_RAW="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-uninstall-test.XXXXXX")"
TMP_ROOT="$(cd -P "$TMP_ROOT_RAW" && pwd)"
SLEEP_PIDS=""

cleanup() {
    local pid
    for pid in $SLEEP_PIDS; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT INT TERM

pass_count=0
pass() {
    pass_count=$((pass_count + 1))
    printf 'ok %d - %s\n' "$pass_count" "$1"
}
fail_test() {
    printf 'not ok - %s\n' "$1" >&2
    exit 1
}

new_home() {
    local name="$1"
    TEST_ROOT="$TMP_ROOT/$name"
    TEST_HOME="$TEST_ROOT/home"
    TEST_BIN="$TEST_ROOT/bin"
    mkdir -p "$TEST_HOME/Library/LaunchAgents" "$TEST_HOME/.iphone-use" "$TEST_BIN"
    chmod 700 "$TEST_HOME/Library/LaunchAgents" "$TEST_HOME/.iphone-use"
    cat > "$TEST_BIN/launchctl" <<'SH'
#!/bin/sh
case "${1:-}" in
    print) exit 1 ;;
    bootout|disable|enable|bootstrap|kickstart) exit 0 ;;
    *) exit 0 ;;
esac
SH
    chmod 700 "$TEST_BIN/launchctl"
}

run_uninstall() {
    env \
        HOME="$TEST_HOME" \
        IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
        /bin/bash "$UNINSTALL" "$@"
}

git_init_checkout() {
    IPU_RUNNER_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
    mkdir -p "$IPU_RUNNER_DIR"
    git -C "$IPU_RUNNER_DIR" init -q
    git -C "$IPU_RUNNER_DIR" config user.name "Safety Test"
    git -C "$IPU_RUNNER_DIR" config user.email "safety@example.invalid"
    git -C "$IPU_RUNNER_DIR" remote add origin https://github.com/appium/WebDriverAgent.git
    printf 'fixture\n' > "$IPU_RUNNER_DIR/fixture.txt"
    git -C "$IPU_RUNNER_DIR" add fixture.txt
    git -C "$IPU_RUNNER_DIR" commit -qm fixture
    IPU_RUNNER_HEAD="$(git -C "$IPU_RUNNER_DIR" rev-parse HEAD)"
    IPU_RUNNER_UDID="00008110-001234567890001E"
}

digest_text() {
    printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
}

write_marker() {
    local refs reflog worktrees objects marker
    refs="$(GIT_OPTIONAL_LOCKS=0 git -C "$IPU_RUNNER_DIR" \
        for-each-ref --format='%(refname) %(objectname)')"
    reflog="$(GIT_OPTIONAL_LOCKS=0 git -C "$IPU_RUNNER_DIR" \
        reflog show --all --format='%H %gD %gs')"
    worktrees="$(GIT_OPTIONAL_LOCKS=0 git -C "$IPU_RUNNER_DIR" \
        worktree list --porcelain)"
    objects="$(GIT_OPTIONAL_LOCKS=0 git -C "$IPU_RUNNER_DIR" \
        cat-file --batch-all-objects --batch-check='%(objectname)')"
    objects="$(printf '%s\n' "$objects" | LC_ALL=C sort)"
    marker="$TEST_HOME/.iphone-use/runner-checkout-owner.v1"
    printf '%s\n' \
        "version=1" \
        "path=$IPU_RUNNER_DIR" \
        "origin=https://github.com/appium/WebDriverAgent.git" \
        "head=$IPU_RUNNER_HEAD" \
        "udid=$IPU_RUNNER_UDID" \
        "refs_sha256=$(digest_text "$refs")" \
        "reflog_sha256=$(digest_text "$reflog")" \
        "worktrees_sha256=$(digest_text "$worktrees")" \
        "objects_sha256=$(digest_text "$objects")" > "$marker"
    chmod 600 "$marker"
}

write_runner_plist() {
    local plist="$TEST_HOME/Library/LaunchAgents/com.leeguoo.iphone-use.runner.plist"
    cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.leeguoo.iphone-use.runner</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$TEST_HOME/.iphone-use/setup-runner.sh</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>IPU_RUNNER_DIR</key><string>$IPU_RUNNER_DIR</string>
    <key>IPU_RUNNER_REF</key><string>$IPU_RUNNER_HEAD</string>
    <key>IPU_RUNNER_UDID</key><string>$IPU_RUNNER_UDID</string>
    <key>IPU_RUNNER_BUNDLE_ID</key><string>com.example.iphoneuse.runner</string>
  </dict>
</dict></plist>
PLIST
    chmod 600 "$plist"
}

expect_preserved_checkout() {
    if run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
        fail_test "unsafe checkout case unexpectedly returned success"
    fi
    [ -d "$IPU_RUNNER_DIR/.git" ] || fail_test "unsafe checkout was deleted"
}

new_home "empty-idempotent"
run_uninstall >/dev/null
run_uninstall >/dev/null
pass "empty uninstall is idempotent"

new_home "arbitrary-pid"
/bin/sleep 60 &
victim_pid=$!
SLEEP_PIDS="$SLEEP_PIDS $victim_pid"
victim_lstart="$(LC_ALL=C /bin/ps -p "$victim_pid" -o lstart= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
victim_command="$(LC_ALL=C /bin/ps -ww -p "$victim_pid" -o command= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
printf '%s|%s|runner:%s\n' \
    "$victim_pid" "$victim_lstart" "$victim_command" \
    > "$TEST_HOME/.iphone-use/runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/runner.pid"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "arbitrary PID record unexpectedly passed dry-run"
fi
kill -0 "$victim_pid" 2>/dev/null || fail_test "arbitrary process was signalled"
grep -q "non-product runner command contract" "$TEST_ROOT/err" \
    || fail_test "arbitrary PID rejection reason missing"
pass "arbitrary same-uid command is never accepted as a runner"

new_home "valid-runner-contract"
IPU_RUNNER_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
mkdir -p "$IPU_RUNNER_DIR"
runner_command="/usr/bin/xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=00008110-001234567890001E -allowProvisioningUpdates DEVELOPMENT_TEAM=ABCD123456 PRODUCT_BUNDLE_IDENTIFIER=com.example.iphoneuse.runner test"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4242\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:00 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$runner_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4242\\nn%s\\n' "$IPU_RUNNER_DIR"
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4242|Mon Jul 28 00:00:00 2026|runner:$runner_command" \
    > "$TEST_HOME/.iphone-use/runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/runner.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "unmarked checkout should still make the aggregate dry-run fail"
fi
grep -q "send SIGTERM to PID-verified runner process 4242" "$TEST_ROOT/out" \
    || fail_test "valid runner contract did not reach the dry-run signal plan"
pass "valid runner command plus canonical cwd reaches the signal plan"

new_home "wrong-runner-cwd"
IPU_RUNNER_DIR="$TEST_HOME/.iphone-use/WebDriverAgent"
mkdir -p "$IPU_RUNNER_DIR" "$TEST_ROOT/not-runner"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4242\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:00 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$runner_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4242\\nn%s\\n' "$TEST_ROOT/not-runner"
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4242|Mon Jul 28 00:00:00 2026|runner:$runner_command" \
    > "$TEST_HOME/.iphone-use/runner.pid"
chmod 600 "$TEST_HOME/.iphone-use/runner.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "wrong runner cwd unexpectedly passed"
fi
grep -q "not a runner rooted in its expected directory" "$TEST_ROOT/err" \
    || fail_test "wrong runner cwd rejection reason missing"
! grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "wrong-cwd runner reached a signal plan"
pass "runner-shaped xcodebuild outside the configured checkout is rejected"

new_home "relay-listener-owner"
relay_command="/usr/local/bin/iproxy -s 127.0.0.1 8100:8100 -u 00008110-001234567890001E"
cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4243\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:01 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$relay_command" ;;
  *) exit 1 ;;
esac
SH
cat > "$TEST_BIN/lsof" <<'SH'
#!/bin/sh
printf 'p9999\n'
SH
chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
printf '%s\n' "4243|Mon Jul 28 00:00:01 2026|relay:$relay_command" \
    > "$TEST_HOME/.iphone-use/runner-relay.pid"
chmod 600 "$TEST_HOME/.iphone-use/runner-relay.pid"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "relay with a foreign listener owner unexpectedly passed"
fi
grep -q "does not exclusively own 127.0.0.1:8100" "$TEST_ROOT/err" \
    || fail_test "relay listener ownership rejection reason missing"
! grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "foreign-listener relay reached a signal plan"
pass "runner relay must exclusively own its exact loopback listener"

# The built-in usbmux relay (`iphone-use relay`) is held to the same rules,
# with its listen port read from --listen.
builtin_relay_case() {  # builtin_relay_case <home-name> <lsof-pid>
    new_home "$1"
    relay_command="/Users/tester/Applications/iPhoneUse.app/Contents/MacOS/iphone-use relay --udid 00008110-001234567890001E --listen 127.0.0.1:8100 --device-port 8100"
    cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4243\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:01 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$relay_command" ;;
  *) exit 1 ;;
esac
SH
    printf '#!/bin/sh\nprintf '"'"'p%s\\nn127.0.0.1:8100\\n'"'"'\n' "$2" > "$TEST_BIN/lsof"
    chmod 700 "$TEST_BIN/ps" "$TEST_BIN/lsof"
    printf '%s\n' "4243|Mon Jul 28 00:00:01 2026|relay:$relay_command" \
        > "$TEST_HOME/.iphone-use/runner-relay.pid"
    chmod 600 "$TEST_HOME/.iphone-use/runner-relay.pid"
    env HOME="$TEST_HOME" \
        IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
        IPHONE_USE_PS="$TEST_BIN/ps" \
        IPHONE_USE_LSOF="$TEST_BIN/lsof" \
        /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"
}
if builtin_relay_case "builtin-relay-foreign" 9999; then
    fail_test "built-in relay with a foreign listener owner unexpectedly passed"
fi
grep -q "does not exclusively own 127.0.0.1:8100" "$TEST_ROOT/err" \
    || fail_test "built-in relay listener ownership rejection reason missing"
! grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "foreign-listener built-in relay reached a signal plan"
builtin_relay_case "builtin-relay-owned" 4243 || true
grep -q "send SIGTERM" "$TEST_ROOT/out" \
    || fail_test "an owned built-in relay was not planned for SIGTERM: $(cat "$TEST_ROOT/err")"
pass "built-in usbmux relay is recognized and must own its loopback listener"

new_home "pid-mode"
/bin/sleep 60 &
victim_pid=$!
SLEEP_PIDS="$SLEEP_PIDS $victim_pid"
victim_lstart="$(LC_ALL=C /bin/ps -p "$victim_pid" -o lstart= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
victim_command="$(LC_ALL=C /bin/ps -ww -p "$victim_pid" -o command= \
    | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
printf '%s|%s|runner:%s\n' \
    "$victim_pid" "$victim_lstart" "$victim_command" \
    > "$TEST_HOME/.iphone-use/runner.pid"
chmod 644 "$TEST_HOME/.iphone-use/runner.pid"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "world-readable PID record unexpectedly passed dry-run"
fi
kill -0 "$victim_pid" 2>/dev/null || fail_test "process referenced by insecure PID file was signalled"
grep -q "mode 600" "$TEST_ROOT/err" || fail_test "PID mode rejection reason missing"
pass "PID record must be current-uid mode 600"

new_home "state-mode"
chmod 755 "$TEST_HOME/.iphone-use"
if run_uninstall --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "insecure state namespace unexpectedly passed dry-run"
fi
grep -q "expected 700" "$TEST_ROOT/err" || fail_test "state mode rejection reason missing"
pass "state namespace must be current-uid mode 700"

new_home "transient-symlink"
printf 'keep\n' > "$TEST_ROOT/outside"
ln -s "$TEST_ROOT/outside" \
    "$TEST_HOME/.iphone-use/runner-checkout-owner.v1.new.attacker"
if run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "known-name transient symlink unexpectedly returned success"
fi
[ -L "$TEST_HOME/.iphone-use/runner-checkout-owner.v1.new.attacker" ] \
    || fail_test "transient symlink itself was removed"
[ "$(sed -n '1p' "$TEST_ROOT/outside")" = "keep" ] \
    || fail_test "transient symlink target was changed"
grep -q "unrecognized or unsafe entries" "$TEST_ROOT/err" \
    || fail_test "unsafe known-name entry rejection reason missing"
pass "known-name transient symlinks are preserved and reported as incomplete"

new_home "unmarked-checkout"
git_init_checkout
expect_preserved_checkout
grep -q "ownership marker is missing" "$TEST_ROOT/err" \
    || fail_test "unmarked checkout rejection reason missing"
pass "legacy/unmarked checkout is preserved"

new_home "marked-checkout"
git_init_checkout
write_marker
write_runner_plist
run_uninstall --dry-run >"$TEST_ROOT/dry-out" 2>"$TEST_ROOT/dry-err" \
    || fail_test "fully marked pristine checkout failed dry-run"
[ -d "$IPU_RUNNER_DIR/.git" ] || fail_test "dry-run deleted the marked checkout"
[ -f "$TEST_HOME/.iphone-use/runner-checkout-owner.v1" ] \
    || fail_test "dry-run deleted the ownership marker"
run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "fully marked pristine checkout did not uninstall"
[ ! -e "$IPU_RUNNER_DIR" ] || fail_test "marked pristine checkout remains"
[ ! -e "$TEST_HOME/.iphone-use" ] || fail_test "empty state directory remains"
run_uninstall >/dev/null
pass "marked checkout dry-run is non-mutating and actual uninstall is idempotent"

new_home "local-commit"
git_init_checkout
write_marker
write_runner_plist
git -C "$IPU_RUNNER_DIR" switch -qc local-safety
printf 'local commit\n' >> "$IPU_RUNNER_DIR/fixture.txt"
git -C "$IPU_RUNNER_DIR" add fixture.txt
git -C "$IPU_RUNNER_DIR" commit -qm local-only
git -C "$IPU_RUNNER_DIR" checkout -q --detach "$IPU_RUNNER_HEAD"
expect_preserved_checkout
grep -q "Git refs changed" "$TEST_ROOT/err" || fail_test "local-ref rejection reason missing"
pass "local unpushed commits/branches after marker are preserved"

new_home "stash"
git_init_checkout
write_marker
write_runner_plist
printf 'stashed change\n' >> "$IPU_RUNNER_DIR/fixture.txt"
git -C "$IPU_RUNNER_DIR" stash push -qm local-safety
expect_preserved_checkout
grep -q "Git refs changed" "$TEST_ROOT/err" || fail_test "stash rejection reason missing"
pass "Git stashes after marker are preserved"

new_home "reflog"
git_init_checkout
write_marker
write_runner_plist
git -C "$IPU_RUNNER_DIR" update-ref -m local-safety HEAD "$IPU_RUNNER_HEAD" "$IPU_RUNNER_HEAD"
expect_preserved_checkout
grep -q "Git reflog changed" "$TEST_ROOT/err" || fail_test "reflog rejection reason missing"
pass "reflog-only history after marker is preserved"

new_home "unreferenced-object"
git_init_checkout
write_marker
write_runner_plist
printf 'unpublished object\n' | git -C "$IPU_RUNNER_DIR" hash-object -w --stdin >/dev/null
expect_preserved_checkout
grep -q "object database changed" "$TEST_ROOT/err" \
    || fail_test "unreferenced-object rejection reason missing"
pass "unreferenced unpublished Git objects after marker are preserved"

new_home "ignored-data"
git_init_checkout
printf 'ignored.dat\n' > "$IPU_RUNNER_DIR/.gitignore"
git -C "$IPU_RUNNER_DIR" add .gitignore
git -C "$IPU_RUNNER_DIR" commit -qm ignore-rule
IPU_RUNNER_HEAD="$(git -C "$IPU_RUNNER_DIR" rev-parse HEAD)"
write_marker
write_runner_plist
printf 'private fixture\n' > "$IPU_RUNNER_DIR/ignored.dat"
expect_preserved_checkout
grep -q "ignored" "$TEST_ROOT/err" || fail_test "ignored-data rejection reason missing"
pass "ignored checkout data is preserved"

new_home "linked-worktree"
git_init_checkout
write_marker
write_runner_plist
git -C "$IPU_RUNNER_DIR" worktree add -q -b linked-safety "$TEST_ROOT/linked" "$IPU_RUNNER_HEAD"
expect_preserved_checkout
grep -Eq "Git refs changed|worktree" "$TEST_ROOT/err" \
    || fail_test "linked-worktree rejection reason missing"
pass "linked worktrees and their Git administration data are preserved"


# ── device runner (replaces WebDriverAgent) ────────────────────────────────
device_runner_fixture() {
    new_home "$1"
    device_runner_command="/Applications/Xcode.app/Contents/Developer/usr/bin/xcodebuild -destination platform=iOS,id=00008110-001234567890001E test-without-building -xctestrun $TEST_HOME/.iphone-use/runner-build/Build/Products/IPhoneUseRunner_iphoneos27.0-arm64.xctestrun -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe"
    cat > "$TEST_BIN/ps" <<SH
#!/bin/sh
case " \$* " in
  *" -o pid= "*) printf '4343\\n' ;;
  *" -o uid= "*) printf '%s\\n' "$(id -u)" ;;
  *" -o lstart= "*) printf 'Mon Jul 28 00:00:00 2026\\n' ;;
  *" -o command= "*) printf '%s\\n' "$device_runner_command" ;;
  *) exit 1 ;;
esac
SH
    chmod 700 "$TEST_BIN/ps"
    printf '%s\n' "4343|Mon Jul 28 00:00:00 2026|runner:$device_runner_command" \
        > "$TEST_HOME/.iphone-use/runner.pid"
    chmod 600 "$TEST_HOME/.iphone-use/runner.pid"
}

device_runner_fixture "device-runner-contract"
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4343\\nn%s\\n' "$TEST_HOME/.iphone-use"
SH
chmod 700 "$TEST_BIN/lsof"
env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" || true
grep -q "send SIGTERM to PID-verified runner process 4343" "$TEST_ROOT/out" \
    || fail_test "the device runner started from the state directory did not reach the signal plan"
pass "device runner command plus state-directory cwd reaches the signal plan"

device_runner_fixture "device-runner-wrong-cwd"
mkdir -p "$TEST_ROOT/elsewhere"
cat > "$TEST_BIN/lsof" <<SH
#!/bin/sh
printf 'p4343\\nn%s\\n' "$TEST_ROOT/elsewhere"
SH
chmod 700 "$TEST_BIN/lsof"
if env HOME="$TEST_HOME" \
    IPHONE_USE_LAUNCHCTL="$TEST_BIN/launchctl" \
    IPHONE_USE_PS="$TEST_BIN/ps" \
    IPHONE_USE_LSOF="$TEST_BIN/lsof" \
    /bin/bash "$UNINSTALL" --dry-run >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "a device-runner-shaped xcodebuild outside the state directory was accepted"
fi
grep -q "send SIGTERM" "$TEST_ROOT/out" && fail_test "a foreign cwd reached the signal plan"
pass "device-runner-shaped xcodebuild outside the state directory is rejected"

runner_state_fixture() {
    new_home "$1"
    local state="$TEST_HOME/.iphone-use"
    mkdir -p "$state/runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj" \
        "$state/runner-build/Build/Products/Debug-iphoneos/iPhoneUse-Runner.app"
    printf '{}\n' > "$state/runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj"
    for name in runner-product.json runner-product-build.log \
        runner-product-build.repair.log runner-retry-state.v1 \
        runner-xcode-compat.xcconfig runner-setup-status.json runner-setup-status.json.lock; do
        printf 'x\n' > "$state/$name"
        chmod 600 "$state/$name"
    done
}

runner_state_fixture "device-runner-state"
run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err" \
    || fail_test "uninstall with device-runner state failed: $(cat "$TEST_ROOT/err")"
[ ! -e "$TEST_HOME/.iphone-use" ] \
    || fail_test "device-runner state was left behind: $(ls -A "$TEST_HOME/.iphone-use")"
pass "uninstall removes runner sources, build products and runner state files"

runner_state_fixture "device-runner-shared"
mkdir -m 700 -p "$TEST_HOME/.iphone-use/instances/lab"
if run_uninstall >"$TEST_ROOT/out" 2>"$TEST_ROOT/err"; then
    fail_test "the default instance uninstalled while a named instance remained"
fi
[ -f "$TEST_HOME/.iphone-use/runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj" ] \
    || fail_test "runner sources were removed while a named instance still builds from them"
pass "shared runner sources survive while a named instance remains"

printf '1..%d\n' "$pass_count"
