#!/bin/bash
# auto-update.sh — unattended upgrades for the iphone-use daemons, gated on idleness.
#
#   auto-update.sh enable        install this script to ~/.iphone-use and register a
#                                daily LaunchAgent (04:30) that runs `auto-update.sh run`
#   auto-update.sh disable       unload and remove that LaunchAgent and keep it off:
#                                install.sh turns it on at every install unless disabled here
#   auto-update.sh status        show whether the job is registered and the last outcome
#   auto-update.sh run           upgrade every idle instance IF a newer release exists
#   auto-update.sh run --force   upgrade now regardless of idleness (still needs a newer release
#                                unless --reinstall)
#   auto-update.sh run --dry-run decide and report, change nothing
#   auto-update.sh run --instance NAME   only that instance (`default` for the default one)
#
# install.sh turns this job on by default. It stays off after `disable` (the
# marker ~/.iphone-use/auto-update.disabled), an install with --no-auto-update or
# IPHONE_USE_AUTO_UPDATE=0, or with IPHONE_USE_NO_UPDATE_CHECK / USE_NO_UPDATE_CHECK / CI.
#
# Every daemon on this Mac is upgraded: the default instance and each named
# instance (`install.sh --instance NAME`, LaunchAgent com.leeguoo.iphone-use.NAME),
# each gated on its own phone being idle.
#
# "Idle" means nobody is using the phone right now: no X-Phone-Owner lease, no
# hold lease, the daemon is not releasing/reconnecting, no live viewer is
# watching, and no agent request reached the daemon in the last
# AUTO_UPDATE_IDLE_SECS (default 900s, from /agent/status `idle_secs`).
# Upgrading restarts the daemon, and a daemon restart in the middle of someone's
# phone task is exactly the kind of interruption #72 was filed about — so when
# the phone is busy we log one line and try again tomorrow.
#
# runner being up (device_state "ready") is deliberately NOT "in use". Since v0.6.3
# the daemon no longer idle-releases runner, so a connected phone is "ready" around
# the clock and a gate on it never upgraded anything. Daemons older than
# `idle_secs` expose activity only through the owner lease (it outlives a named
# client's last request by 300s); unnamed clients are invisible to them.
#
# The upgrade itself is the documented one-liner (`install.sh` from the release's
# pinned commit): SHA-256-checked assets, skill first, daemon second, rollback on
# failure. Success is the version the restarted daemon reports, not the
# installer's exit status; it is recorded in ~/.iphone-use/auto-update-last.json,
# which /agent/status (`auto_update`) and `iphone-use doctor` report.
#
# After an instance is upgraded, its runner product is prebuilt
# (`setup-runner.sh prebuild`): every release replaces the on-phone daemon
# library linked into the device runner, so without it the next connect
# builds, validates and re-signs the runner while a person waits (6.5 min on
# a busy Mac). The prebuild never touches the phone, runs below normal
# priority, skips itself on a busy Mac or while setup works, and its failure
# changes nothing: the next connect then builds as before.
# AUTO_UPDATE_PREBUILD=0 turns it off.
set -euo pipefail

# Installs made before the rename set PHONE_REMOTE_*; read them as IPHONE_USE_*
# (the new name wins). Names are [A-Z0-9_], so the eval is safe.
for _legacy in $(env | sed -n 's/^PHONE_REMOTE_\([A-Z0-9_][A-Z0-9_]*\)=.*/\1/p'); do
    eval "[ -n \"\${IPHONE_USE_${_legacy}+x}\" ] || export IPHONE_USE_${_legacy}=\"\$PHONE_REMOTE_${_legacy}\""
done
unset _legacy

