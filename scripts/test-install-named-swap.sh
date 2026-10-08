#!/usr/bin/env bash
# The named-instance daemon swap in install.sh must (1) be able to roll back
# from its very first destructive step, and (2) only accept a daemon that is
# provably ours: launchd's pid running the instance binary, owning the
# listener, answering an authenticated /agent/status with 2xx, same pid after.
# probe_launchd_daemon runs against fake launchctl/ps/lsof/curl; nothing here
# touches a real LaunchAgent.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-named-swap.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass=0
ok_t() { pass=$((pass + 1)); printf 'ok %d - %s\n' "$pass" "$1"; }
bad()  { printf 'not ok - %s\n' "$1" >&2; exit 1; }

# --- structure of install_named_instance -----------------------------------
FN="$(sed -n '/^install_named_instance() {$/,/^}$/p' "$ROOT/install.sh")"
[ -n "$FN" ] || bad "install_named_instance not found"
first_bootout="$(printf '%s\n' "$FN" | grep -n 'launchctl bootout' | head -1 | cut -d: -f1)"
rollback_def="$(printf '%s\n' "$FN" | grep -n '^    named_rollback() {' | head -1 | cut -d: -f1)"
[ -n "$rollback_def" ] && [ -n "$first_bootout" ] || bad "missing rollback or bootout"
[ "$rollback_def" -lt "$first_bootout" ] \
    || bad "named_rollback is defined after the first bootout (line $rollback_def >= $first_bootout)"
ok_t "named_rollback is defined before the first launchctl bootout"

ok_line="$(printf '%s\n' "$FN" | grep -n 'ok "Instance \$INSTANCE_NAME daemon is up' | cut -d: -f1)"
# From the old job's bootout (after the rollback body) up to success, a plain
# `die` would strand a booted-out instance with half-installed files.
after_def="$(printf '%s\n' "$FN" | awk -v s="$rollback_def" 'NR>s && /^    }$/ {print NR; exit}')"
stray="$(printf '%s\n' "$FN" | sed -n "${after_def},${ok_line}p" | grep -n '[^_]die ' || true)"
[ -z "$stray" ] || bad "plain die between bootout and success: $stray"
ok_t "no plain die between the bootout and the success line"

printf '%s\n' "$FN" | grep -q 'wait_label_gone "gui/\$UID_NUM/\$label"' \
    || bad "no wait after bootout"
printf '%s\n' "$FN" | grep -q 'probe_launchd_daemon "gui/\$UID_NUM/\$label" "\$binary"' \
    || bad "readiness does not use probe_launchd_daemon"
ok_t "waits for the old job to go and probes identity + authenticated status"

# --- probe_launchd_daemon against fakes --------------------------------------
PROBE="$(sed -n '/^probe_launchd_daemon() {$/,/^}$/p' "$ROOT/install.sh")"
[ -n "$PROBE" ] || bad "probe_launchd_daemon not found"
BIN="$TMP/app/Contents/MacOS/iphone-use"
mkdir -p "$TMP/bin" "$(dirname "$BIN")"
UIDN="$(id -u)"

# Fakes read their behaviour from files so each case can flip one thing.
cat > "$TMP/bin/launchctl" <<EOF
#!/bin/sh
[ "\$1" = print ] || exit 0
pid="\$(cat "$TMP/pid")"
prog="\$(cat "$TMP/program")"
printf '\tpid = %s\n\tprogram = %s\n' "\$pid" "\$prog"
# The pid changes after the first print when "flap" is set.
[ -f "$TMP/flap" ] && echo 4243 > "$TMP/pid"
exit 0
EOF
cat > "$TMP/bin/ps" <<EOF
#!/bin/sh
case "\$*" in
  *uid=*) echo " $UIDN" ;;
  *command=*) cat "$TMP/command" ;;
esac
EOF
cat > "$TMP/bin/lsof" <<EOF
#!/bin/sh
[ -f "$TMP/listening" ]
EOF
cat > "$TMP/bin/curl" <<EOF
#!/bin/sh
# Record whether a token arrived on stdin (config), never in argv.
case "\$*" in *Bearer*) echo argv-token > "$TMP/leak" ;; esac
cfg="\$(cat 2>/dev/null || true)"
case "\$cfg" in *"Bearer good"*) exit 0 ;; *Bearer*) exit 22 ;; esac
[ -f "$TMP/open" ] && exit 0
exit 22
EOF
chmod +x "$TMP"/bin/*

reset() {
    echo 4242 > "$TMP/pid"; printf '%s' "$BIN" > "$TMP/program"
    printf '%s --serve\n' "$BIN" > "$TMP/command"
    touch "$TMP/listening"; rm -f "$TMP/flap" "$TMP/open" "$TMP/leak"
}
probe() {  # token
    PATH="$TMP/bin:/usr/bin:/bin" UID_NUM="$UIDN" \
        IPHONE_USE_LSOF_BIN="$TMP/bin/lsof" IPHONE_USE_PS_BIN="$TMP/bin/ps" \
        /bin/bash -c "$PROBE
probe_launchd_daemon gui/$UIDN/test.label \"$BIN\" 45999 http://127.0.0.1:45999/agent/status \"\$0\" && echo PID=\$PROBED_DAEMON_PID" "$1"
}

reset
out="$(probe good)" || bad "a healthy, owned, authenticated daemon was rejected"
[ "$out" = "PID=4242" ] || bad "unexpected probe output: $out"
[ ! -f "$TMP/leak" ] || bad "the token was passed in curl's argv"
ok_t "accepts our pid + listener + authenticated 200, token never in argv"

reset
! probe wrong >/dev/null || bad "a 401 (wrong token) passed"
ok_t "rejects a daemon that answers but refuses the token"

reset; printf '/usr/bin/other\n' > "$TMP/command"
! probe good >/dev/null || bad "a pid running another binary passed"
ok_t "rejects a launchd pid that runs a different binary"

reset; rm -f "$TMP/listening"
! probe good >/dev/null || bad "a pid that does not own the listener passed"
ok_t "rejects when the listener belongs to someone else"

reset; touch "$TMP/flap"
! probe good >/dev/null || bad "a pid that changed during the probe passed"
ok_t "rejects a crash-looping job (pid changed during the probe)"

printf '1..%d\n' "$pass"
