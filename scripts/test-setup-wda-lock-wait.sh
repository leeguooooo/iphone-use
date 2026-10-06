#!/bin/bash
# Before launching the runner, setup-wda.sh asks CoreDevice whether the phone
# is locked, publishes `locked` at once, and continues the moment it unlocks
# (instead of xcodebuild's ~70 s automation-mode timeout per attempt).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
cat > "$tmp/bin/xcrun" <<'FAKE'
#!/bin/bash
while [ $# -gt 0 ]; do [ "$1" = -j ] && { out="$2"; shift; }; shift; done
# A scripted sequence of answers (one per line, consumed in order) wins.
if [ -n "${LOCK_SEQ:-}" ] && [ -s "$LOCK_SEQ" ]; then
    answer="$(head -1 "$LOCK_SEQ")"; sed -i '' 1d "$LOCK_SEQ"
    [ "$answer" = fail ] && exit 1   # devicectl could not answer
    printf '{"result":{"passcodeRequired":%s}}' "$answer" > "$out"
    exit 0
fi
if [ -f "$UNLOCK_AT" ] && [ "$(date +%s)" -ge "$(cat "$UNLOCK_AT")" ]; then
    printf '{"result":{"passcodeRequired":false}}' > "$out"
else
    printf '{"result":{"passcodeRequired":true}}' > "$out"
fi
FAKE
chmod +x "$tmp/bin/xcrun"
# The block under test: from the lock-wait header to the build status line.
block="$(sed -n '/^# ── 3b. Wait for the phone to be unlocked/,/^_setstatus building "\$_BUILD_BLOCKER" "building + launching the device runner"$/p' "$here/setup-wda.sh")"
[ -n "$block" ] || { echo "not ok 1 - lock-wait block not found"; exit 1; }
devicectl_t="$(sed -n '/^_devicectl_t() {/,/^}/p' "$here/setup-wda.sh")"
run() {  # $1 = seconds until unlock (or "never"), extra env after
    local unlock_in="$1"; shift
    rm -f "$tmp/unlock_at"
    [ "$unlock_in" = never ] || echo $(( $(date +%s) + unlock_in )) > "$tmp/unlock_at"
    env PATH="$tmp/bin:$PATH" UNLOCK_AT="$tmp/unlock_at" LOCK_SEQ="$tmp/seq" "$@" /bin/bash -c "
        WDA_UDID=X; _BUILD_BLOCKER=''
        info() { echo \"== \$*\"; }; ok() { echo \"ok: \$*\"; }; die() { echo \"die: \$*\"; exit 9; }
        warn() { echo \"warn: \$*\"; }
        _setstatus() { echo \"status \$1 \$2\"; }
        _prepare_locked_retry() { echo locked-retry; }
        $devicectl_t
        $block
        echo launched"
}
n=0; fail=0
check() { n=$((n + 1)); if eval "$2"; then echo "ok $n - $1"; else echo "not ok $n - $1"; echo "$out" | sed 's/^/#   /'; fail=1; fi; }

out="$(run 2)"
check "a locked phone publishes 'locked' before anything launches" 'echo "$out" | grep -q "^status lock-wait locked"'
check "unlocking lets the launch continue" 'echo "$out" | grep -q "^ok: iPhone unlocked after" && echo "$out" | tail -1 | grep -q launched'
check "the build status clears the blocker afterwards" 'echo "$out" | grep -q "^status building $"'

out="$(run 0)"
check "an unlocked phone goes straight on, no lock status" '! echo "$out" | grep -q lock-wait && echo "$out" | tail -1 | grep -q launched'

out="$(run never WDA_LOCK_WAIT_SECS=2 WDA_KEEPALIVE=1)"; code=$?
check "under KeepAlive a phone locked past the wait hands over to the locked backoff" '[ $code -eq 1 ] && echo "$out" | grep -q locked-retry && ! echo "$out" | grep -q launched'

out="$(run never WDA_LOCK_WAIT_SECS=2 WDA_KEEPALIVE=0)"; code=$?
check "interactive setup fails with a locked status" '[ $code -eq 9 ] && echo "$out" | grep -q "^status building-fail locked"'
# locked, one stray "not required", locked again, then unlocked for good
printf 'true\ntrue\nfalse\ntrue\nfalse\nfalse\n' > "$tmp/seq"
out="$(run never)"; code=$?
check "a single unlocked reading between locked ones does not launch" '[ $code -eq 0 ] && [ "$(echo "$out" | grep -c "^launched")" -eq 1 ] && [ ! -s "$tmp/seq" ]'
rm -f "$tmp/seq"

# locked, then several reads devicectl could not answer, then really unlocked
printf 'true\nfail\nfail\nfail\nfail\ntrue\nfalse\nfalse\n' > "$tmp/seq"
out="$(run never)"; code=$?
check "unreadable lock states while waiting never count as unlocked" '[ $code -eq 0 ] && [ "$(echo "$out" | grep -c "^launched")" -eq 1 ] && [ ! -s "$tmp/seq" ]'
rm -f "$tmp/seq"

out="$(run 1 WDA_LOCK_WAIT_SECS=abc)"; code=$?
check "a non-numeric wait falls back to 300 s instead of breaking the comparison" '[ $code -eq 0 ] && echo "$out" | grep -q "using 300" && echo "$out" | tail -1 | grep -q launched'
echo "1..$n"
exit "$fail"