REPO="${IPHONE_USE_REPO:-leeguooooo/iphone-use}"
STATE_DIR="${IPHONE_USE_STATE_DIR:-$HOME/.iphone-use}"
LABEL_PREFIX="com.leeguoo.iphone-use"
LABEL="$LABEL_PREFIX.autoupdate"
AGENTS_DIR="$HOME/Library/LaunchAgents"
DAEMON_PLIST="$AGENTS_DIR/$LABEL_PREFIX.plist"
JOB_PLIST="$AGENTS_DIR/$LABEL.plist"
# Names under the label prefix that are not daemons (instance.rs RESERVED_NAMES).
RESERVED_NAMES="runner wda autoupdate daily-maintenance flow-reverify"
LOG_DIR="$HOME/Library/Logs/iPhoneUse"
LOG="$LOG_DIR/auto-update.log"
INSTALLED_SELF="$STATE_DIR/auto-update.sh"
DISABLED_MARKER="$STATE_DIR/auto-update.disabled"
OUTCOME="$STATE_DIR/auto-update-last.json"
# Set per run to install.sh at the release's own commit (see cmd_run); `main`
# only when the tag cannot be resolved, since main may be ahead of any release.
INSTALLER_URL="${AUTO_UPDATE_INSTALLER_URL:-}"
HOUR="${AUTO_UPDATE_HOUR:-4}"
IDLE_SECS="${AUTO_UPDATE_IDLE_SECS:-900}"
case "$IDLE_SECS" in ''|*[!0-9]*) IDLE_SECS=900 ;; esac  # a non-integer override falls back
MINUTE="${AUTO_UPDATE_MINUTE:-30}"
# How long a restarted daemon gets to report the new version.
SETTLE_SECS="${AUTO_UPDATE_SETTLE_SECS:-60}"
case "$SETTLE_SECS" in ''|*[!0-9]*) SETTLE_SECS=60 ;; esac
# How long one runner prebuild may take before it is stopped.
PREBUILD_TIMEOUT="${AUTO_UPDATE_PREBUILD_TIMEOUT:-1800}"
case "$PREBUILD_TIMEOUT" in ''|*[!0-9]*) PREBUILD_TIMEOUT=1800 ;; esac

# Under launchd stderr already is $LOG; teeing there too wrote every line twice.
# (`-ef` cannot tell: /dev/fd/2 stats on its own device, so compare inodes.)
stderr_is_log() {
    local fd file
    fd="$(stat -L -f %i /dev/fd/3 3>&2 2>/dev/null)" || return 1
    file="$(stat -f %i "$LOG" 2>/dev/null)" || return 1
    [ -n "$fd" ] && [ "$fd" = "$file" ]
}
log() {
    mkdir -p "$LOG_DIR"
    local line; line="$(date '+%Y-%m-%dT%H:%M:%S%z') $*"
    if stderr_is_log; then
        printf '%s\n' "$line" >&2
    else
        printf '%s\n' "$line" | tee -a "$LOG" >&2
    fi
}
die() { log "ERROR: $*"; exit 1; }

# --- daemon access ----------------------------------------------------------
# One line per daemon on this Mac, "<instance> <plist>", the default instance first.
instances() {
    if [ -f "$DAEMON_PLIST" ]; then printf 'default %s\n' "$DAEMON_PLIST"; fi
    local plist name reserved
    for plist in "$AGENTS_DIR/$LABEL_PREFIX".*.plist; do
        [ -f "$plist" ] || continue
        name="${plist##*/}"; name="${name#"$LABEL_PREFIX".}"; name="${name%.plist}"
        # runner supervisors (.runner, .runner.NAME) and other dotted labels are not daemons
        [[ "$name" =~ ^[a-z][a-z0-9-]{0,31}$ ]] || continue
        for reserved in $RESERVED_NAMES; do [ "$name" != "$reserved" ] || continue 2; done
        printf '%s %s\n' "$name" "$plist"
    done
}

daemon_env() {  # print "port token host" from a daemon LaunchAgent plist
    [ -f "$1" ] || return 1
    python3 - "$1" <<'PY'
import plistlib, sys
env = plistlib.load(open(sys.argv[1], 'rb')).get('EnvironmentVariables', {})
get = lambda name, default: env.get('IPHONE_USE_' + name, env.get('PHONE_REMOTE_' + name, default))  # older plists
# Without an agent token the daemon takes its password as the bearer; a default
# install has only the password.
print(get('PORT', '44321'), get('AGENT_TOKEN', '') or get('PASSWORD', '') or '-', get('HOST', '127.0.0.1'))
PY
}

