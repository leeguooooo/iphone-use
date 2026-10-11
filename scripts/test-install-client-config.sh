#!/usr/bin/env bash
# install.sh's MCP registration with desktop clients (Cursor, Claude Desktop),
# in an isolated fake HOME: skipped when the app is absent, merged without
# touching other servers, backed up, idempotent, never overwriting a foreign
# entry or an unreadable file, and off with IPHONE_USE_NO_CLIENT_SETUP=1.
# No network, no device.
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/install.sh"
TMP_BASE="$(cd -P "${TMPDIR:-/tmp}" && pwd)"
TMP_ROOT="$(cd -P "$(mktemp -d "$TMP_BASE/iphone-use-client-config-test.XXXXXX")" && pwd)"
trap 'rm -rf "$TMP_ROOT"' EXIT INT TERM

pass_count=0
pass() { pass_count=$((pass_count + 1)); printf 'ok %d - %s\n' "$pass_count" "$*"; }
fail_test() { printf 'not ok - %s\n' "$*" >&2; exit 1; }

new_home() {
    TEST_HOME="$TMP_ROOT/$1/home"
    mkdir -p "$TEST_HOME"
    TEST_HOME="$(cd -P "$TEST_HOME" && pwd)"
    touch "$TEST_HOME/.iphone-use-installer-test-root"
    CURSOR="$TEST_HOME/.cursor/mcp.json"
    DESKTOP="$TEST_HOME/Library/Application Support/Claude/claude_desktop_config.json"
    MCP="$TEST_HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
}

run_hook() {  # run_hook [VAR=value ...]
    env HOME="$TEST_HOME" IPHONE_USE_INTERNAL_TEST_CLIENT_CONFIG_ONLY=1 "$@" \
        /bin/bash "$INSTALLER" >"$TEST_HOME/out" 2>&1 \
        || fail_test "the hook failed: $(cat "$TEST_HOME/out")"
}

jq_py() {  # jq_py <file> <python expression over cfg>
    python3 -c 'import json, sys; cfg = json.load(open(sys.argv[1])); print(eval(sys.argv[2]))' "$1" "$2"
}

# ── no client installed: nothing is created ─────────────────────────────────
new_home absent
run_hook
[ ! -e "$TEST_HOME/.cursor" ] || fail_test "created ~/.cursor although Cursor is absent"
[ ! -e "$TEST_HOME/Library" ] || fail_test "created the Claude Desktop folder although it is absent"
if grep -Eq 'Cursor|Claude Desktop' "$TEST_HOME/out"; then
    fail_test "mentioned an absent client: $(cat "$TEST_HOME/out")"
fi
pass "absent clients are skipped silently"

# ── fresh configs: directory present, no file yet ───────────────────────────
new_home fresh
mkdir -p "$(dirname "$CURSOR")" "$(dirname "$DESKTOP")"
run_hook
[ "$(jq_py "$CURSOR" 'cfg["mcpServers"]["iphone-use"]["command"]')" = "$MCP" ] \
    || fail_test "Cursor entry missing: $(cat "$CURSOR")"
[ "$(jq_py "$DESKTOP" 'cfg["mcpServers"]["iphone-use"]["command"]')" = "$MCP" ] \
    || fail_test "Claude Desktop entry missing"
[ ! -e "$CURSOR.iphone-use.bak" ] || fail_test "backed up a file that did not exist"
grep -q 'Cursor can now use the iPhone' "$TEST_HOME/out" || fail_test "no success line: $(cat "$TEST_HOME/out")"
pass "a new config file is written"

