#!/bin/bash
# Runs every device-free setup/install test with HOME pointed at a throwaway
# directory, so no test can reach the real ~/.iphone-use or LaunchAgents.
# Prints one PASS/FAIL line per test and exits non-zero when any fails.
set -u
cd "$(dirname "$0")/.."
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/iu-test-home.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT
mkdir -p "$SANDBOX/home"
failed=0
for test in \
    scripts/test-install-release-transaction.sh \
    scripts/test-install-cli-link.sh \
    scripts/test-install-runner-sources.sh \
    scripts/test-uninstall-safety.sh \
    scripts/test-setup-wda-shim.sh \
    scripts/test-instance-context.py \
    scripts/test-auto-update.py
do
    [ -f "$test" ] || { printf 'SKIP %s (missing)\n' "$test"; continue; }
    log="$SANDBOX/$(basename "$test").log"
    case "$test" in
        *.py) runner=(python3 "$test") ;;
        *) runner=(bash "$test") ;;
    esac
    if HOME="$SANDBOX/home" "${runner[@]}" >"$log" 2>&1; then
        printf 'PASS %s\n' "$test"
    else
        printf 'FAIL %s\n' "$test"
        tail -25 "$log" | sed 's/^/    /'
        failed=1
    fi
done
exit "$failed"