status_json() {  # status_json <instance> <plist>
    if [ "$1" = default ] && [ -n "${AUTO_UPDATE_STATUS_URL:-}" ]; then
        curl -fsS -m 10 -H "Authorization: Bearer ${AUTO_UPDATE_TOKEN:-}" "$AUTO_UPDATE_STATUS_URL"
        return
    fi
    local port token host
    read -r port token host < <(daemon_env "$2") || return 1
    [ "$token" != "-" ] || token=""
    [ "$host" != "0.0.0.0" ] || host="127.0.0.1"
    curl -fsS -m 10 --noproxy '*' -H "Authorization: Bearer $token" "http://$host:$port/agent/status"
}

status_version() {  # the version a daemon reports, or nothing
    status_json "$1" "$2" 2>/dev/null \
        | python3 -c 'import sys,json; print(json.load(sys.stdin).get("version",""))' 2>/dev/null || true
}

latest_tag() {
    if [ -n "${AUTO_UPDATE_LATEST_TAG:-}" ]; then printf '%s\n' "$AUTO_UPDATE_LATEST_TAG"; return; fi
    # The releases/latest redirect needs no API token and is not rate-limited
    # like api.github.com; the tag is the last path segment of the target URL.
    local url
    url="$(curl -fsSIL -m 15 -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" || return 1
    case "$url" in
        */releases/tag/v[0-9]*) printf '%s\n' "${url##*/}" ;;
        *) return 1 ;;
    esac
}

newer_or_same() {  # is version $1 >= version $2?
    python3 - "$1" "$2" <<'PY'
import sys
def ver(v):
    try: return tuple(int(x) for x in v.lstrip('v').split('-')[0].split('.'))
    except ValueError: return ()
a, b = ver(sys.argv[1]), ver(sys.argv[2])
sys.exit(0 if a and b and a >= b else 1)
PY
}

# --- the decision -----------------------------------------------------------
# Prints one of: upgrade | skip:<reason> ; exit 0 either way, 2 on a probe failure.
decide() {  # decide <force> <reinstall> <latest tag> <instance> <plist>
    local force="$1" reinstall="$2" latest="$3" status
    status="$(status_json "$4" "$5")" || { printf 'skip:daemon_unreachable\n'; return 2; }
    STATUS_JSON="$status" LATEST="$latest" FORCE="$force" REINSTALL="$reinstall" IDLE_SECS="$IDLE_SECS" python3 - <<'PY'
import json, os, sys
s = json.loads(os.environ['STATUS_JSON'])
latest = os.environ['LATEST'].lstrip('v')
current = str(s.get('version') or '').lstrip('v')
force = os.environ['FORCE'] == '1'
reinstall = os.environ['REINSTALL'] == '1'
def ver(v):
    try: return tuple(int(x) for x in v.split('-')[0].split('.'))
    except ValueError: return ()
if not s.get('ok'):
    print('skip:daemon_not_ok'); sys.exit(0)
if not reinstall and (not current or ver(latest) <= ver(current)):
    print(f'skip:up_to_date current={current} latest={latest}'); sys.exit(0)
if not force:
    if s.get('owner'):
        print(f"skip:phone_owned owner={s['owner']} lease={s.get('owner_lease_remaining_secs')}s"); sys.exit(0)
    if (s.get('hold_remaining_secs') or 0) > 0:
        print(f"skip:held {s['hold_remaining_secs']}s"); sys.exit(0)
    if s.get('releasing') or s.get('reconnecting'):
        print('skip:transitioning'); sys.exit(0)
    # The hand-off lives in the daemon's memory: a restart would forget the
    # phone is a person's, and the next agent request would start runner under them.
    if s.get('human_handoff'):
        print('skip:handed_to_person'); sys.exit(0)
    viewers = (s.get('viewer_count') or 0) + (s.get('mjpeg_viewer_count') or 0)
    if viewers > 0:
        print(f'skip:watched viewers={viewers}'); sys.exit(0)
    idle = s.get('idle_secs')
    need = int(os.environ['IDLE_SECS'])
    if isinstance(idle, (int, float)) and idle < need:
        print(f'skip:in_use idle_secs={int(idle)} (<{need})'); sys.exit(0)
print(f'upgrade current={current} latest={latest}')
PY
}

