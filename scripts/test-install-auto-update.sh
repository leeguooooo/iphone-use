#!/usr/bin/env bash
# install.sh turns the auto-update job on at every install, unless the user
# turned it off (persistently), CI or the update check says leave it, or the
# install itself runs under the job (reloading the job would kill it). And an
# upgrade carries forward every IPHONE_USE_* / IPU_RUNNER_* setting of the old
# plist, so the nightly upgrade never drops one. The functions are extracted
# from install.sh and run against a stub auto-update.sh and throwaway plists.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-install-auto-update.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass=0
ok_t() { pass=$((pass + 1)); printf 'ok %d - %s\n' "$pass" "$1"; }
bad()  { printf 'not ok - %s\n' "$1" >&2; exit 1; }

FUNCS="$(sed -n '/^plist_own_env_keys() {$/,/^}$/p;/^setup_auto_update() {$/,/^}$/p' "$ROOT/install.sh")"
[ -n "$FUNCS" ] || bad "setup_auto_update / plist_own_env_keys not found in install.sh"

# A stub auto-update.sh that records how it was called.
mkdir -p "$TMP/checkout/scripts"
cat > "$TMP/checkout/scripts/auto-update.sh" <<EOF
#!/bin/bash
echo "\$1 state_dir=\${IPHONE_USE_STATE_DIR:-unset}" >> "$TMP/calls"
EOF

setup() {  # setup [VAR=value ...] -> the stub's calls, one per line
    rm -f "$TMP/calls"
    env -i HOME="$TMP/home" PATH=/usr/bin:/bin "$@" bash -c "
info() { :; }; ok() { :; }; warn() { :; }
SCRIPT_IS_LOCAL=1; SCRIPT_DIR='$TMP/checkout'; REPO=x/y; LOG_DIR='$TMP/logs'
$FUNCS
setup_auto_update"
    cat "$TMP/calls" 2>/dev/null || true
}

mkdir -p "$TMP/home/.iphone-use"
[ "$(setup)" = "enable state_dir=unset" ] || bad "a plain install did not enable the job: '$(setup)'"
ok_t "a plain install turns the job on"
[ "$(setup IPHONE_USE_STATE_DIR=/elsewhere)" = "enable state_dir=unset" ] \
    || bad "the daemon's IPHONE_USE_STATE_DIR leaked into the job: '$(setup IPHONE_USE_STATE_DIR=/elsewhere)'"
ok_t "the daemon's state dir does not move the job's files"
[ "$(setup IPHONE_USE_AUTO_UPDATE=0)" = "disable state_dir=unset" ] \
    || bad "IPHONE_USE_AUTO_UPDATE=0 did not disable the job"
ok_t "--no-auto-update / IPHONE_USE_AUTO_UPDATE=0 turns it off"
touch "$TMP/home/.iphone-use/auto-update.disabled"
[ -z "$(setup)" ] || bad "an install re-enabled a job the user disabled"
ok_t "a disabled job stays off across installs"
rm -f "$TMP/home/.iphone-use/auto-update.disabled"
for var in IPHONE_USE_AUTO_UPDATE_JOB=1 IPHONE_USE_AUTO_UPDATE=1 CI=1 IPHONE_USE_NO_UPDATE_CHECK=1 USE_NO_UPDATE_CHECK=1; do
    [ -z "$(setup "$var")" ] || bad "$var still touched the job"
done
ok_t "under the job, in CI or with the update check off, the job is left alone"

cat > "$TMP/old.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>EnvironmentVariables</key><dict>
<key>IPHONE_USE_PORT</key><string>45432</string>
<key>IPHONE_USE_BASE_PATH</key><string>/ix</string>
<key>IPHONE_USE_TELEMETRY</key><string>0</string>
<key>IPU_RUNNER_TRANSPORT</key><string>wifi</string>
<key>IPHONE_USE_BACKEND</key><string>mirror</string>
<key>IPHONE_USE_WDA_URL</key><string>http://127.0.0.1:8100</string>
<key>IPHONE_USE_INTERNAL_TEST_X</key><string>1</string>
<key>IPU_RUNNER_ASC_KEY_ID</key><string>half-a-set</string>
<key>RUST_LOG</key><string>info</string>
<key>PATH</key><string>/usr/bin</string>
</dict></dict></plist>
PLIST
got="$(bash -c "$FUNCS
plist_own_env_keys '$TMP/old.plist'" | sort | tr '\n' ' ')"
[ "$got" = "IPHONE_USE_BASE_PATH IPHONE_USE_PORT IPHONE_USE_TELEMETRY IPU_RUNNER_TRANSPORT " ] \
    || bad "carried-forward keys were '$got'"
ok_t "an upgrade carries our settings forward, minus the mirror backend, old runner names, ASC keys and test hooks"
grep -q '^    IPHONE_USE_BASE_PATH \\$' "$ROOT/install.sh" || bad "IPHONE_USE_BASE_PATH is not on the default instance's list"
ok_t "the default instance keeps IPHONE_USE_BASE_PATH"

printf '1..%d\n' "$pass"