# ── merge into existing configs; others kept; backup made; idempotent ───────
new_home merge
mkdir -p "$(dirname "$CURSOR")" "$(dirname "$DESKTOP")"
printf '{"mcpServers":{"other":{"command":"/bin/other","args":["x"]}},"theme":"dark"}\n' >"$CURSOR"
printf '{"preferences":{"a":1}}\n' >"$DESKTOP"
chmod 600 "$CURSOR"
cp "$CURSOR" "$TEST_HOME/cursor.orig"
run_hook
[ "$(jq_py "$CURSOR" 'cfg["mcpServers"]["other"]["args"]')" = "['x']" ] || fail_test "lost another server"
[ "$(jq_py "$CURSOR" 'cfg["theme"]')" = dark ] || fail_test "lost another top-level key"
[ "$(jq_py "$CURSOR" 'sorted(cfg["mcpServers"])')" = "['iphone-use', 'other']" ] || fail_test "wrong servers"
[ "$(jq_py "$DESKTOP" 'cfg["preferences"]["a"]')" = 1 ] || fail_test "lost Claude Desktop preferences"
cmp -s "$CURSOR.iphone-use.bak" "$TEST_HOME/cursor.orig" || fail_test "no faithful backup"
case "$(stat -c %a "$CURSOR" 2>/dev/null || stat -f %Lp "$CURSOR")" in
    600) ;; *) fail_test "file mode not kept" ;;
esac
cp "$CURSOR" "$TEST_HOME/cursor.after"
run_hook
cmp -s "$CURSOR" "$TEST_HOME/cursor.after" || fail_test "a second run changed the file"
grep -q 'Cursor already has the iphone-use MCP server' "$TEST_HOME/out" || fail_test "second run not reported as done"
pass "existing configs are merged, backed up and left alone on a rerun"

# ── foreign entry with our name, and an unreadable file: untouched ──────────
new_home foreign
mkdir -p "$(dirname "$CURSOR")" "$(dirname "$DESKTOP")"
printf '{"mcpServers":{"iphone-use":{"command":"/somewhere/else"}}}\n' >"$CURSOR"
printf '{ "mcpServers": { // a comment\n } }\n' >"$DESKTOP"
cp "$CURSOR" "$TEST_HOME/c.orig"; cp "$DESKTOP" "$TEST_HOME/d.orig"
run_hook
cmp -s "$CURSOR" "$TEST_HOME/c.orig" || fail_test "overwrote a foreign iphone-use entry"
cmp -s "$DESKTOP" "$TEST_HOME/d.orig" || fail_test "rewrote a file it could not parse"
[ ! -e "$CURSOR.iphone-use.bak" ] && [ ! -e "$DESKTOP.iphone-use.bak" ] || fail_test "backup made without a change"
grep -q 'another command; left it unchanged' "$TEST_HOME/out" || fail_test "no foreign-entry warning"
grep -q 'Could not read .* as JSON' "$TEST_HOME/out" || fail_test "no unreadable-file warning"
pass "a foreign entry and an unreadable file are left unchanged"

# ── named instance: own name and env ────────────────────────────────────────
new_home instance
mkdir -p "$(dirname "$CURSOR")"
run_hook IPHONE_USE_INTERNAL_TEST_MCP_NAME=iphone-use-work IPHONE_USE_INTERNAL_TEST_MCP_INSTANCE=work
[ "$(jq_py "$CURSOR" 'cfg["mcpServers"]["iphone-use-work"]["env"]["IPHONE_USE_INSTANCE"]')" = work ] \
    || fail_test "instance env missing: $(cat "$CURSOR")"
pass "a named instance is registered with its env"

# ── opt-out ─────────────────────────────────────────────────────────────────
new_home optout
mkdir -p "$(dirname "$CURSOR")"
run_hook IPHONE_USE_NO_CLIENT_SETUP=1
[ ! -e "$CURSOR" ] || fail_test "wrote Cursor's config despite IPHONE_USE_NO_CLIENT_SETUP=1"
pass "IPHONE_USE_NO_CLIENT_SETUP=1 skips the desktop clients"

# ── the hook needs the sentinel ─────────────────────────────────────────────
new_home nosentinel
rm "$TEST_HOME/.iphone-use-installer-test-root"
if env HOME="$TEST_HOME" IPHONE_USE_INTERNAL_TEST_CLIENT_CONFIG_ONLY=1 /bin/bash "$INSTALLER" >/dev/null 2>&1; then
    fail_test "the hook ran without its sentinel"
fi
pass "the test hook refuses to run without its sentinel"

printf '1..%d\n' "$pass_count"