# IPHONE_USE_AUTO_UPDATE_JOB tells install.sh it runs under this job, so it must
# not reload the job: a bootout would kill the upgrade halfway. Earlier copies of
# this script passed IPHONE_USE_AUTO_UPDATE=1, which install.sh reads the same way.
# IPHONE_USE_RELEASE_REF keeps the installer on the release this run decided on.
run_installer() {  # run_installer <instance> <release tag>
    if [ -n "${AUTO_UPDATE_INSTALLER_CMD:-}" ]; then
        AUTO_UPDATE_INSTANCE="$1" IPHONE_USE_AUTO_UPDATE_JOB=1 bash -c "$AUTO_UPDATE_INSTALLER_CMD"
    elif [ "$1" = default ]; then
        curl -fsSL -m 120 "$INSTALLER_URL" | IPHONE_USE_AUTO_UPDATE_JOB=1 IPHONE_USE_RELEASE_REF="$2" sh
    else
        curl -fsSL -m 120 "$INSTALLER_URL" \
            | IPHONE_USE_AUTO_UPDATE_JOB=1 IPHONE_USE_RELEASE_REF="$2" sh -s -- --instance "$1"
    fi
}

# The installed setup entry point of an instance (setup-runner.sh's own layout).
setup_script() {  # setup_script <instance>
    if [ "$1" = default ]; then
        printf '%s\n' "$STATE_DIR/setup-runner.sh"
    else
        printf '%s\n' "$STATE_DIR/instances/$1/setup-runner.sh"
    fi
}

# Build the upgraded instance's runner product now, so its next connect reuses
# it (see the header). Bounded by PREBUILD_TIMEOUT; a stop signal makes the
# setup engine end its xcodebuild. Never fails the run.
prebuild_runner() {  # prebuild_runner <instance>
    if [ "${AUTO_UPDATE_PREBUILD:-1}" = 0 ]; then
        log "prebuild[$1]: off (AUTO_UPDATE_PREBUILD=0)"; return 0
    fi
    local script pid waited=0 rc=0
    if [ -n "${AUTO_UPDATE_PREBUILD_CMD:-}" ]; then
        AUTO_UPDATE_INSTANCE="$1" bash -c "$AUTO_UPDATE_PREBUILD_CMD" </dev/null >>"$LOG" 2>&1 &
    else
        script="$(setup_script "$1")"
        if [ ! -f "$script" ]; then
            log "prebuild[$1]: skipped, $script is missing"; return 0
        fi
        IPHONE_USE_INSTANCE="$1" /bin/bash "$script" prebuild </dev/null >>"$LOG" 2>&1 &
    fi
    pid=$!
    log "prebuild[$1]: building the runner product for the next connect (pid $pid)"
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$waited" -ge "$PREBUILD_TIMEOUT" ]; then
            kill -TERM "$pid" 2>/dev/null || true
            log "prebuild[$1]: stopped after ${PREBUILD_TIMEOUT}s; the next connect builds"
            # TERM is a request; the run holds auto-update.lock, so do not
            # wait on a build that ignores it.
            waited=0
            while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 30 ]; do
                sleep 1; waited=$((waited + 1))
            done
            kill -KILL "$pid" 2>/dev/null || true
            break
        fi
        sleep 1; waited=$((waited + 1))
    done
    wait "$pid" || rc=$?
    log "prebuild[$1]: finished (exit $rc)"
}

# The version the restarted daemon serves (empty when it never answered),
# waiting up to SETTLE_SECS for it to reach <latest>.
settled_version() {  # settled_version <instance> <plist> <latest>
    local deadline=$(( $(date +%s) + SETTLE_SECS )) version
    while :; do
        version="$(status_version "$1" "$2")"
        if [ -n "$version" ] && newer_or_same "$version" "$3"; then break; fi
        [ "$(date +%s)" -lt "$deadline" ] || break
        sleep 3
    done
    printf '%s' "$version"
}

