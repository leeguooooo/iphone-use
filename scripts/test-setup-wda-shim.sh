#!/bin/bash
# setup-wda.sh is a thin entry point: it finds the iphone-use binary and hands
# every command to `iphone-use setup-native <command>` with the same
# environment. This checks the hand-over, the instance guard, and the
# messages when no capable binary is installed — with a fake binary, so no
# phone, Xcode or launchd is touched.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SETUP="$ROOT/scripts/setup-wda.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
n=0; failed=0
check() {
    n=$((n + 1))
    if eval "$2"; then echo "ok $n - $1"; else echo "not ok $n - $1"; failed=1; fi
}

/bin/bash -n "$SETUP"
check "the shim parses under macOS bash 3.2" '[ $? = 0 ]'

HOME_DIR="$TMP/home"; mkdir -p "$HOME_DIR"
FAKE="$TMP/iphone-use"
cat > "$FAKE" <<'FAKE_BIN'
#!/bin/bash
if [ "$1 $2" = "setup-native --supports" ]; then
    case "$3" in setup|status|stop|pause|resume|doctor|instance-context) exit 0 ;; *) exit 1 ;; esac
fi
{
    echo "args=$*"
    echo "instance=${IPHONE_USE_INSTANCE:-}"
    echo "script=${IPHONE_USE_SETUP_SCRIPT:-}"
    echo "udid=${WDA_UDID:-}"
} > "$FAKE_LOG"
exit 7
FAKE_BIN
chmod +x "$FAKE"
OLD="$TMP/old-iphone-use"
printf '#!/bin/bash\nexit 2\n' > "$OLD"; chmod +x "$OLD"

run() { env -i HOME="$HOME_DIR" PATH=/usr/bin:/bin FAKE_LOG="$TMP/log" "$@"; }

for command in setup status stop pause resume doctor instance-context; do
    rm -f "$TMP/log"
    if [ "$command" = setup ]; then args=(); else args=("$command"); fi
    run IPHONE_USE_SETUP_BIN="$FAKE" WDA_UDID=0000AB /bin/bash "$SETUP" ${args[@]+"${args[@]}"}
    code=$?
    check "$command is handed to setup-native with its exit code" \
        "[ $code = 7 ] && grep -qx 'args=setup-native $command' '$TMP/log'"
done
check "the environment and the script path travel with it" \
    "grep -qx 'udid=0000AB' '$TMP/log' && grep -qx 'script=$SETUP' '$TMP/log' && grep -qx 'instance=default' '$TMP/log'"

rm -f "$TMP/log"
run IPHONE_USE_SETUP_BIN="$FAKE" IPHONE_USE_INSTANCE=lab /bin/bash "$SETUP" status
check "IPHONE_USE_INSTANCE is passed through" "grep -qx 'instance=lab' '$TMP/log'"

# An installed copy belongs to its instance.
COPY="$HOME_DIR/.iphone-use/instances/lab/setup-wda.sh"
mkdir -p "$(dirname "$COPY")"; cp "$SETUP" "$COPY"
rm -f "$TMP/log"
run IPHONE_USE_SETUP_BIN="$FAKE" /bin/bash "$COPY" status
check "an instance's copy runs as that instance" "grep -qx 'instance=lab' '$TMP/log'"
out="$(run IPHONE_USE_SETUP_BIN="$FAKE" IPHONE_USE_INSTANCE=other /bin/bash "$COPY" status 2>&1)"
code=$?
check "an instance's copy refuses another instance (exit 2)" \
    "[ $code = 2 ] && printf '%s' \"\$out\" | grep -q 'belongs to instance \"lab\"'"

out="$(run IPHONE_USE_SETUP_BIN="$OLD" /bin/bash "$SETUP" status 2>&1)"
code=$?
check "with no capable binary it says to reinstall (exit 1)" \
    "[ $code = 1 ] && printf '%s' \"\$out\" | grep -q 'Reinstall it' && printf '%s' \"\$out\" | grep -q 'setup-native status'"

out="$(run IPHONE_USE_SETUP_BIN="$FAKE" /bin/bash "$SETUP" frobnicate 2>&1)"
code=$?
check "an unknown command is refused (exit 1)" \
    "[ $code = 1 ] && printf '%s' \"\$out\" | grep -q 'unknown command: frobnicate'"

check "the shim still names the runner sources the installer ships" \
    "grep -q 'IPhoneUseRunner' '$SETUP'"

echo "1..$n"
exit $failed