OUTCOME_ROWS=""
UPGRADED_INSTANCES=""  # upgraded by this run: their runner products are prebuilt
CURRENT_INSTANCES=" "  # found up to date this run: an earlier failure there is resolved
record() {  # record <instance> <from> <to> <ok 0|1> <error>
    OUTCOME_ROWS="${OUTCOME_ROWS}$1"$'\t'"$2"$'\t'"$3"$'\t'"$4"$'\t'"$5"$'\n'
}

# Through a temp file and a rename, so the daemon never reads half of it.
write_outcome() {  # write_outcome <latest>
    # Nothing attempted: rewrite only to clear failures now found current.
    if [ -z "$OUTCOME_ROWS" ]; then
        [ -f "$OUTCOME" ] && [ "$CURRENT_INSTANCES" != " " ] || return 0
        python3 - "$OUTCOME" "$CURRENT_INSTANCES" <<'PY' || return 0
import json, sys
last = json.load(open(sys.argv[1]))
current = set(sys.argv[2].split())
sys.exit(0 if any(not r.get('ok') and r.get('instance') in current for r in last.get('instances') or []) else 1)
PY
    fi
    local tmp
    tmp="$(mktemp "$STATE_DIR/.auto-update-last.XXXXXX")" || return 0
    if ROWS="$OUTCOME_ROWS" LATEST="$1" PREVIOUS="$OUTCOME" CURRENT="$CURRENT_INSTANCES" python3 - >"$tmp" <<'PY'
import json, os, time
rows = []
for line in os.environ['ROWS'].splitlines():
    instance, frm, to, ok, error = (line.split('\t') + [''] * 5)[:5]
    rows.append({'instance': instance, 'from': frm or None, 'to': to or None,
                 'ok': ok == '1', 'error': error or None})
# An earlier failure stays reported until a run upgrades that instance or finds
# it current; a run that only reached the other instances does not clear it.
seen = {r['instance'] for r in rows} | set(os.environ['CURRENT'].split())
try:
    previous = json.load(open(os.environ['PREVIOUS'])).get('instances') or []
except (OSError, ValueError, AttributeError):
    previous = []
rows += [r for r in previous if isinstance(r, dict) and not r.get('ok') and r.get('instance') not in seen]
print(json.dumps({'at': int(time.time()), 'latest': os.environ['LATEST'].lstrip('v'),
                  'ok': all(r['ok'] for r in rows), 'instances': rows}, indent=1))
PY
    then
        chmod 0644 "$tmp" && mv -f "$tmp" "$OUTCOME"
    else
        rm -f "$tmp"
    fi
}

# install.sh ships and enables this script, but only an install refreshes it,
# and this job is what runs the installs: replace the copy under ~/.iphone-use
# from the tag just installed so a fix to the idle gate reaches this machine.
# `mv` swaps the inode, so the running bash keeps reading the old file safely.
# Best effort: a failure logs and keeps the current copy.
# Resolve a release tag to its exact commit, like install.sh does, so a moved
# tag cannot swap in different code.
release_commit() {
    local tag="$1" refs commit
    case "$tag" in v[0-9]*) ;; *) return 1 ;; esac
    refs="$(git ls-remote "https://github.com/$REPO.git" "refs/tags/$tag" "refs/tags/$tag^{}" 2>/dev/null)" || return 1
    commit="$(printf '%s\n' "$refs" | awk -v want="refs/tags/$tag^{}" '$2 == want { print $1; exit }')"
    [ -n "$commit" ] || commit="$(printf '%s\n' "$refs" | awk -v want="refs/tags/$tag" '$2 == want { print $1; exit }')"
    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || return 1
    printf '%s\n' "$commit"
}

refresh_self() {
    local tag="$1" url tmp commit
    [ -f "$INSTALLED_SELF" ] || return 0
    [ -n "$tag" ] || { log "self-refresh skipped: no release tag"; return 0; }
    if [ -n "${AUTO_UPDATE_SELF_URL:-}" ]; then
        url="$AUTO_UPDATE_SELF_URL"
    else
        commit="$(release_commit "$tag")" || { log "self-refresh skipped: cannot resolve $tag to a commit"; return 0; }
        url="https://raw.githubusercontent.com/$REPO/$commit/scripts/auto-update.sh"
    fi
    tmp="$(mktemp "$STATE_DIR/auto-update.sh.XXXXXX")" || return 0
    if curl -fsSL -m 30 "$url" -o "$tmp" && bash -n "$tmp" && head -1 "$tmp" | grep -q '^#!/bin/bash'; then
        if cmp -s "$tmp" "$INSTALLED_SELF"; then
            rm -f "$tmp"
        else
            chmod 0755 "$tmp" && mv -f "$tmp" "$INSTALLED_SELF" && log "self-refresh: $INSTALLED_SELF updated from $tag"
        fi
    else
        rm -f "$tmp"; log "self-refresh: could not fetch $url; keeping the current copy"
    fi
}

cmd_run() {
    local force=0 reinstall=0 dry=0 only=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --force) force=1 ;;
            --reinstall) reinstall=1; force=1 ;;
            --dry-run) dry=1 ;;
            --instance) [ "$#" -ge 2 ] || die "--instance needs a name"; only="$2"; shift ;;
            *) die "unknown option for run: $1" ;;
        esac
        shift
    done
    mkdir -p "$STATE_DIR"
    if [ -f "$DISABLED_MARKER" ] && [ "$force" = 0 ]; then
        log "skip: disabled ($DISABLED_MARKER; 'auto-update.sh enable' turns it back on)"; return 0
    fi
    local lock="$STATE_DIR/auto-update.lock"
    if ! mkdir "$lock" 2>/dev/null; then
        log "skip: another auto-update is running (lock $lock)"; return 0
    fi
    # shellcheck disable=SC2064  # expand now: the local is gone by EXIT time
    trap "rmdir '$lock' 2>/dev/null" EXIT
    local latest list
    latest="$(latest_tag)" || { log "decision: skip:latest_unresolved"; return 2; }
    list="$(instances)"
    [ -z "$only" ] || list="$(printf '%s\n' "$list" | awk -v want="$only" '$1 == want')"
    [ -n "$list" ] || { log "decision: skip:not_installed (no daemon LaunchAgent${only:+ for $only})"; return 0; }
    if [ -z "$INSTALLER_URL" ] && [ -z "${AUTO_UPDATE_INSTALLER_CMD:-}" ]; then
        local commit
        if commit="$(release_commit "$latest")"; then
            INSTALLER_URL="https://raw.githubusercontent.com/$REPO/$commit/install.sh"
        else
            INSTALLER_URL="https://raw.githubusercontent.com/$REPO/main/install.sh"
            log "could not resolve $latest to a commit; the installer comes from main"
        fi
    fi
    local rc=0 failed=0 upgraded=0 instance plist decision drc before after installer_ok why
    while read -r instance plist; do
        drc=0
        decision="$(decide "$force" "$reinstall" "$latest" "$instance" "$plist")" || drc=$?
        log "decision[$instance]: $decision"
        case "$decision" in
            upgrade*) ;;
            skip:up_to_date*) CURRENT_INSTANCES="$CURRENT_INSTANCES$instance "; continue ;;
            *) [ "$drc" = 0 ] || rc=$drc; continue ;;
        esac
        if [ "$dry" = 1 ]; then log "dry-run: would run the installer now for $instance"; continue; fi
        before="$(status_version "$instance" "$plist")"
        log "upgrading $instance from ${before:-?} to $latest via $INSTALLER_URL"
        installer_ok=1
        run_installer "$instance" "$latest" </dev/null >>"$LOG" 2>&1 || installer_ok=0
        after="$(settled_version "$instance" "$plist" "$latest")"
        if [ -n "$after" ] && newer_or_same "$after" "$latest"; then
            upgraded=1
            UPGRADED_INSTANCES="$UPGRADED_INSTANCES$instance "
            log "upgrade finished: $instance ${before:-?} -> $after"
            record "$instance" "$before" "$after" 1 ""
        else
            failed=1
            if [ "$installer_ok" = 0 ]; then
                why="the installer failed and rolled back; see $LOG"
            elif [ -n "$after" ]; then
                why="still $after after the install"
            else
                why="the daemon did not answer after the install"
            fi
            log "ERROR: $instance: $why"
            record "$instance" "$before" "$after" 0 "$why"
        fi
    done <<<"$list"
    [ "$dry" = 1 ] || write_outcome "$latest"
    [ "$upgraded" = 0 ] || refresh_self "$latest"
    for instance in $UPGRADED_INSTANCES; do prebuild_runner "$instance"; done
    [ "$failed" = 0 ] || return 1
    return "$rc"
}

# --- the schedule -----------------------------------------------------------
write_job_plist() {
    mkdir -p "$(dirname "$JOB_PLIST")" "$LOG_DIR"
    local tmp; tmp="$(mktemp "$JOB_PLIST.XXXXXX")"
    cat > "$tmp" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>$INSTALLED_SELF</string><string>run</string></array>
    <key>EnvironmentVariables</key>
    <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
    <key>StartCalendarInterval</key>
    <dict><key>Hour</key><integer>$HOUR</integer><key>Minute</key><integer>$MINUTE</integer></dict>
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>$LOG</string>
    <key>StandardErrorPath</key><string>$LOG</string>
</dict></plist>
PLIST
    plutil -lint "$tmp" >/dev/null || { rm -f "$tmp"; die "generated $JOB_PLIST is not a valid plist"; }
    mv -f "$tmp" "$JOB_PLIST"
}

job_running() {  # is a run of the job in progress (perhaps the one that called us)?
    launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null | grep -q 'state = running'
}

cmd_enable() {
    mkdir -p "$STATE_DIR"
    local self tmp
    self="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
    if [ "$self" != "$INSTALLED_SELF" ]; then
        tmp="$(mktemp "$STATE_DIR/auto-update.sh.XXXXXX")"
        cp "$self" "$tmp" && chmod 0755 "$tmp" && mv -f "$tmp" "$INSTALLED_SELF"
    fi
    chmod 0755 "$INSTALLED_SELF"
    rm -f "$DISABLED_MARKER"
    write_job_plist
    # Reloading the job while it runs would kill that run, which may be the
    # installer that called us. The plist on disk is current; launchd reads it
    # at the next load.
    if job_running; then
        log "enabled: $LABEL is running now; its new definition loads next time"
        return 0
    fi
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$JOB_PLIST"
    log "enabled: $LABEL runs '$INSTALLED_SELF run' daily at $(printf '%02d:%02d' "$HOUR" "$MINUTE")"
}

cmd_disable() {
    mkdir -p "$STATE_DIR"
    : > "$DISABLED_MARKER"
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    rm -f "$JOB_PLIST"
    log "disabled: $LABEL removed; installs keep it off until 'auto-update.sh enable'"
}

cmd_status() {
    if [ -f "$JOB_PLIST" ] && launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
        echo "auto-update: enabled ($JOB_PLIST, daily $(printf '%02d:%02d' "$HOUR" "$MINUTE"))"
    elif [ -f "$DISABLED_MARKER" ]; then
        echo "auto-update: disabled ($DISABLED_MARKER)"
    else
        echo "auto-update: disabled"
    fi
    local latest instance plist
    latest="$(latest_tag 2>/dev/null || true)"
    while read -r instance plist; do
        [ -n "$instance" ] || continue
        echo "decision now [$instance]: $(decide 0 0 "$latest" "$instance" "$plist" 2>/dev/null || true)"
    done <<<"$(instances)"
    if [ -f "$OUTCOME" ]; then echo "last upgrade:"; cat "$OUTCOME"; fi
    if [ -f "$LOG" ]; then echo "last log lines:"; tail -n 5 "$LOG"; fi
}

case "${1:-}" in
    enable) cmd_enable ;;
    disable) cmd_disable ;;
    status) cmd_status ;;
    run) shift; cmd_run "$@" ;;
    *) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
