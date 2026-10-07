#!/usr/bin/env bash
# scripts/setup-wda.sh — one-command setup for the device layer (the iphone-use
# device runner).
#
# Builds the iphone-use device runner (runner/IPhoneUseRunner, an XCTest UI-test
# bundle that serves the daemon's HTTP API on the phone's port 8100 and an MJPEG
# stream on 9100), installs it on your iPhone, keeps it running, starts the
# localhost relays, and points the iphone-use daemon at them. Encodes every
# pitfall we hit validating this on hardware (see docs/wda-setup.html).
#
# The file keeps its name, its WDA_* environment variables, its launchd label and
# its status file: the daemon, the installer and existing installs all address
# the device layer through them.
#
# Usage:
#   ./scripts/setup-wda.sh            # full setup (interactive prompts as needed)
#   ./scripts/setup-wda.sh status     # is the runner + relay up?
#   ./scripts/setup-wda.sh stop       # stop the runner + relays
#   ./scripts/setup-wda.sh pause      # give the phone back; disable auto-restart
#   ./scripts/setup-wda.sh resume     # re-enable the managed runner supervisor
#   ./scripts/setup-wda.sh doctor     # read-only preflight checklist
#   ./scripts/setup-wda.sh instance-context  # read-only: resolved paths/ports
#
# Env overrides:
#   WDA_UDID=...        target device UDID (default: the one USB iPhone)
#   WDA_TEAM_ID=...     Apple dev team (default: Xcode's last-selected team)
#   WDA_ASC_KEY_PATH=... absolute .p8 path; with both IDs, use ASC API key signing
#   WDA_ASC_KEY_ID=...  App Store Connect key ID (all three WDA_ASC_* required)
#   WDA_ASC_ISSUER_ID=... App Store Connect issuer ID
#   WDA_BUNDLE_ID=...   runner bundle id (default: derived from validated Team ID)
#   IPU_RUNNER_SRC=...  device runner sources (default: ~/.iphone-use/runner, laid
#                       down by install.sh; a repo checkout uses its own runner/)
#   WDA_PORT=...        control relay port (default: 8100; named instances: derived)
#   MJPEG_PORT=...      video relay port (default: 9100; named instances: derived)
#   PHONE_REMOTE_INSTANCE=... which daemon/phone pair (default: default). A named
#                       instance keeps its state, build products, launchd labels
#                       and ports apart and requires an explicit target UDID (#67).
#   WDA_ALLOW_LAN=1     permit an unauthenticated LAN relay (unsafe; default off)
#   WDA_RUNNER_REBUILD=1 ignore the recorded runner product and build again
#
# Requirements: Xcode (an Apple ID in Settings → Accounts, or WDA_ASC_* signing),
# the iPhone paired + Developer Mode on. The USB relay is `iphone-use relay`
# (macOS usbmuxd; no libimobiledevice needed); a Homebrew `iproxy` is used only
# when the app binary is missing or predates it. `socat` is accepted only with
# the explicit WDA_ALLOW_LAN=1 escape hatch.
set -eu
umask 077

# When spawned by the daemon (POST /agent/mode) the environment is a bare
# LaunchAgent PATH — Homebrew tools (socat, legacy iproxy) and even xcrun helpers
# live outside it. Extend deterministically rather than relying on the shell.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:$PATH"

COMMAND="${1:-setup}"

# BEGIN instance context (#67). One daemon drives one phone; everything this
# script names on the Mac derives from PHONE_REMOTE_INSTANCE the same way
# crates/server/src/instance.rs does (pinned by
# scripts/fixtures/instance-derivation.json). `default` keeps every path, label
# and port a single-phone install has always used.
INSTANCE_LABEL_PREFIX="com.leeguoo.iphone-use"
# `wda` would collide with the default WDA label; the rest are the product's
# other LaunchAgents, which share the label namespace.
INSTANCE_RESERVED_NAMES="wda autoupdate daily-maintenance flow-reverify"
# Named instances take a deterministic slot so a rerun picks the same ports;
# a slot whose ports another instance claims, or something listens on, is
# skipped. Persisted ports always win over derivation.
INSTANCE_PORT_SLOTS=500
INSTANCE_DAEMON_PORT_BASE=45500
INSTANCE_WDA_PORT_BASE=8200
INSTANCE_MJPEG_PORT_BASE=9200

_instance_name_valid() {
    local name="$1" reserved
    printf '%s' "$name" | LC_ALL=C grep -Eq '^[a-z][a-z0-9-]{0,31}$' || return 1
    [ "$(printf '%s' "$name" | wc -l | tr -d ' ')" = "0" ] || return 1
    for reserved in $INSTANCE_RESERVED_NAMES; do
        [ "$name" != "$reserved" ] || return 1
    done
}

_instance_state_dir_override_valid() {
    local dir="$1" base="$HOME/.iphone-use"
    case "$dir" in
        /*) ;;
        *) return 1 ;;
    esac
    case "/$dir/" in
        */./*|*/../*) return 1 ;;
    esac
    case "$dir" in
        /|"$HOME"|"$HOME/"|"$base"|"$base"/*|*$'\n'*|*$'\r'*) return 1 ;;
    esac
    case "$base/" in
        "${dir%/}"/*) return 1 ;;
    esac
}

# Sets INSTANCE_NAME, STATE_DIR, WDA_AGENT_LABEL, DAEMON_LABEL from the
# environment and, for an installed copy, from where this script lives: the
# copy under ~/.iphone-use/instances/<name>/ is that instance's and refuses to
# run as any other.
_instance_resolve() {
    local requested="${PHONE_REMOTE_INSTANCE-}" self installed=""
    self="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")"
    case "$self" in
        "$HOME/.iphone-use/instances/"*/setup-wda.sh)
            installed="${self#"$HOME/.iphone-use/instances/"}"
            installed="${installed%/setup-wda.sh}"
            ;;
        "$HOME/.iphone-use/setup-wda.sh") installed=default ;;
    esac
    [ -n "$requested" ] || requested="${installed:-default}"
    if [ "$requested" != default ] && ! _instance_name_valid "$requested"; then
        printf 'PHONE_REMOTE_INSTANCE="%s" is not a valid instance name: lowercase [a-z][a-z0-9-], at most 32 chars, not one of: %s\n' \
            "$requested" "$INSTANCE_RESERVED_NAMES" >&2
        exit 2
    fi
    if [ -n "$installed" ] && [ "$installed" != "$requested" ] \
        && [ -z "${PHONE_REMOTE_STATE_DIR:-}" ]; then
        printf 'this setup-wda.sh belongs to instance "%s" but PHONE_REMOTE_INSTANCE is "%s"; run %s instead\n' \
            "$installed" "$requested" \
            "$([ "$requested" = default ] && printf '%s' "$HOME/.iphone-use/setup-wda.sh" \
                || printf '%s' "$HOME/.iphone-use/instances/$requested/setup-wda.sh")" >&2
        exit 2
    fi
    INSTANCE_NAME="$requested"
    if [ "$INSTANCE_NAME" = default ]; then
        STATE_DIR="$HOME/.iphone-use"
        DAEMON_LABEL="$INSTANCE_LABEL_PREFIX"
        WDA_AGENT_LABEL="$INSTANCE_LABEL_PREFIX.wda"
    else
        STATE_DIR="$HOME/.iphone-use/instances/$INSTANCE_NAME"
        DAEMON_LABEL="$INSTANCE_LABEL_PREFIX.$INSTANCE_NAME"
        WDA_AGENT_LABEL="$INSTANCE_LABEL_PREFIX.wda.$INSTANCE_NAME"
    fi
    if [ -n "${PHONE_REMOTE_STATE_DIR:-}" ]; then
        if ! _instance_state_dir_override_valid "$PHONE_REMOTE_STATE_DIR"; then
            printf 'PHONE_REMOTE_STATE_DIR="%s" must be absolute, free of . and .., not / or HOME, and outside ~/.iphone-use\n' \
                "$PHONE_REMOTE_STATE_DIR" >&2
            exit 2
        fi
        STATE_DIR="${PHONE_REMOTE_STATE_DIR%/}"
    fi
    export PHONE_REMOTE_INSTANCE="$INSTANCE_NAME"
}

# Every other instance's plists, as "<instance> <plist>" lines. Only plists
# that carry a phone or a port count; the product's maintenance agents share
# the label prefix but bind neither.
_instance_other_plists() {
    local plist label name
    for plist in "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX".plist \
        "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX".*.plist; do
        [ -f "$plist" ] || continue
        label="$(/usr/libexec/PlistBuddy -c 'Print :Label' "$plist" 2>/dev/null || true)"
        case "$label" in
            "$INSTANCE_LABEL_PREFIX") name=default ;;
            "$INSTANCE_LABEL_PREFIX.wda") name=default ;;
            "$INSTANCE_LABEL_PREFIX.wda."*) name="${label#"$INSTANCE_LABEL_PREFIX.wda."}" ;;
            "$INSTANCE_LABEL_PREFIX."*) name="${label#"$INSTANCE_LABEL_PREFIX."}" ;;
            *) continue ;;
        esac
        [ "$name" != "$INSTANCE_NAME" ] || continue
        case " $INSTANCE_RESERVED_NAMES " in
            *" $name "*) continue ;;
        esac
        printf '%s %s\n' "$name" "$plist"
    done
}

_instance_plist_env() {
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$2" "$1" 2>/dev/null || true
}

# Ports other instances own: what their plists persist, plus the default
# instance's built-in defaults, which an older plist may leave implicit.
_instance_claimed_ports() {
    local name plist key value
    if [ "$INSTANCE_NAME" != default ]; then
        printf '44321\n8100\n9100\n'
    fi
    while read -r name plist; do
        for key in PHONE_REMOTE_PORT WDA_PORT MJPEG_PORT \
            PHONE_REMOTE_WDA_URL PHONE_REMOTE_WDA_MJPEG_URL; do
            value="$(_instance_plist_env "$plist" "$key")"
            value="$(printf '%s' "$value" | sed -n 's#^http://127\.0\.0\.1:\([0-9][0-9]*\).*$#\1#p;s#^\([0-9][0-9]*\)$#\1#p')"
            [ -z "$value" ] || printf '%s\n' "$value"
        done
    done < <(_instance_other_plists)
}

# The instance a UDID is already bound to, if it is not this one.
_instance_udid_owner() {
    local wanted name plist key value
    wanted="$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')"
    [ -n "$wanted" ] || return 1
    while read -r name plist; do
        for key in PHONE_REMOTE_UDID WDA_UDID; do
            value="$(_instance_plist_env "$plist" "$key" | tr '[:lower:]' '[:upper:]')"
            if [ -n "$value" ] && [ "$value" = "$wanted" ]; then
                printf '%s\n' "$name"
                return 0
            fi
        done
    done < <(_instance_other_plists)
    return 1
}

_instance_port_listening() {
    command -v lsof >/dev/null 2>&1 || return 1
    [ -n "$(lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | head -1)" ]
}

# First-choice slot for a name: cksum is POSIX, so every shell agrees.
_instance_port_slot() {
    local sum
    sum="$(printf '%s' "$1" | cksum | awk '{ print $1 }')"
    printf '%s\n' $((sum % INSTANCE_PORT_SLOTS))
}

# Sets INSTANCE_SLOT_DAEMON_PORT/WDA_PORT/MJPEG_PORT to the first slot from the
# name's own whose three ports are unclaimed and free. With PROBE=0 the
# first-choice slot is returned unchecked (what the fixture pins).
_instance_derive_ports() {
    local probe="${1:-1}" slot attempt claimed d w m
    slot="$(_instance_port_slot "$INSTANCE_NAME")"
    claimed=" $(_instance_claimed_ports | tr '\n' ' ') "
    attempt=0
    while [ "$attempt" -lt 20 ]; do
        d=$((INSTANCE_DAEMON_PORT_BASE + (slot + attempt) % INSTANCE_PORT_SLOTS))
        w=$((INSTANCE_WDA_PORT_BASE + (slot + attempt) % INSTANCE_PORT_SLOTS))
        m=$((INSTANCE_MJPEG_PORT_BASE + (slot + attempt) % INSTANCE_PORT_SLOTS))
        if [ "$probe" = 0 ]; then
            break
        fi
        case "$claimed" in
            *" $d "*|*" $w "*|*" $m "*) ;;
            *)
                if ! _instance_port_listening "$d" && ! _instance_port_listening "$w" \
                    && ! _instance_port_listening "$m"; then
                    break
                fi
                ;;
        esac
        attempt=$((attempt + 1))
    done
    [ "$attempt" -lt 20 ] || return 1
    INSTANCE_SLOT_DAEMON_PORT="$d"
    INSTANCE_SLOT_WDA_PORT="$w"
    INSTANCE_SLOT_MJPEG_PORT="$m"
}

# Refuse ports another instance owns and a phone another instance drives.
_instance_check_bindings() {
    local claimed port owner
    claimed=" $(_instance_claimed_ports | tr '\n' ' ') "
    for port in "$@"; do
        [ -n "$port" ] || continue
        case "$claimed" in
            *" $port "*)
                printf 'TCP %s is already assigned to another iphone-use instance; pick a different port for instance "%s"\n' \
                    "$port" "$INSTANCE_NAME" >&2
                return 1
                ;;
        esac
    done
    if [ -n "${WDA_UDID:-}" ] && owner="$(_instance_udid_owner "$WDA_UDID")"; then
        printf 'iPhone %s is already driven by iphone-use instance "%s"; one phone cannot be bound to two daemons\n' \
            "$WDA_UDID" "$owner" >&2
        return 1
    fi
}

# _instance_check_bindings reads every instance plist through PlistBuddy
# (~0.7 s with three instances) on each reconnect, yet its answer depends only
# on those files. Remember a passing verdict keyed on the instance, the ports,
# the target and each plist's path, mtime and size; any change re-checks.
# A refusal is never cached.
_instance_bindings_stamp() {
    local plist
    printf '%s|%s|%s|' "$INSTANCE_NAME" "$*" "${WDA_UDID:-}"
    for plist in "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX".plist \
        "$HOME/Library/LaunchAgents/$INSTANCE_LABEL_PREFIX".*.plist; do
        [ -f "$plist" ] || continue
        stat -f '%N:%m:%z;' "$plist" 2>/dev/null || printf '%s:?;' "$plist"
    done
}

_instance_check_bindings_cached() {
    local cache stamp
    cache="$STATE_DIR/.instance-bindings.ok"
    stamp="$(_instance_bindings_stamp "$@")"
    if [ -f "$cache" ] && [ ! -L "$cache" ] \
        && [ "$(cat "$cache" 2>/dev/null)" = "$stamp" ]; then
        return 0
    fi
    _instance_check_bindings "$@" || { rm -f "$cache" 2>/dev/null; return 1; }
    if [ ! -L "$cache" ]; then
        printf '%s' "$stamp" > "$cache" 2>/dev/null || true
    fi
}

_instance_resolve
# END instance context.

RUN_LOG="$STATE_DIR/wda-runner.log"
RUNNER_PID_FILE="$STATE_DIR/wda-runner.pid"
RELAY_PID_FILE="$STATE_DIR/wda-relay.pid"
# The device runner: one UI-test bundle, built with build-for-testing and run with
# test-without-building. Products live under this instance's state directory, so
# instances never share (or invalidate) each other's build.
RUNNER_SCHEME="IPhoneUseRunner"
RUNNER_TEST_ID="IPhoneUseRunnerUITests/RunnerTests/testServe"
RUNNER_APP_NAME="iPhoneUse-Runner.app"
RUNNER_DERIVED_DATA="$STATE_DIR/runner-build"
RUNNER_PRODUCTS_DIR="$RUNNER_DERIVED_DATA/Build/Products/Debug-iphoneos"
RUNNER_DEFAULT_SRC="$HOME/.iphone-use/runner"
WDA_AGENT_PLIST="$HOME/Library/LaunchAgents/$WDA_AGENT_LABEL.plist"
WDA_AGENT_LOG="$STATE_DIR/wda-agent.log"
WDA_RETRY_STATE="$STATE_DIR/wda-retry-state.v1"
WDA_AGENT_ROLLBACK_PLIST="$STATE_DIR/wda-supervisor.rollback.$$.plist"
DAEMON_PLIST="$HOME/Library/LaunchAgents/$DAEMON_LABEL.plist"
DAEMON_ROLLBACK_PLIST="$STATE_DIR/daemon.rollback.$$.plist"
UID_NUM="$(id -u)"
GUI_DOMAIN="gui/$UID_NUM"
RUNNER_BUILT_PRODUCTS=""
RUNNER_APP_PATH=""
RUNNER_BUILD_LOCKED=0
RUNNER_SOURCE_HASH=""
WDA_RUNNER_REPAIR_ATTEMPTED=0
WDA_RUNNER_VALIDATION_ERROR=""
KEEPALIVE_ATTEMPT_ACTIVE=0
KEEPALIVE_FAILURE_KIND="generic"
KEEPALIVE_LOCK_RETRY=0
INTERACTIVE_LOCK_STARTED_AT=0
INTERACTIVE_LOCK_NOTICE_AT=0
INTERACTIVE_LOCK_NOTICE_ATTEMPT=0
STATUS_RUN_ID=""
STATUS_OWNER_PID=""
STATUS_OWNER_START=""
STATUS_HEARTBEAT_PID=""

_existing_wda_env() {
    [ -f "$WDA_AGENT_PLIST" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" \
        "$WDA_AGENT_PLIST" 2>/dev/null || printf ''
}
_existing_daemon_env() {
    [ -f "$DAEMON_PLIST" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$1" \
        "$DAEMON_PLIST" 2>/dev/null || printf ''
}
_port_from_daemon_url() {
    local value
    value="$(_existing_daemon_env "$1")"
    printf '%s' "$value" | sed -n 's#^http://127\.0\.0\.1:\([0-9][0-9]*\).*$#\1#p'
}

# Preserve setup-owned supervisor policy on a plain rerun. Precedence is:
# explicit environment > existing runner supervisor > daemon endpoint > default.
#
# WDA_DIR names the WebDriverAgent checkout that releases before the device
# runner built from. Nothing is built from it any more; it is kept only so a
# PID-only record from such a release can still be matched against its cwd.
WDA_DIR="${WDA_DIR:-$(_existing_wda_env WDA_DIR)}"
WDA_DIR="${WDA_DIR:-$STATE_DIR/WebDriverAgent}"

# Device runner sources. Explicit > persisted in the supervisor > the runner/
# directory of the repo checkout this script was started from > the copy
# install.sh lays down. The installed copy of this script never looks next to
# itself, so ~/.iphone-use cannot be mistaken for a repo.
_runner_repo_src() {
    local self_dir repo
    self_dir="$(cd "$(dirname "$0")" 2>/dev/null && pwd)" || return 1
    case "$self_dir" in
        "$HOME/.iphone-use"|"$HOME/.iphone-use/"*) return 1 ;;
    esac
    repo="$(cd "$self_dir/.." 2>/dev/null && pwd)" || return 1
    [ -f "$repo/runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj" ] \
        && [ -d "$repo/crates/server" ] || return 1
    printf '%s\n' "$repo/runner"
}
RUNNER_SRC="${IPU_RUNNER_SRC:-$(_existing_wda_env IPU_RUNNER_SRC)}"
[ -n "$RUNNER_SRC" ] || RUNNER_SRC="$(_runner_repo_src || true)"
RUNNER_SRC="${RUNNER_SRC:-$RUNNER_DEFAULT_SRC}"
RUNNER_SRC="${RUNNER_SRC%/}"
RUNNER_PROJECT="$RUNNER_SRC/IPhoneUseRunner/IPhoneUseRunner.xcodeproj"
WDA_PORT="${WDA_PORT:-$(_existing_wda_env WDA_PORT)}"
WDA_PORT="${WDA_PORT:-$(_port_from_daemon_url PHONE_REMOTE_WDA_URL)}"
MJPEG_PORT="${MJPEG_PORT:-$(_existing_wda_env MJPEG_PORT)}"
MJPEG_PORT="${MJPEG_PORT:-$(_port_from_daemon_url PHONE_REMOTE_WDA_MJPEG_URL)}"
if [ "$INSTANCE_NAME" = default ]; then
    WDA_PORT="${WDA_PORT:-8100}"
    MJPEG_PORT="${MJPEG_PORT:-9100}"
elif [ -z "$WDA_PORT" ] || [ -z "$MJPEG_PORT" ]; then
    # The installer persists the named daemon's relay ports; derive only for a
    # setup that runs before it (the daemon port is the installer's concern).
    WDA_PORT="${WDA_PORT:-$(_existing_daemon_env WDA_PORT)}"
    MJPEG_PORT="${MJPEG_PORT:-$(_existing_daemon_env MJPEG_PORT)}"
    if [ -z "$WDA_PORT" ] || [ -z "$MJPEG_PORT" ]; then
        _instance_derive_ports 1 || {
            printf 'no free port slot for instance "%s"; set WDA_PORT and MJPEG_PORT\n' "$INSTANCE_NAME" >&2
            exit 1
        }
        WDA_PORT="${WDA_PORT:-$INSTANCE_SLOT_WDA_PORT}"
        MJPEG_PORT="${MJPEG_PORT:-$INSTANCE_SLOT_MJPEG_PORT}"
    fi
fi
WDA_BUNDLE_ID="${WDA_BUNDLE_ID:-$(_existing_wda_env WDA_BUNDLE_ID)}"
WDA_TEAM_ID="${WDA_TEAM_ID:-$(_existing_wda_env WDA_TEAM_ID)}"
# Restore the saved trio only when no ASC override was supplied. A partial
# explicit override must not silently combine with a different saved key.
if [ "${WDA_ASC_KEY_PATH+x}${WDA_ASC_KEY_ID+x}${WDA_ASC_ISSUER_ID+x}" = "" ]; then
    WDA_ASC_KEY_PATH="$(_existing_wda_env WDA_ASC_KEY_PATH)"
    WDA_ASC_KEY_ID="$(_existing_wda_env WDA_ASC_KEY_ID)"
    WDA_ASC_ISSUER_ID="$(_existing_wda_env WDA_ASC_ISSUER_ID)"
fi
# A named instance's first setup has no supervisor yet: its signing policy is
# whatever `install.sh --instance` persisted in the daemon plist, taken as a
# set (the three ASC keys never mix with a different saved key).
if [ "$INSTANCE_NAME" != default ]; then
    WDA_BUNDLE_ID="${WDA_BUNDLE_ID:-$(_existing_daemon_env WDA_BUNDLE_ID)}"
    WDA_TEAM_ID="${WDA_TEAM_ID:-$(_existing_daemon_env WDA_TEAM_ID)}"
    WDA_ALLOW_LAN="${WDA_ALLOW_LAN:-$(_existing_daemon_env WDA_ALLOW_LAN)}"
    if [ "${WDA_ASC_KEY_PATH:-}${WDA_ASC_KEY_ID:-}${WDA_ASC_ISSUER_ID:-}" = "" ]; then
        WDA_ASC_KEY_PATH="$(_existing_daemon_env WDA_ASC_KEY_PATH)"
        WDA_ASC_KEY_ID="$(_existing_daemon_env WDA_ASC_KEY_ID)"
        WDA_ASC_ISSUER_ID="$(_existing_daemon_env WDA_ASC_ISSUER_ID)"
    fi
fi
# WDA_REF, WDA_RUNNER_NAME and WDA_RUNNER_ICON configured the WebDriverAgent
# build this script no longer makes. A supervisor plist written by an older
# release may still carry them; they are read by nothing and dropped on the
# next supervisor install.
WDA_ALLOW_LAN="${WDA_ALLOW_LAN:-$(_existing_wda_env WDA_ALLOW_LAN)}"
WDA_ALLOW_LAN="${WDA_ALLOW_LAN:-0}"
WDA_UDID="${WDA_UDID:-${PHONE_REMOTE_UDID:-}}"
WDA_UDID="${WDA_UDID:-$(_existing_daemon_env PHONE_REMOTE_UDID)}"
WDA_UDID="${WDA_UDID:-$(_existing_wda_env WDA_UDID)}"
case "$WDA_ALLOW_LAN" in
    0|1) ;;
    *) printf 'WDA_ALLOW_LAN must be 0 or 1\n' >&2; exit 1 ;;
esac

# Read-only: what this instance resolves to, for install.sh/uninstall.sh and
# the fixture test. Exits non-zero when a port or the phone is already bound
# to another instance, before anything is touched.
if [ "$COMMAND" = "instance-context" ]; then
    _instance_derive_ports 0
    INSTANCE_FIRST_DAEMON_PORT="$INSTANCE_SLOT_DAEMON_PORT"
    INSTANCE_FIRST_WDA_PORT="$INSTANCE_SLOT_WDA_PORT"
    INSTANCE_FIRST_MJPEG_PORT="$INSTANCE_SLOT_MJPEG_PORT"
    INSTANCE_DAEMON_PORT="${PHONE_REMOTE_PORT:-$(_existing_daemon_env PHONE_REMOTE_PORT)}"
    if [ -z "$INSTANCE_DAEMON_PORT" ]; then
        if [ "$INSTANCE_NAME" = default ]; then
            INSTANCE_DAEMON_PORT=44321
        elif [ "$INSTANCE_FIRST_WDA_PORT" = "$WDA_PORT" ] \
            && [ "$INSTANCE_FIRST_MJPEG_PORT" = "$MJPEG_PORT" ]; then
            INSTANCE_DAEMON_PORT="$INSTANCE_FIRST_DAEMON_PORT"
        else
            _instance_derive_ports 1 || exit 1
            INSTANCE_DAEMON_PORT="$INSTANCE_SLOT_DAEMON_PORT"
        fi
    fi
    printf 'name=%s\n' "$INSTANCE_NAME"
    printf 'state_dir=%s\n' "$STATE_DIR"
    printf 'daemon_label=%s\n' "$DAEMON_LABEL"
    printf 'wda_label=%s\n' "$WDA_AGENT_LABEL"
    printf 'daemon_plist=%s\n' "$DAEMON_PLIST"
    printf 'wda_plist=%s\n' "$WDA_AGENT_PLIST"
    printf 'wda_dir=%s\n' "$WDA_DIR"
    printf 'runner_src=%s\n' "$RUNNER_SRC"
    printf 'daemon_port=%s\n' "$INSTANCE_DAEMON_PORT"
    printf 'wda_port=%s\n' "$WDA_PORT"
    printf 'mjpeg_port=%s\n' "$MJPEG_PORT"
    printf 'udid=%s\n' "$WDA_UDID"
    printf 'first_slot_ports=%s %s %s\n' "$INSTANCE_FIRST_DAEMON_PORT" \
        "$INSTANCE_FIRST_WDA_PORT" "$INSTANCE_FIRST_MJPEG_PORT"
    _instance_check_bindings "$INSTANCE_DAEMON_PORT" "$WDA_PORT" "$MJPEG_PORT" || exit 1
    exit 0
fi

BOLD=$'\033[1m'; RED=$'\033[0;31m'; GRN=$'\033[0;32m'; YLW=$'\033[1;33m'; RST=$'\033[0m'
# Each stage header carries the seconds since this run started, so a slow
# connect can be read straight off wda-agent.log.
# (Suffix, not prefix: the daemon finds rounds by the "== Checking prerequisites" text.)
info() { printf '%s\n' "${BOLD}== $* (+${SECONDS}s)${RST}"; }
ok()   { printf '%s\n' "${GRN}✓${RST} $*"; }
warn() { printf '%s\n' "${YLW}⚠${RST}  $*"; }
die()  { printf '%s\n' "${RED}✗ $*${RST}" >&2; exit 1; }

# BEGIN ASC signing helpers.
_asc_signing_enabled() {
    [ -n "${WDA_ASC_KEY_PATH:-}" ] && [ -n "${WDA_ASC_KEY_ID:-}" ] \
        && [ -n "${WDA_ASC_ISSUER_ID:-}" ]
}

_prepare_xcodebuild_args() {
    XCODEBUILD_ARGS=("$@")
    _asc_signing_enabled || return 0
    # Validate strings only. Never read/copy the private key or echo its values.
    # Spaces in an absolute key path remain within one argv element.
    if ! printf '%s\n' "$WDA_ASC_KEY_PATH" | LC_ALL=C grep -Eq '^/[^|[:cntrl:]]+\.p8$' \
        || ! printf '%s\n' "$WDA_ASC_KEY_ID" | LC_ALL=C grep -Eq '^[A-Za-z0-9]+$' \
        || ! printf '%s\n' "$WDA_ASC_ISSUER_ID" | LC_ALL=C grep -Eq '^[A-Za-z0-9-]+$' \
        || ! _safe_expected "$WDA_ASC_KEY_PATH$WDA_ASC_KEY_ID$WDA_ASC_ISSUER_ID"; then
        printf '%s\n' 'Invalid WDA_ASC_* configuration: use an absolute .p8 path and valid key/issuer IDs.' >&2
        return 1
    fi
    local argument has_updates=0
    for argument in "$@"; do
        [ "$argument" != "-allowProvisioningUpdates" ] || has_updates=1
    done
    if [ "$has_updates" = "0" ]; then
        XCODEBUILD_ARGS+=(-allowProvisioningUpdates)
    fi
    XCODEBUILD_ARGS+=(-authenticationKeyPath "$WDA_ASC_KEY_PATH"
        -authenticationKeyID "$WDA_ASC_KEY_ID"
        -authenticationKeyIssuerID "$WDA_ASC_ISSUER_ID"
        -allowProvisioningDeviceRegistration)
}

_wda_xcodebuild() {
    _prepare_xcodebuild_args "$@" || return 1
    "$XCODEBUILD_BIN" "${XCODEBUILD_ARGS[@]}"
}

# The runner is always launched from its built product: build-for-testing
# produced the .xctestrun, and test-without-building installs it as-is. This
# argv is also the runner's process identity (see _runner_signature_valid).
_prepare_runner_args() {
    [ -n "${WDA_XCTESTRUN:-}" ] || return 1
    _prepare_xcodebuild_args -destination "platform=iOS,id=$WDA_UDID" \
        test-without-building -xctestrun "$WDA_XCTESTRUN" \
        "-only-testing:$RUNNER_TEST_ID" || return 1
    RUNNER_ARGV=("${XCODEBUILD_ARGS[@]}")
    RUNNER_ARGS="${RUNNER_ARGV[*]}"
}

# Every build-time xcodebuild of the runner project: build-for-testing and the
# destination listing. -derivedDataPath keeps products under STATE_DIR.
_runner_xcodebuild() {
    _wda_xcodebuild -project "$RUNNER_PROJECT" -scheme "$RUNNER_SCHEME" \
        -derivedDataPath "$RUNNER_DERIVED_DATA" "$@"
}

_report_missing_xcode_account() {
    _setstatus signing-fail account "sign in to an Apple account in Xcode, or configure WDA_ASC_KEY_PATH / WDA_ASC_KEY_ID / WDA_ASC_ISSUER_ID for API key signing"
    die "Xcode has no signed-in Apple account. Open Xcode → Settings → Accounts,
   sign in and select the development team, or configure WDA_ASC_KEY_PATH,
   WDA_ASC_KEY_ID and WDA_ASC_ISSUER_ID for App Store Connect API key signing,
   then rerun."
}
# END ASC signing helpers.

if [ "$COMMAND" = "setup" ]; then
    mkdir -p "$STATE_DIR"
    chmod 700 "$STATE_DIR"
fi

# Self-install: keep a copy at a fixed path so the daemon's `POST /agent/mode`
# can start/stop WDA without knowing where the repo lives. Only a real setup may
# replace it; status/doctor/stop must be read-only with respect to the runtime
# script. Preserve the prior copy so a failed upgrade can roll back both the
# supervisor plist and the exact script it points to.
SELF_INSTALL="$STATE_DIR/setup-wda.sh"
SELF_INSTALL_ROLLBACK="$STATE_DIR/setup-wda.rollback.$$.sh"
SELF_INSTALL_REPLACED_THIS_RUN=0
SELF_INSTALL_HAD_PREVIOUS=0
if [ "$COMMAND" = "setup" ] \
    && [ "$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")" != "$SELF_INSTALL" ]; then
    if [ -f "$SELF_INSTALL" ]; then
        cp -p "$SELF_INSTALL" "$SELF_INSTALL_ROLLBACK" \
            || { printf 'could not back up existing setup-wda.sh\n' >&2; exit 1; }
        SELF_INSTALL_HAD_PREVIOUS=1
    fi
    SELF_INSTALL_TEMP="$STATE_DIR/setup-wda.install.$$"
    if cp -p "$0" "$SELF_INSTALL_TEMP" 2>/dev/null \
        && chmod 700 "$SELF_INSTALL_TEMP" 2>/dev/null \
        && mv -f "$SELF_INSTALL_TEMP" "$SELF_INSTALL" 2>/dev/null; then
        SELF_INSTALL_REPLACED_THIS_RUN=1
    else
        rm -f "$SELF_INSTALL_TEMP"
        printf 'could not atomically install setup-wda.sh at %s\n' "$SELF_INSTALL" >&2
        exit 1
    fi
fi

# devicectl can HANG FOREVER on a device whose tunnel is stuck "connecting"
# (hardware-verified 2026-06-12 — it wedged the whole mode-switch). Every call
# goes through this wrapper: run in background, kill after $1 seconds.
_devicectl_t() {
    local secs="$1"; shift
    local out; out="$(mktemp)"
    # Poll instead of a `( sleep N; kill ) &` watchdog: killing that subshell
    # left its `sleep` running, and reaping it made every call last the full
    # timeout — 8-10 s per call, three calls per connect, even when devicectl
    # answered at once.
    xcrun devicectl "$@" > "$out" 2>/dev/null &
    local pid=$! ticks=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$ticks" -ge $((secs * 10)) ]; then
            kill "$pid" 2>/dev/null
            break
        fi
        sleep 0.1
        ticks=$((ticks + 1))
    done
    wait "$pid" 2>/dev/null || true
    cat "$out"; rm -f "$out"
}

_wda_endpoint_lock_state() {
    local body
    body="$(curl -fsS -m 3 "$TARGET_URL/wda/locked" 2>/dev/null || true)"
    if printf '%s' "$body" \
        | grep -Eq '"value"[[:space:]]*:[[:space:]]*true'; then
        printf 'locked\n'
    elif printf '%s' "$body" \
        | grep -Eq '"value"[[:space:]]*:[[:space:]]*false'; then
        printf 'unlocked\n'
    else
        printf 'unknown\n'
    fi
}

_wda_failure_is_lock_related() {
    if grep -Eiq 'Unlock iPhone to Continue|device is locked|deviceprep.*Code=-3|Code=-3.*deviceprep' \
        "$RUN_LOG" 2>/dev/null; then
        return 0
    fi
    [ "${1:-endpoint}" != "log-only" ] || return 1
    [ "$(_wda_endpoint_lock_state)" = "locked" ]
}

# iOS 17+ refuses to hand an unlocked phone to XCTest until UI automation is
# switched on (Settings › Developer › Enable UI Automation) and any pending
# passcode / "Allow automation" prompt is accepted on the phone. xcodebuild
# then exits with "The test runner failed to initialize for UI testing.
# (Underlying Error: Timed out while enabling automation mode.)". Reported as
# the generic `wda` blocker, that left the operator reading logs for something
# that is fixed with two taps on the phone. Call after the lock checks: a
# locked phone can fail the same initialization and has its own blocker.
AUTOMATION_MODE_HINT="enable UI automation on the iPhone: Settings › Developer › Enable UI Automation, then accept any passcode or Allow automation prompt while the phone is unlocked"
_runner_log_shows_automation_mode_disabled() {
    grep -Eiq 'Timed out while enabling automation mode|failed to initialize for UI testing' \
        "${1:-$RUN_LOG}" 2>/dev/null
}

_report_automation_mode_disabled() {
    _setstatus building-fail automation_mode_disabled "$AUTOMATION_MODE_HINT"
    die "iOS did not enable UI automation for the device runner — $AUTOMATION_MODE_HINT, then rerun setup (KeepAlive retries on its own). Log: $RUN_LOG"
}

_exponential_retry_delay() {
    local base="$1"
    local cap="$2"
    local attempt="$3"
    local delay index
    delay="$base"
    index=1
    while [ "$index" -lt "$attempt" ] && [ "$delay" -lt "$cap" ]; do
        delay=$((delay * 2))
        [ "$delay" -le "$cap" ] || delay="$cap"
        index=$((index + 1))
    done
    printf '%s\n' "$delay"
}

_read_keepalive_retry_state() {
    KEEPALIVE_RETRY_ATTEMPT=0
    KEEPALIVE_RETRY_NEXT_AT=0
    KEEPALIVE_RETRY_KIND=""
    [ -e "$WDA_RETRY_STATE" ] || return 0
    _marker_file_secure "$WDA_RETRY_STATE" || return 1
    [ "$(awk 'END { print NR }' "$WDA_RETRY_STATE" 2>/dev/null)" = "4" ] || return 1
    [ "$(sed -n '1s/^version=//p' "$WDA_RETRY_STATE")" = "1" ] || return 1
    KEEPALIVE_RETRY_KIND="$(sed -n '2s/^kind=//p' "$WDA_RETRY_STATE")"
    KEEPALIVE_RETRY_ATTEMPT="$(sed -n '3s/^attempt=//p' "$WDA_RETRY_STATE")"
    KEEPALIVE_RETRY_NEXT_AT="$(sed -n '4s/^next_at=//p' "$WDA_RETRY_STATE")"
    case "$KEEPALIVE_RETRY_KIND" in
        generic|locked|xcode_too_old) ;;
        *) return 1 ;;
    esac
    case "$KEEPALIVE_RETRY_ATTEMPT" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$KEEPALIVE_RETRY_ATTEMPT" -le 64 ] 2>/dev/null || return 1
    case "$KEEPALIVE_RETRY_NEXT_AT" in
        ''|*[!0-9]*) return 1 ;;
    esac
    return 0
}

_wait_for_keepalive_retry() {
    local now wait_for
    if ! _read_keepalive_retry_state; then
        warn "ignoring invalid KeepAlive retry state: $WDA_RETRY_STATE"
        return 0
    fi
    if [ "$KEEPALIVE_RETRY_KIND" = "locked" ]; then
        KEEPALIVE_LOCK_RETRY=1
    fi
    now="$(date +%s)"
    if [ "$KEEPALIVE_RETRY_NEXT_AT" -gt "$now" ] 2>/dev/null; then
        wait_for=$((KEEPALIVE_RETRY_NEXT_AT - now))
        if [ "$KEEPALIVE_RETRY_KIND" != "locked" ]; then
            info "KeepAlive retry backoff: waiting ${wait_for}s before the next rebuild"
        fi
        sleep "$wait_for"
    fi
}

_record_keepalive_failure() {
    local attempt delay cap next_at tmp previous_kind
    previous_kind=""
    if _read_keepalive_retry_state; then
        previous_kind="$KEEPALIVE_RETRY_KIND"
    fi
    if [ "$previous_kind" = "$KEEPALIVE_FAILURE_KIND" ]; then
        attempt=$((KEEPALIVE_RETRY_ATTEMPT + 1))
        [ "$attempt" -le 64 ] || attempt=64
    else
        attempt=1
    fi
    case "$KEEPALIVE_FAILURE_KIND" in
        # Short: the pre-launch lock wait now holds a locked phone without
        # launching anything, so this only covers a lock that landed between
        # that check and the runner start. The old 30 s → 15 min backoff
        # kept a just-unlocked phone waiting minutes for its next attempt.
        locked) delay=5; cap=60 ;;
        # Only a different Xcode fixes this; each attempt would launch the
        # runner on the phone again for nothing.
        xcode_too_old) delay=900; cap=900 ;;
        *) delay=5; cap=300; KEEPALIVE_FAILURE_KIND="generic" ;;
    esac
    delay="$(_exponential_retry_delay "$delay" "$cap" "$attempt")"
    next_at=$(( $(date +%s) + delay ))
    tmp="$(mktemp "$STATE_DIR/wda-retry-state.v1.new.XXXXXX")" || return 1
    if printf 'version=1\nkind=%s\nattempt=%s\nnext_at=%s\n' \
        "$KEEPALIVE_FAILURE_KIND" "$attempt" "$next_at" > "$tmp" \
        && chmod 600 "$tmp" \
        && mv -f "$tmp" "$WDA_RETRY_STATE"; then
        if [ "$KEEPALIVE_FAILURE_KIND" = "locked" ]; then
            # `locked` is its own blocker: the phone is fine, the build is
            # fine, and the retry is already scheduled. Reporting `wda` here
            # made both the daemon hint and the web client tell the operator
            # to read logs and re-run setup for a state that clears by
            # unlocking the phone.
            _setstatus lock-backoff locked "lock screen blocked the device runner; next quiet retry in ${delay}s"
            if [ "$previous_kind" != "locked" ]; then
                warn "iPhone lock screen blocked the device runner; retrying quietly every 5s to 1min until it is unlocked"
            fi
        else
            warn "KeepAlive rebuild failed; next retry in ${delay}s (failure $attempt)"
        fi
        return 0
    fi
    rm -f "$tmp"
    return 1
}

_reset_keepalive_retry() {
    if [ -L "$WDA_RETRY_STATE" ]; then
        warn "refusing to remove symlinked KeepAlive retry state: $WDA_RETRY_STATE"
        return 1
    fi
    rm -f "$WDA_RETRY_STATE"
}

_prepare_locked_retry() {
    KEEPALIVE_FAILURE_KIND="locked"
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || true
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || true
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || true
}

# Wait (up to $2 seconds, default 3) until something accepts on loopback port
# $1. A relay binds within milliseconds of starting; the fixed `sleep 1` that
# used to precede each relay check was pure waiting on the reconnect path.
# Ownership is still proven afterwards by _verify_loopback_listener.
_wait_tcp_listening() {
    local port="$1" limit="${2:-3}" tick=0
    while [ "$tick" -lt $((limit * 20)) ]; do
        if ( : <>"/dev/tcp/127.0.0.1/$port" ) 2>/dev/null; then
            return 0
        fi
        tick=$((tick + 1))
        sleep 0.05
    done
    return 1
}

_interactive_lock_wait_tick() {
    local now elapsed delay
    now="${1:-$(date +%s)}"
    if [ "$INTERACTIVE_LOCK_STARTED_AT" -eq 0 ]; then
        INTERACTIVE_LOCK_STARTED_AT="$now"
        INTERACTIVE_LOCK_NOTICE_ATTEMPT=1
        delay="$(_exponential_retry_delay 30 120 "$INTERACTIVE_LOCK_NOTICE_ATTEMPT")"
        INTERACTIVE_LOCK_NOTICE_AT=$((now + delay))
        warn "phone is locked; waiting up to 5 minutes without repeating this prompt every poll (Ctrl-C to stop)"
    fi
    elapsed=$((now - INTERACTIVE_LOCK_STARTED_AT))
    if [ "$elapsed" -ge 300 ]; then
        _setstatus building-fail wda "phone remained locked for 5 minutes"
        return 1
    fi
    if [ "$now" -ge "$INTERACTIVE_LOCK_NOTICE_AT" ]; then
        warn "phone is still locked after ${elapsed}s; unlock it, or press Ctrl-C and rerun setup later"
        INTERACTIVE_LOCK_NOTICE_ATTEMPT=$((INTERACTIVE_LOCK_NOTICE_ATTEMPT + 1))
        delay="$(_exponential_retry_delay 30 120 "$INTERACTIVE_LOCK_NOTICE_ATTEMPT")"
        INTERACTIVE_LOCK_NOTICE_AT=$((now + delay))
    fi
    _setstatus building wda "phone is locked; interactive setup is waiting up to 5 minutes"
    return 0
}

STATUS_FILE="$STATE_DIR/wda-setup-status.json"
# BEGIN setup status protocol (also exercised without device access by tests).
# One owner per setup attempt. All writers, including EXIT and the heartbeat,
# compare run_id under the same lock before atomically replacing the JSON file.
_status_publish() {
    local mode="$1"; shift
    [ -n "${STATUS_RUN_ID:-}" ] || return 0
    local -a invoke=(python3)
    # The watcher must be a direct child of the setup shell, so it can detect
    # parent death without following/reaping unrelated processes.
    [ "$mode" != "watch" ] || invoke=(exec python3)
    "${invoke[@]}" - "$STATUS_FILE" "$mode" "$STATUS_RUN_ID" \
        "$STATUS_OWNER_PID" "$STATUS_OWNER_START" "$@" <<'PY_STATUS'
import fcntl
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import time

path = Path(sys.argv[1])
mode, run_id, pid_text, owner_start = sys.argv[2:6]
owner_pid = int(pid_text)
arguments = sys.argv[6:]

def owner_alive():
    try:
        output = subprocess.check_output(
            ["ps", "-p", pid_text, "-o", "lstart="], text=True,
            env={**os.environ, "LC_ALL": "C"}, timeout=2,
        )
        return output.strip() == owner_start
    except (OSError, subprocess.SubprocessError):
        return False

def publish(operation):
    flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(str(path) + ".lock", flags, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
            raise OSError("unsafe setup status lock")
        deadline = time.monotonic() + 2
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise OSError("setup status lock timed out")
                time.sleep(0.02)
        if path.is_symlink():
            raise OSError("refusing symlinked setup status")
        try:
            data = json.loads(path.read_text())
            if not isinstance(data, dict):
                data = {}
        except (FileNotFoundError, ValueError):
            data = {}
        now = int(time.time())
        if operation == "begin":
            blocker = data.get("blocked_on", "")
            data = {
                "schema_version": 1, "run_id": run_id,
                "owner_pid": owner_pid, "owner_start": owner_start,
                "phase": "starting", "phase_started_at": now,
                "blocked_on": blocker if blocker in {"warp", "proxy", "usb", "trust", "ddi", "automation_mode_disabled", "xcode_too_old", "wda"} else "",
                "message": "starting setup", "active": True, "terminal": False,
            }
        elif data.get("run_id") != run_id:
            return False  # a new attempt owns the shared file now
        elif operation == "phase":
            phase, blocked, message = arguments
            if phase != data.get("phase"):
                data["phase_started_at"] = now
            terminal = phase == "ready" or phase.endswith("-fail") or phase == "lock-backoff"
            data.update(phase=phase, blocked_on=blocked, message=message,
                        active=not terminal, terminal=terminal)
        elif operation == "heartbeat":
            if not data.get("active") or data.get("terminal"):
                return False
        elif operation in {"finish", "abandoned"}:
            if operation == "abandoned" and data.get("terminal"):
                return False
            code = int(arguments[0]) if operation == "finish" else 137
            previous = str(data.get("phase", "starting"))
            if operation == "abandoned":
                phase = "interrupted"
            elif code == 130:
                phase = "stopped"
            elif code:
                phase = previous if previous.endswith("-fail") else previous + "-fail"
            else:
                phase = "ready" if previous == "ready" else "completed"
            data.update(phase=phase, last_phase=previous, active=False,
                        terminal=True, exit_code=code, ended_at=now)
            # Keep the last blocker/message as diagnostics, even on failure.
        data.update(ts=now, heartbeat_ts=now)
        temporary_fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
        try:
            with os.fdopen(temporary_fd, "w") as output:
                json.dump(data, output, separators=(",", ":"))
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        return True
    finally:
        os.close(fd)

if mode == "watch":
    # No subprocess is spawned on each one-second parent check. The expensive
    # identity recheck and heartbeat happen only once per 15 seconds.
    next_heartbeat = time.monotonic() + 15
    while True:
        if os.getppid() != owner_pid:
            publish("abandoned")
            break
        if time.monotonic() >= next_heartbeat:
            if not owner_alive():
                publish("abandoned")
                break
            if not publish("heartbeat"):
                break
            next_heartbeat = time.monotonic() + 15
        time.sleep(1)
else:
    publish(mode)
PY_STATUS
}

_status_begin_run() {
    [ "$COMMAND" = "setup" ] || return 0
    STATUS_OWNER_PID="$$"
    STATUS_OWNER_START="$(LC_ALL=C ps -p "$$" -o lstart= | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
    [ -n "$STATUS_OWNER_START" ] || return 1
    STATUS_RUN_ID="$$-$(date +%s)-$RANDOM$RANDOM"
    _status_publish begin || return 1
    _status_publish watch >/dev/null 2>&1 &
    STATUS_HEARTBEAT_PID=$!
}

_status_finish_run() {
    local code="$1"
    [ -n "${STATUS_RUN_ID:-}" ] || return 0
    _status_publish finish "$code" || true
    # Only signal an unreaped job belonging to this shell, never a reused PID.
    if [ -n "${STATUS_HEARTBEAT_PID:-}" ] \
        && jobs -pr | grep -Fxq "$STATUS_HEARTBEAT_PID"; then
        kill "$STATUS_HEARTBEAT_PID" 2>/dev/null || true
    fi
    [ -z "${STATUS_HEARTBEAT_PID:-}" ] || wait "$STATUS_HEARTBEAT_PID" 2>/dev/null || true
    STATUS_HEARTBEAT_PID=""
}

# $1=phase  $2=blocked_on(empty=ok)  $3=human message.
_setstatus() {
    [ "$COMMAND" = "setup" ] || return 0
    _status_publish phase "$1" "${2:-}" "${3:-}" 2>/dev/null || true
}
# END setup status protocol.

_xml_escape() {
    printf '%s' "$1" \
        | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

_valid_team_id() {
    [ "${#1}" -eq 10 ] || return 1
    case "$1" in
        *[!ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789]*) return 1 ;;
        *) return 0 ;;
    esac
}

_valid_bundle_id() {
    [ -n "$1" ] || return 1
    printf '%s\n' "$1" \
        | LC_ALL=C grep -Eq '^[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]+([A-Za-z0-9-]*[A-Za-z0-9])?)+$'
}

_valid_port() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge 1 ] 2>/dev/null && [ "$1" -le 65535 ] 2>/dev/null
}

_marker_file_secure() {
    local marker="$1"
    local metadata owner mode
    [ ! -L "$marker" ] && [ -f "$marker" ] || return 1
    metadata="$(/usr/bin/stat -f '%u|%Lp' "$marker" 2>/dev/null)" || return 1
    owner="${metadata%%|*}"
    mode="${metadata#*|}"
    [ "$owner" = "$UID_NUM" ] && [ "$mode" = "600" ]
}

# Team IDs of the accounts signed in to Xcode, one per line (deduplicated).
XCODE_APP_STORE_URL="https://apps.apple.com/app/xcode/id497799835"

_xcode_account_teams() {
    defaults export com.apple.dt.Xcode - 2>/dev/null | python3 -c '
import plistlib, sys
try:
    prefs = plistlib.loads(sys.stdin.buffer.read())
except Exception:
    sys.exit(0)
teams = set()
for entries in (prefs.get("IDEProvisioningTeamByIdentifier") or {}).values():
    for team in entries if isinstance(entries, list) else []:
        team_id = team.get("teamID") if isinstance(team, dict) else None
        if isinstance(team_id, str) and team_id:
            teams.add(team_id)
print("\n".join(sorted(teams)))
' 2>/dev/null || true
}

# Best effort, interactive runs only: put Xcode in front so the person can add
# an account. Never from launchd or over SSH, where nobody sees the window.
_open_xcode_for_account() {
    [ "${WDA_KEEPALIVE:-0}" != "1" ] || return 0
    [ -t 1 ] || return 0
    [ -z "${SSH_CONNECTION:-}${SSH_TTY:-}" ] || return 0
    open -a Xcode >/dev/null 2>&1 || return 0
    info "Opened Xcode: Settings (⌘,) → Accounts → + → Apple ID"
}

_checklist_line() {  # _checklist_line 1|0 <item> [fix]
    if [ "$1" = 1 ]; then
        printf '  %s %s\n' "${GRN}✓${RST}" "$2"
    else
        printf '  %s %s — %s\n' "${RED}✗${RST}" "$2" "$3"
    fi
}

# "enabled", "disabled", or nothing when devicectl cannot tell.
_developer_mode_status() {
    local j
    j="$(mktemp "${TMPDIR:-/tmp}/iphone-use-devmode.XXXXXX")" || return 0
    if _devicectl_t 5 device info details --device "$1" -j "$j" >/dev/null 2>&1; then
        python3 -c '
import json, sys
try:
    props = json.load(open(sys.argv[1]))["result"]["deviceProperties"]
    print(props.get("developerModeStatus", ""))
except Exception:
    pass
' "$j" 2>/dev/null || true
    fi
    rm -f "$j"
}

# What a person has to do by hand before the first build, one plain fix per
# missing item. Only an interactive run shows it (launchd and the tests have
# no terminal); the detailed checks that follow stay the source of truth.
_first_run_checklist() {
    local missing=0 account=1 usb count devmode
    printf '\n%s\n' "${BOLD}Before the first build${RST}"
    if xcodebuild -version >/dev/null 2>&1; then
        _checklist_line 1 "Xcode is installed"
    else
        _checklist_line 0 "Xcode is installed" "get it from the App Store ($XCODE_APP_STORE_URL) and open it once"
        missing=1
    fi
    # Any source setup itself would sign with counts: an explicit or persisted
    # team (an App Store Connect API key setup has no Xcode account at all),
    # the team last picked in Xcode, or a signed-in account.
    if [ -z "${WDA_TEAM_ID:-}" ] \
        && ! _asc_signing_enabled \
        && [ -z "$(/usr/libexec/PlistBuddy -c 'Print :EnvironmentVariables:WDA_TEAM_ID' "$WDA_AGENT_PLIST" 2>/dev/null || true)" ] \
        && [ -z "$(defaults read com.apple.dt.Xcode IDEProvisioningTeamManagerLastSelectedTeamID 2>/dev/null || true)" ] \
        && [ -z "$(_xcode_account_teams)" ]; then
        account=0
    fi
    if [ "$account" = 1 ]; then
        _checklist_line 1 "Xcode is signed in to an Apple account"
    else
        _checklist_line 0 "Xcode is signed in to an Apple account" "Xcode → Settings → Accounts → + → Apple ID (a free one works)"
        missing=1
    fi
    if [ "$WDA_ALLOW_LAN" != "1" ]; then
        usb="$(_usb_udids)"
        count="$(printf '%s' "$usb" | wc -w | tr -d '[:space:]')"
        if [ "$count" = 0 ]; then
            _checklist_line 0 "iPhone connected over USB" "plug it in with a cable, unlock it, and tap Trust"
            missing=1
        else
            _checklist_line 1 "iPhone connected over USB"
            if [ "$count" = 1 ]; then
                devmode="$(_developer_mode_status "$usb")"
                case "$devmode" in
                    enabled) _checklist_line 1 "Developer Mode is on" ;;
                    disabled)
                        _checklist_line 0 "Developer Mode is on" "on the iPhone: Settings → Privacy & Security → Developer Mode → On, then let it restart"
                        missing=1
                        ;;
                esac
            fi
        fi
    fi
    printf '  %s\n\n' "Keep the iPhone unlocked and awake until setup finishes."
    if [ "$missing" = 1 ]; then
        [ "$account" = 1 ] || _open_xcode_for_account
        return 1
    fi
    return 0
}

# Resolve one signing identity for doctor, setup, and the persisted supervisor.
# A shared default bundle ID cannot work across Apple Developer teams, so a fresh
# install derives a legal, team-specific suffix only after the Team ID validates.
_resolve_signing_identity() {
    SIGNING_ERROR=""
    BUNDLE_ID_DERIVED=0
    TEAM_ID="${WDA_TEAM_ID:-$(defaults read com.apple.dt.Xcode IDEProvisioningTeamManagerLastSelectedTeamID 2>/dev/null || true)}"
    if [ -z "$TEAM_ID" ]; then
        # Signing in to Xcode lists the account teams, but only selecting one
        # in a project records the "last selected" key read above. One team
        # is unambiguous, so use it rather than send a first-time user into
        # Xcode project settings.
        local teams
        teams="$(_xcode_account_teams)"
        case "$(printf '%s' "$teams" | wc -w | tr -d '[:space:]')" in
            1) TEAM_ID="$teams" ;;
            0)
                SIGNING_ERROR="Xcode is not signed in to an Apple account. Open Xcode → Settings → Accounts, click + and add your Apple ID (a free Apple ID works), then rerun."
                return 1
                ;;
            *)
                SIGNING_ERROR="Xcode is signed in to several teams ($(printf '%s' "$teams" | tr '\n' ' ' | sed 's/ $//')); pick one: export WDA_TEAM_ID=<one of them>, then rerun."
                return 1
                ;;
        esac
    fi
    if ! _valid_team_id "$TEAM_ID"; then
        SIGNING_ERROR="Invalid WDA_TEAM_ID '$TEAM_ID'. Expected exactly 10 uppercase ASCII letters/digits, e.g. ABCD123456."
        return 1
    fi
    WDA_TEAM_ID="$TEAM_ID"
    if [ -z "$WDA_BUNDLE_ID" ]; then
        WDA_BUNDLE_ID="com.leeguoo.iphone-use.wda.$(printf '%s' "$TEAM_ID" \
            | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')"
        BUNDLE_ID_DERIVED=1
    fi
    if ! _valid_bundle_id "$WDA_BUNDLE_ID"; then
        SIGNING_ERROR="Invalid WDA_BUNDLE_ID '$WDA_BUNDLE_ID'. Use dot-separated ASCII letters, digits, dots, and hyphens only."
        return 1
    fi
    return 0
}

# BEGIN runner source helpers.
# The sources are built (and their build scripts run) as this user, so they
# must be this user's own files that nobody else can change.
RUNNER_SOURCE_ERROR=""
_runner_source_valid() {
    local dir meta
    RUNNER_SOURCE_ERROR=""
    case "$RUNNER_SRC" in
        /*) ;;
        *) RUNNER_SOURCE_ERROR="IPU_RUNNER_SRC must be an absolute path (got '$RUNNER_SRC')"; return 1 ;;
    esac
    case "$RUNNER_SRC" in
        *[[:space:]]*|*$'\n'*)
            RUNNER_SOURCE_ERROR="the runner source path must not contain whitespace: $RUNNER_SRC"
            return 1
            ;;
    esac
    if [ ! -f "$RUNNER_PROJECT/project.pbxproj" ]; then
        RUNNER_SOURCE_ERROR="device runner sources are missing: $RUNNER_PROJECT
   Rerun the installer (it lays them down at $RUNNER_DEFAULT_SRC), or set
   IPU_RUNNER_SRC=<repo>/runner when working from a checkout."
        return 1
    fi
    for dir in "$RUNNER_SRC" "$RUNNER_SRC/IPhoneUseRunner"; do
        if [ -L "$dir" ]; then
            RUNNER_SOURCE_ERROR="refusing a symlinked runner source directory: $dir"
            return 1
        fi
        meta="$(/usr/bin/stat -f '%u %Lp' "$dir" 2>/dev/null)" || {
            RUNNER_SOURCE_ERROR="cannot inspect $dir"
            return 1
        }
        if [ "${meta%% *}" != "$UID_NUM" ]; then
            RUNNER_SOURCE_ERROR="runner sources are not owned by this user: $dir"
            return 1
        fi
        case "${meta##* }" in
            *[2367][0-7]|*[0-7][2367])
                RUNNER_SOURCE_ERROR="runner sources are writable by other users: $dir"
                return 1
                ;;
        esac
    done
    return 0
}

# Content hash of the runner sources: every regular file under IPhoneUseRunner
# (Xcode's per-user state excluded), path and bytes, in sorted order. It keys
# the product cache, so an upgraded runner is never mistaken for the old build.
_runner_source_hash() {
    python3 - "$RUNNER_SRC/IPhoneUseRunner" <<'PY_RUNNER_HASH'
import hashlib
import os
import sys

root = sys.argv[1]
digest = hashlib.sha256()
entries = []
for directory, dirs, files in os.walk(root, followlinks=False):
    dirs[:] = sorted(d for d in dirs if d != "xcuserdata" and not d.startswith("."))
    for name in files:
        if name.startswith("."):
            continue
        full = os.path.join(directory, name)
        if os.path.islink(full) or not os.path.isfile(full):
            continue
        entries.append(os.path.relpath(full, root))
if not entries:
    raise SystemExit(1)
for relative in sorted(entries):
    digest.update(relative.encode() + b"\0")
    with open(os.path.join(root, relative), "rb") as handle:
        digest.update(hashlib.sha256(handle.read()).digest())
print(digest.hexdigest())
PY_RUNNER_HASH
}
# END runner source helpers.

_wait_job_gone() {
    local label="$1"
    local _
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 0
        sleep 0.5
    done
    ! launchctl print "$GUI_DOMAIN/$label" >/dev/null 2>&1
}

_job_is_disabled() {
    local state
    state="$(_job_disabled_state "$1")" || return 2
    [ "$state" = "1" ]
}

_job_disabled_state() {
    local label="$1"
    local disabled_output
    disabled_output="$(launchctl print-disabled "$GUI_DOMAIN" 2>/dev/null)" \
        || return 1
    if printf '%s\n' "$disabled_output" \
        | awk -v key="\"$label\"" \
            '$1 == key && $2 == "=>" && ($3 == "true" || $3 == "disabled") { found=1 } END { exit !found }'; then
        printf '1\n'
    else
        printf '0\n'
    fi
}

_restore_job_policy() {
    local label="$1"
    local expected_disabled="$2"
    local restored_disabled
    case "$expected_disabled" in
        0)
            launchctl enable "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 1
            ;;
        1)
            launchctl disable "$GUI_DOMAIN/$label" >/dev/null 2>&1 || return 1
            ;;
        *)
            return 1
            ;;
    esac
    restored_disabled="$(_job_disabled_state "$label")" || return 1
    [ "$restored_disabled" = "$expected_disabled" ]
}

_plist_set_env() {
    local plist="$1"
    local key="$2"
    local value="$3"
    if ! /usr/libexec/PlistBuddy \
        -c "Set :EnvironmentVariables:$key $value" "$plist" 2>/dev/null; then
        /usr/libexec/PlistBuddy \
            -c "Add :EnvironmentVariables:$key string $value" "$plist"
    fi
}

# Install the same dedicated runner supervisor used by POST /agent/mode. Keeping
# setup-wda.sh in its own launchd job means daemon restarts cannot reap the runner, and
# KeepAlive can rebuild after sleep/USB/CoreDevice failures.
_install_wda_supervisor() {
    local env_block=""
    local key value escaped
    local setup_xml log_xml

    if [ ! -x "$SELF_INSTALL" ]; then
        warn "fixed setup script is missing or not executable: $SELF_INSTALL"
        return 1
    fi

    for key in \
        WDA_KEEPALIVE PATH WDA_UDID WDA_TEAM_ID WDA_BUNDLE_ID \
        IPU_RUNNER_SRC WDA_PORT MJPEG_PORT WDA_ALLOW_LAN \
        WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID \
        PHONE_REMOTE_INSTANCE PHONE_REMOTE_STATE_DIR
    do
        case "$key" in
            # The default instance's supervisor stays byte-for-byte what it was.
            PHONE_REMOTE_INSTANCE)
                [ "${INSTANCE_NAME:-default}" != default ] || continue
                value="$INSTANCE_NAME"
                ;;
            PHONE_REMOTE_STATE_DIR) value="${PHONE_REMOTE_STATE_DIR:-}" ;;
            WDA_KEEPALIVE) value="1" ;;
            PATH) value="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:/usr/bin:/bin" ;;
            WDA_UDID) value="$WDA_UDID" ;;
            WDA_TEAM_ID) value="$TEAM_ID" ;;
            WDA_BUNDLE_ID) value="$WDA_BUNDLE_ID" ;;
            # Only a non-default source is persisted, so an install keeps
            # following the copy install.sh refreshes.
            IPU_RUNNER_SRC)
                [ "$RUNNER_SRC" != "$RUNNER_DEFAULT_SRC" ] || continue
                value="$RUNNER_SRC"
                ;;
            WDA_PORT) value="$WDA_PORT" ;;
            MJPEG_PORT) value="$MJPEG_PORT" ;;
            WDA_ALLOW_LAN) value="${WDA_ALLOW_LAN:-}" ;;
            WDA_ASC_KEY_PATH|WDA_ASC_KEY_ID|WDA_ASC_ISSUER_ID)
                _asc_signing_enabled || continue
                value="${!key}"
                ;;
        esac
        [ -n "$value" ] || continue
        escaped="$(_xml_escape "$value")"
        env_block="${env_block}        <key>${key}</key><string>${escaped}</string>
"
    done

    mkdir -p "$HOME/Library/LaunchAgents"
    setup_xml="$(_xml_escape "$SELF_INSTALL")"
    log_xml="$(_xml_escape "$WDA_AGENT_LOG")"
    WDA_AGENT_STAGED_PLIST="${WDA_AGENT_PLIST}.install.$$"
    if ! cat > "$WDA_AGENT_STAGED_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>${WDA_AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>/bin/bash</string><string>${setup_xml}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
${env_block}    </dict>
    <key>KeepAlive</key><true/>
    <!-- The script persists a 5s→10s→…300s retry schedule. Keep launchd's
         own floor at 5s so it does not flatten the first retry steps. -->
    <key>ThrottleInterval</key><integer>5</integer>
    <key>RunAtLoad</key><true/>
    <key>StandardOutPath</key><string>${log_xml}</string>
    <key>StandardErrorPath</key><string>${log_xml}</string>
</dict></plist>
PLIST
    then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "could not stage the runner supervisor plist"
        return 1
    fi
    if ! chmod 600 "$WDA_AGENT_STAGED_PLIST" \
        || ! plutil -lint "$WDA_AGENT_STAGED_PLIST" >/dev/null 2>&1; then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "generated runner supervisor plist is invalid"
        return 1
    fi
    if ! mv -f "$WDA_AGENT_STAGED_PLIST" "$WDA_AGENT_PLIST"; then
        rm -f "$WDA_AGENT_STAGED_PLIST"
        WDA_AGENT_STAGED_PLIST=""
        warn "could not atomically install the runner supervisor plist"
        return 1
    fi
    WDA_AGENT_STAGED_PLIST=""

    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    if ! _wait_job_gone "$WDA_AGENT_LABEL"; then
        warn "old runner supervisor did not finish stopping"
        return 1
    fi
    # A prior installer may have persistently disabled the supervisor label.
    # Clear that policy before bootstrap; enabling afterward is too late.
    launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    if ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" 2>/dev/null; then
        warn "could not bootstrap the runner supervisor: $WDA_AGENT_PLIST"
        return 1
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        ok "runner supervisor job loaded: $GUI_DOMAIN/$WDA_AGENT_LABEL"
        return 0
    fi
    warn "launchctl accepted the runner plist but the job is not visible"
    return 1
}

# UDIDs of iPhones physically on USB (usbmuxd — always present, no libimobiledevice
# needed, can't hang like devicectl).
_usb_udids() {
python3 - 2>/dev/null <<'PY'
import socket,struct,plistlib
try:
    s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.settimeout(3);s.connect("/var/run/usbmuxd")
    p=plistlib.dumps({"MessageType":"ListDevices","ClientVersionString":"x","ProgName":"x"})
    s.sendall(struct.pack("<IIII",len(p)+16,1,8,1)+p)
    h=s.recv(16);ln=struct.unpack("<I",h[:4])[0];d=b""
    while len(d)<ln-16:d+=s.recv(ln-16-len(d))
    print(" ".join(sorted({x["Properties"]["SerialNumber"] for x in plistlib.loads(d).get("DeviceList",[]) if x["Properties"].get("ConnectionType")=="USB"})))
except Exception: pass
PY
}

# The iphone-use binary that serves the USB relays: the daemon this setup
# configures (an instance runs its own runtime copy), then the standard app
# locations. It must understand `relay` (older releases do not) and sit
# on a path without spaces, because the PID record matches its exact argv.
_relay_binary() {
    local candidate program
    program=""
    if [ -f "$DAEMON_PLIST" ]; then
        program="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$DAEMON_PLIST" 2>/dev/null || true)"
    fi
    for candidate in "${IPHONE_USE_RELAY_BIN:-}" "$program" \
        "$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use" \
        "/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"; do
        [ -n "$candidate" ] || continue
        case "$candidate" in
            /*/iphone-use) ;;
            *) continue ;;
        esac
        case "$candidate" in
            *[[:space:]]*) continue ;;
        esac
        [ -x "$candidate" ] || continue
        "$candidate" relay --help >/dev/null 2>&1 || continue
        printf '%s\n' "$candidate"
        return 0
    done
    return 1
}

# The iphone-use binary that reads device facts from lockdownd over usbmuxd
# (`iphone-use device info|ddi`), in place of a devicectl process per read:
# ~15-50 ms over USB against ~0.2-0.3 s, and no CoreDevice round trip. Same
# candidates as the relay binary; resolved once per run into DEVICE_TOOL_BIN
# (call it directly, not in $(...), or the result is lost with the subshell).
# An older binary without `device` leaves the devicectl paths in charge.
DEVICE_TOOL_RESOLVED=0
DEVICE_TOOL_BIN=""
_device_tool() {
    local candidate program
    if [ "$DEVICE_TOOL_RESOLVED" = "1" ]; then
        [ -n "$DEVICE_TOOL_BIN" ]
        return
    fi
    DEVICE_TOOL_RESOLVED=1
    program=""
    if [ -f "$DAEMON_PLIST" ]; then
        program="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$DAEMON_PLIST" 2>/dev/null || true)"
    fi
    for candidate in "${IPHONE_USE_RELAY_BIN:-}" "$program" \
        "$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use" \
        "/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"; do
        [ -n "$candidate" ] && [ -x "$candidate" ] || continue
        "$candidate" device info --help >/dev/null 2>&1 || continue
        DEVICE_TOOL_BIN="$candidate"
        return 0
    done
    return 1
}

# Run `iphone-use device <query>` for the target into DEVICE_QUERY_JSON.
# Exit status: 0 read, 3 the phone is not attached to usbmuxd, 1 otherwise.
DEVICE_QUERY_JSON=""
_device_query() {
    local query="$1"
    DEVICE_QUERY_JSON=""
    [ -n "${WDA_UDID:-}" ] || return 1
    _device_tool || return 1
    DEVICE_QUERY_JSON="$("$DEVICE_TOOL_BIN" device "$query" --udid "$WDA_UDID" 2>/dev/null)"
}

# A string or boolean field from DEVICE_QUERY_JSON (one flat JSON object).
_device_field() {
    printf '%s\n' "$DEVICE_QUERY_JSON" \
        | sed -nE "s/.*\"$1\":(\"([^\"]*)\"|(true|false)).*/\\2\\3/p" | head -1
}

_target_on_usb() {
    [ -n "${WDA_UDID:-}" ] || return 1
    case " $(_usb_udids) " in
        *" $WDA_UDID "*) return 0 ;;
        *) return 1 ;;
    esac
}

# WARP breaks the CoreDevice tunnel xcodebuild needs to install AND keep WDA
# alive (hardware-verified: the runner dies "connection was invalidated" the
# moment WARP reconnects). This was the #1 cause of the whole "Device is busy /
# Waiting for developer services" nightmare. Detect it up front.
PREVIOUS_SUPERVISOR_LOADED=0
PREVIOUS_SUPERVISOR_PLIST_PRESENT=0
PREVIOUS_SUPERVISOR_DISABLED=0
SUPERVISOR_TRANSACTION_ACTIVE=0
SUPERVISOR_HANDOFF_COMPLETE=0
DAEMON_TRANSACTION_ACTIVE=0
# Set once this run installs a changed daemon plist or reloads the daemon job.
# Until then the rollback has nothing to undo and must leave the daemon alone.
DAEMON_TOUCHED=0
DAEMON_JOB_WAS_LOADED=0
DAEMON_WAS_DISABLED=0
STARTED_RUNNER=0
STARTED_CONTROL_RELAY=0
STARTED_MJPEG_RELAY=0
WDA_AGENT_STAGED_PLIST=""
DAEMON_STAGED_PLIST=""
# -w (whole word): plain "Connected" also matches "Dis-Connected" as a substring,
# which mis-read WARP-off as WARP-on and blocked WDA for no reason (user-reported).
_warp_cli() {
    if [ -n "${IPHONE_USE_INTERNAL_TEST_WARP_CLI:-}" ]; then
        printf '%s\n' "$IPHONE_USE_INTERNAL_TEST_WARP_CLI"
    else
        command -v warp-cli 2>/dev/null
    fi
}
_warp_on() {
    local cli
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] \
        && "$cli" status 2>/dev/null | grep -qiw "Connected"
}

_warp_mode() {
    local cli
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] || return 1
    "$cli" settings 2>/dev/null | awk '
        match($0, /Mode:[[:space:]]*/) {
            value = substr($0, RSTART + RLENGTH)
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", value)
            print value
            exit
        }
    '
}

# Local proxy mode only tunnels HTTP(S) requests explicitly sent to the
# loopback proxy. It does not install catch-all routes, so CoreDevice traffic
# cannot be captured even while the client reports Connected. It is route-safe,
# although Cloudflare's request timeout can make it unsuitable for long uploads.
_warp_local_proxy_mode() {
    case "$(_warp_mode 2>/dev/null | tr '[:upper:]' '[:lower:]')" in
        proxy|localproxy|"local proxy"|warpproxy*) return 0 ;;
        *) return 1 ;;
    esac
}

# CoreDevice creates an RSD tunnel even for a USB-connected iPhone. The
# device-facing interface uses IPv6 link-local plus a dynamic ULA /64 (observed
# as fd2c:.../64); if WARP captures either range, devicectl hangs and a live
# xcodebuild/WDA session is invalidated. Trust the client's effective route
# dump, not only the shorter policy summary shown by `warp-cli settings`.
_warp_coredevice_bypass_ready() {
    local cli excluded
    cli="$(_warp_cli)" || return 1
    [ -n "$cli" ] || return 1
    excluded="$("$cli" tunnel dump 2>/dev/null | awk '
        /^Excluded:[[:space:]]*$/ { inside = 1; next }
        inside && /^[A-Za-z][A-Za-z ]*:[[:space:]]*$/ { exit }
        inside {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "")
            if (length($0)) print
        }
    ')" || return 1
    printf '%s\n' "$excluded" | grep -Fxq "fe80::/10" || return 1
    if printf '%s\n' "$excluded" | grep -Fxq "fd00::/8"; then
        return 0
    fi
    # A broader ULA exclusion is also sufficient.
    printf '%s\n' "$excluded" | grep -Fxq "fc00::/7"
}

WARP_PREFLIGHT_ERROR=""
_warp_preflight() {
    WARP_PREFLIGHT_ERROR=""
    _warp_on || return 0
    _warp_local_proxy_mode && return 0
    if _warp_coredevice_bypass_ready; then
        return 0
    fi
    WARP_PREFLIGHT_ERROR="WARP is connected, but its effective Split Tunnel exclusions do not cover the CoreDevice device tunnel.
   If WARP is only needed for specific destinations, prefer a Traffic only
   device profile with Split Tunnels in Include mode and only those destination
   IPs/CIDRs. (Local proxy mode is also route-safe, but its request timeout can
   be unsuitable for long Git uploads.)
   Otherwise add BOTH routes to the device profile's Exclude list:
     - fe80::/10  (IPv6 link-local)
     - fd00::/8   (CoreDevice RSD ULA)
   Then wait for policy propagation/reconnect and verify with:
     warp-cli tunnel dump
   Temporary alternative: warp-cli disconnect
   The script did not change WARP or organization policy."
    return 1
}

_warp_ready_summary() {
    if _warp_local_proxy_mode; then
        printf '%s\n' "WARP: connected in Local proxy mode; only explicitly proxied traffic is tunneled"
    else
        printf '%s\n' "WARP: connected with CoreDevice Split Tunnel exclusions (fe80::/10 + fd00::/8)"
    fi
}

# Read the top-level fixed proxy entries from the current System Configuration
# snapshot reported by `scutil --proxy`. Nested scoped/interface dictionaries
# are separate configurations and must not overwrite the global active values.
# Output is protocol|host|port, one enabled entry per line.
# macOS omits the HTTPEnable/HTTPSEnable/SOCKSEnable keys entirely on a network
# service whose proxies were never configured, so a well-formed dictionary
# without them means "none enabled", not "unreadable" (issue #91).
_system_proxy_entries() {
    local scutil_bin="${IPHONE_USE_INTERNAL_TEST_SCUTIL:-/usr/sbin/scutil}"
    local snapshot
    [ -x "$scutil_bin" ] || return 1
    snapshot="$("$scutil_bin" --proxy 2>/dev/null)" || return 1
    printf '%s\n' "$snapshot" | awk '
        {
            before = depth
            braces = $0
            opens = gsub(/{/, "{", braces)
            closes = gsub(/}/, "}", braces)
            if (before == 0 && opens > 0 && $1 == "<dictionary>") {
                root_seen = 1
            }
            if (before == 1 && $2 == ":" &&
                $1 ~ /^(HTTP|HTTPS|SOCKS)(Enable|Proxy|Port)$/) {
                value[$1] = $3
            }
            depth += opens - closes
            if (depth < 0) {
                invalid = 1
            }
        }
        END {
            if (!root_seen || depth != 0 || invalid) {
                exit 65
            }
            protocols[1] = "HTTP"
            protocols[2] = "HTTPS"
            protocols[3] = "SOCKS"
            for (i = 1; i <= 3; i++) {
                protocol = protocols[i]
                if (value[protocol "Enable"] == "1") {
                    printf "%s|%s|%s\n", protocol,
                        value[protocol "Proxy"], value[protocol "Port"]
                }
            }
        }
    '
}

_valid_proxy_host() {
    [ -n "$1" ] \
        && printf '%s\n' "$1" | LC_ALL=C grep -Eq '^[A-Za-z0-9._:%-]+$'
}

_proxy_is_loopback() {
    local host
    host="$(printf '%s' "$1" | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')"
    case "$host" in
        localhost|localhost.|::1|0:0:0:0:0:0:0:1) return 0 ;;
    esac
    printf '%s\n' "$host" | awk -F. '
        NF != 4 || $1 != "127" { exit 1 }
        {
            for (i = 2; i <= 4; i++) {
                if ($i !~ /^[0-9]+$/ || $i > 255) {
                    exit 1
                }
            }
        }
    '
}

# A successful TCP connect proves only that the configured local endpoint
# exists. It deliberately does not claim that the listener speaks the selected
# proxy protocol or that CoreDevice supports the proxy.
_proxy_tcp_reachable() {
    local host="$1"
    local port="$2"
    local probe="${IPHONE_USE_INTERNAL_TEST_PROXY_PROBE:-}"
    if [ -n "$probe" ]; then
        [ -x "$probe" ] || return 2
        "$probe" "$host" "$port" >/dev/null 2>&1
        return $?
    fi
    if [ -x /usr/bin/nc ]; then
        /usr/bin/nc -z -w 1 "$host" "$port" >/dev/null 2>&1
        return $?
    fi
    return 2
}

# Detect active macOS HTTP/HTTPS/SOCKS settings without changing them. A
# reachable proxy is only reported as a diagnostic variable. We fail closed
# only for a malformed enabled entry or a loopback endpoint with no listener:
# those are concrete local configuration faults and the latter reproduced the
# CoreDevice/DDI failure that motivated this check.
SYSTEM_PROXY_ERROR=""
_system_proxy_check() {
    local entries
    local protocol host port probe_status
    local active=0
    local invalid=""
    local dead=""
    SYSTEM_PROXY_ERROR=""

    if ! entries="$(_system_proxy_entries)"; then
        SYSTEM_PROXY_ERROR="Could not inspect macOS HTTP/HTTPS/SOCKS proxy state with /usr/sbin/scutil.
   The script did not change any proxy settings. Run '/usr/sbin/scutil --proxy'
   to repair System Configuration access, then rerun setup."
        return 1
    fi
    if [ -z "$entries" ]; then
        ok "System proxies (HTTP/HTTPS/SOCKS): none enabled"
        return 0
    fi

    while IFS='|' read -r protocol host port; do
        [ -n "$protocol" ] || continue
        active=1
        if ! _valid_proxy_host "$host" || ! _valid_port "$port"; then
            invalid="${invalid}${invalid:+, }$protocol"
            continue
        fi
        if _proxy_is_loopback "$host"; then
            if _proxy_tcp_reachable "$host" "$port"; then
                warn "~ $protocol system proxy enabled at $host:$port (TCP listener responds)"
            else
                probe_status=$?
                if [ "$probe_status" = "1" ]; then
                    dead="${dead}${dead:+, }$protocol $host:$port"
                else
                    warn "~ $protocol system proxy enabled at $host:$port (local endpoint could not be probed)"
                fi
            fi
        else
            warn "~ $protocol system proxy enabled at $host:$port (endpoint not probed)"
        fi
    done <<EOF
$entries
EOF

    if [ -n "$invalid" ] || [ -n "$dead" ]; then
        SYSTEM_PROXY_ERROR="macOS has an enabled but unusable system proxy configuration."
        if [ -n "$invalid" ]; then
            SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   Invalid or incomplete entries: $invalid"
        fi
        if [ -n "$dead" ]; then
            SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   No reachable TCP listener at configured loopback endpoints: $dead"
        fi
        SYSTEM_PROXY_ERROR="$SYSTEM_PROXY_ERROR
   A stale system proxy can prevent Xcode/CoreDevice from reaching developer services;
   this check does not claim that the iPhone or DDI is defective.
   Fix ONE of:
     - restart the proxy app so the listed local endpoint is listening
     - disable only the stale protocols in System Settings -> Network -> active service
       -> Details -> Proxies
   The script did not change proxy settings. Verify with '/usr/sbin/scutil --proxy',
   then rerun setup."
        return 1
    fi

    if [ "$active" = "1" ]; then
        warn "~ Active system proxies are not automatically treated as a blocker. If CoreDevice/DDI stalls, retry after bypassing or disabling them."
    fi
    return 0
}

if [ "${IPHONE_USE_INTERNAL_TEST_PROXY_PREFLIGHT_ONLY:-0}" = "1" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal proxy preflight fixture requires the read-only doctor command"
    if _system_proxy_check; then
        exit 0
    fi
    warn "X $SYSTEM_PROXY_ERROR"
    exit 1
fi

_restore_backup_file() {
    local backup="$1"
    local target="$2"
    local mode="$3"
    local restore_tmp="${target}.restore.$$"
    [ -f "$backup" ] || return 1
    if cp -p "$backup" "$restore_tmp" 2>/dev/null \
        && chmod "$mode" "$restore_tmp" 2>/dev/null \
        && mv -f "$restore_tmp" "$target" 2>/dev/null \
        && cmp -s "$backup" "$target"; then
        return 0
    fi
    rm -f "$restore_tmp"
    return 1
}

_cleanup_on_exit() {
    local status=$?
    local cleanup_failed=0
    local self_restore_ok=1
    local supervisor_restore_ok=1
    local daemon_restore_ok=1
    set +e
    if [ "$status" -ne 0 ]; then
        if [ "${STARTED_MJPEG_RELAY:-0}" = "1" ]; then
            _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg \
                || cleanup_failed=1
        fi
        if [ "${STARTED_CONTROL_RELAY:-0}" = "1" ]; then
            _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay \
                || cleanup_failed=1
        fi
        if [ "${STARTED_RUNNER:-0}" = "1" ]; then
            _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
                || cleanup_failed=1
        fi
    fi
    if [ "$status" -ne 0 ] && [ "$SELF_INSTALL_REPLACED_THIS_RUN" = "1" ]; then
        if [ "$SELF_INSTALL_HAD_PREVIOUS" = "1" ]; then
            if _restore_backup_file "$SELF_INSTALL_ROLLBACK" "$SELF_INSTALL" 700; then
                rm -f "$SELF_INSTALL_ROLLBACK"
            else
                self_restore_ok=0
                cleanup_failed=1
                warn "could not restore the prior setup script; rescue backup retained at:
   $SELF_INSTALL_ROLLBACK"
            fi
        else
            rm -f "$SELF_INSTALL"
            if [ -e "$SELF_INSTALL" ]; then
                self_restore_ok=0
                cleanup_failed=1
                warn "could not remove the newly installed setup script: $SELF_INSTALL"
            fi
        fi
    fi
    if [ "$status" -ne 0 ] \
        && [ "$SUPERVISOR_TRANSACTION_ACTIVE" = "1" ] \
        && [ "$SUPERVISOR_HANDOFF_COMPLETE" != "1" ]; then
        warn "setup failed — restoring the prior runner supervisor file and loaded state"
        launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        if ! _wait_job_gone "$WDA_AGENT_LABEL"; then
            supervisor_restore_ok=0
            cleanup_failed=1
            warn "new runner supervisor did not fully stop during rollback"
        fi
        if [ "$PREVIOUS_SUPERVISOR_PLIST_PRESENT" = "1" ]; then
            if ! _restore_backup_file "$WDA_AGENT_ROLLBACK_PLIST" \
                "$WDA_AGENT_PLIST" 600; then
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "could not restore the prior supervisor plist; rescue backup retained at:
   $WDA_AGENT_ROLLBACK_PLIST"
            fi
        else
            rm -f "$WDA_AGENT_PLIST"
            if [ -e "$WDA_AGENT_PLIST" ]; then
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "could not remove the newly created supervisor plist: $WDA_AGENT_PLIST"
            fi
        fi
        if [ "$PREVIOUS_SUPERVISOR_LOADED" = "1" ]; then
            if [ "$supervisor_restore_ok" = "1" ] \
                && [ "$self_restore_ok" = "1" ] \
                && plutil -lint "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
                launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
                if ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" >/dev/null 2>&1 \
                    || ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
                    supervisor_restore_ok=0
                    cleanup_failed=1
                    warn "prior runner supervisor plist was restored, but its loaded state was not"
                fi
            else
                supervisor_restore_ok=0
                cleanup_failed=1
                warn "prior runner supervisor was not restarted because its files were not fully restored"
            fi
        fi
        _restore_job_policy "$WDA_AGENT_LABEL" "$PREVIOUS_SUPERVISOR_DISABLED" \
            || supervisor_restore_ok=0
        if [ "$supervisor_restore_ok" = "1" ]; then
            rm -f "$WDA_AGENT_ROLLBACK_PLIST"
        else
            cleanup_failed=1
            [ -f "$WDA_AGENT_ROLLBACK_PLIST" ] \
                && warn "supervisor rescue backup retained at: $WDA_AGENT_ROLLBACK_PLIST"
        fi
    fi
    # A round that found the daemon already configured changed nothing, so
    # there is nothing to restore. Reloading it anyway killed the daemon that
    # had just asked for the stop: a `{"mode":"human"}` sent while a reconnect
    # round was still finishing lost its connection and the daemon restarted.
    if [ "$status" -ne 0 ] && [ "$DAEMON_TRANSACTION_ACTIVE" = "1" ] \
        && [ "$DAEMON_TOUCHED" != "1" ]; then
        rm -f "$DAEMON_ROLLBACK_PLIST"
    elif [ "$status" -ne 0 ] && [ "$DAEMON_TRANSACTION_ACTIVE" = "1" ]; then
        warn "setup failed — restoring the prior daemon configuration and loaded state"
        launchctl bootout "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1 || true
        if ! _wait_job_gone "$DAEMON_LABEL"; then
            daemon_restore_ok=0
            cleanup_failed=1
            warn "new daemon job did not fully stop during rollback"
        fi
        if ! _restore_backup_file "$DAEMON_ROLLBACK_PLIST" "$DAEMON_PLIST" 600; then
            daemon_restore_ok=0
            cleanup_failed=1
            warn "could not restore the prior daemon plist; rescue backup retained at:
   $DAEMON_ROLLBACK_PLIST"
        fi
        if [ "$DAEMON_JOB_WAS_LOADED" = "1" ]; then
            if [ "$daemon_restore_ok" = "1" ] \
                && plutil -lint "$DAEMON_PLIST" >/dev/null 2>&1; then
                launchctl enable "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1 || true
                if ! launchctl bootstrap "$GUI_DOMAIN" "$DAEMON_PLIST" >/dev/null 2>&1 \
                    || ! launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
                    daemon_restore_ok=0
                    cleanup_failed=1
                    warn "prior daemon plist was restored, but its loaded state was not"
                fi
            else
                daemon_restore_ok=0
                cleanup_failed=1
            fi
        fi
        _restore_job_policy "$DAEMON_LABEL" "$DAEMON_WAS_DISABLED" \
            || daemon_restore_ok=0
        if [ "$daemon_restore_ok" = "1" ]; then
            rm -f "$DAEMON_ROLLBACK_PLIST"
        else
            cleanup_failed=1
            [ -f "$DAEMON_ROLLBACK_PLIST" ] \
                && warn "daemon rescue backup retained at: $DAEMON_ROLLBACK_PLIST"
        fi
    fi
    [ -z "${WDA_AGENT_STAGED_PLIST:-}" ] || rm -f "$WDA_AGENT_STAGED_PLIST"
    [ -z "${DAEMON_STAGED_PLIST:-}" ] || rm -f "$DAEMON_STAGED_PLIST"
    if [ "$status" -ne 0 ] && [ "$status" -ne 130 ] \
        && [ "${WDA_KEEPALIVE:-0}" = "1" ] \
        && [ "${KEEPALIVE_ATTEMPT_ACTIVE:-0}" = "1" ]; then
        KEEPALIVE_ATTEMPT_ACTIVE=0
        _record_keepalive_failure || cleanup_failed=1
    fi
    [ "$cleanup_failed" = "0" ] || status=1
    _status_finish_run "$status"
    trap - EXIT
    exit "$status"
}
trap _cleanup_on_exit EXIT
trap 'exit 130' INT TERM

if [ -n "${IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND:-}" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal KeepAlive retry fixture requires the read-only doctor command"
    case "$IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND" in
        generic|locked|xcode_too_old)
            KEEPALIVE_FAILURE_KIND="$IPHONE_USE_INTERNAL_TEST_KEEPALIVE_RETRY_KIND"
            _record_keepalive_failure
            ;;
        reset)
            _reset_keepalive_retry
            ;;
        *)
            die "invalid internal KeepAlive retry fixture"
            ;;
    esac
    exit $?
fi

_warp_check() {
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        fi
        return 0
    fi
    _setstatus prereq warp "WARP is connected and breaks CoreDevice"
    die "$WARP_PREFLIGHT_ERROR
   WARP would otherwise invalidate the just-verified runner session and create a restart loop.
   See docs/wda-setup.html pitfall (WARP)."
}

if [ "${IPHONE_USE_INTERNAL_TEST_WARP_PREFLIGHT_ONLY:-0}" = "1" ]; then
    [ "$COMMAND" = "doctor" ] \
        || die "internal WARP preflight fixture requires the read-only doctor command"
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        else
            ok "WARP: off / not present"
        fi
        exit 0
    fi
    warn "X $WARP_PREFLIGHT_ERROR"
    exit 1
fi

# BEGIN Xcode compatibility helpers.
# A future Xcode can drop iOS deployment targets the runner project still names
# (Xcode 27 did exactly that to WebDriverAgent's 12.0), and `xcodebuild` then
# refuses to build at all. The runner argv is an identity checked by
# `_runner_signature_valid` and `_command_matches_expected`, so the fix must not
# add build settings to the command line. Instead setup writes an xcconfig in
# the state directory and exports XCODE_XCCONFIG_FILE, which every xcodebuild
# this script starts (and the runner it leaves behind) inherits. The runner
# sources are not edited.
WDA_XCCONFIG_FILE="$STATE_DIR/wda-xcode-compat.xcconfig"
WDA_DEPLOYMENT_TARGET_OVERRIDE=""
WDA_IOS_SDK_VERSION=""

_valid_os_version() {
    printf '%s\n' "${1:-}" | LC_ALL=C grep -Eq '^[0-9]+(\.[0-9]+){0,2}$'
}

# Succeeds when dotted version $1 is strictly lower than $2.
_version_lt() {
    local a="$1" b="$2" x y i
    local -a pa pb
    IFS=. read -r -a pa <<< "$a"
    IFS=. read -r -a pb <<< "$b"
    for i in 0 1 2; do
        x="${pa[$i]:-0}"
        y="${pb[$i]:-0}"
        [ "$((10#$x))" -lt "$((10#$y))" ] && return 0
        [ "$((10#$x))" -gt "$((10#$y))" ] && return 1
    done
    return 1
}

# Major version of the selected Xcode ("Xcode 27.0" -> 27), or nothing.
# "Xcode 27.0", cached per selected developer directory. `xcodebuild -version`
# costs ~0.4 s and ran on every reconnect; the cache key is the developer
# directory plus the mtime of its version.plist, so switching or updating
# Xcode re-reads it. Any read failure falls back to asking xcodebuild.
_xcode_version_cached() {
    local xcodebuild="$1" developer stamp cache line version
    developer="$(xcode-select -p 2>/dev/null || true)"
    stamp=""
    if [ -n "$developer" ] && [ -f "$developer/../version.plist" ]; then
        stamp="$developer|$(stat -f '%m' "$developer/../version.plist" 2>/dev/null || true)"
    fi
    cache="$STATE_DIR/.xcode-version.cache"
    if [ -n "$stamp" ] && [ -f "$cache" ] && [ ! -L "$cache" ]; then
        line="$(head -1 "$cache" 2>/dev/null || true)"
        if [ "${line%%	*}" = "$stamp" ]; then
            version="${line#*	}"
            case "$version" in
                "Xcode "[0-9]*) printf '%s\n' "$version"; return 0 ;;
            esac
        fi
    fi
    version="$("$xcodebuild" -version 2>/dev/null | head -1 || true)"
    if [ -n "$stamp" ] && [ -n "$version" ] && [ ! -L "$cache" ]; then
        printf '%s\t%s\n' "$stamp" "$version" > "$cache" 2>/dev/null || true
    fi
    printf '%s\n' "$version"
}

_xcode_major() {
    xcodebuild -version 2>/dev/null \
        | sed -n '1s/^Xcode \([0-9][0-9]*\).*/\1/p'
}

_ios_sdk_version() {
    local version
    version="$(xcrun --sdk iphoneos --show-sdk-version 2>/dev/null || true)"
    _valid_os_version "$version" && printf '%s\n' "$version"
}

# The phone's iOS version ("27.2"), read from lockdownd (`iphone-use device
# info`) or devicectl's JSON, or nothing. Failures are tolerated: the version
# only sharpens a diagnosis and never gates setup on its own.
_device_ios_version() {
    local udid="${1:-${WDA_UDID:-}}" j version
    [ -n "$udid" ] || return 0
    if WDA_UDID="$udid" _device_query info; then
        version="$(_device_field product_version)"
        if _valid_os_version "$version"; then
            printf '%s\n' "$version"
            return 0
        fi
    fi
    j="$(mktemp "${TMPDIR:-/tmp}/iphone-use-device-details.XXXXXX")" || return 0
    _devicectl_t 10 device info details --device "$udid" -j "$j" >/dev/null 2>&1 || true
    version="$(sed -nE 's/.*"osVersionNumber"[[:space:]]*:[[:space:]]*"([0-9.]+)".*/\1/p' "$j" 2>/dev/null | head -1)"
    rm -f "$j"
    if _valid_os_version "$version"; then
        printf '%s\n' "$version"
    fi
    return 0
}

# "27.2.1" -> "27.2", "27" -> "27.0".
_os_major_minor() {
    printf '%s\n' "$1" | awk -F. '{ printf "%d.%d\n", $1, $2 }'
}

# testmanagerd refuses the IDE channel and the runner exits with code 74,
# sometimes without the "refused channel" line (#126).
_runner_log_shows_ide_refusal() {
    grep -Eq 'with code 74([^0-9]|$)|XCTestManager_IDEInterface|before establishing connection' \
        "${1:-$RUN_LOG}" 2>/dev/null
}

# Succeeds, with XCODE_TOO_OLD_MESSAGE set, when the runner log carries the
# refusal signature AND the phone's iOS major.minor is newer than the SDK's.
# The version gap alone is not enough to fail: Xcode usually drives an iOS one
# minor release ahead of its SDK (Xcode 15.4 / SDK 17.5 runs iOS 17.6), so a
# version-only preflight would block setups that work today. With matching
# versions the refusal keeps its existing classification.
XCODE_TOO_OLD_MESSAGE=""
_runner_failure_is_xcode_too_old() {
    local log="${1:-$RUN_LOG}" device sdk
    XCODE_TOO_OLD_MESSAGE=""
    _runner_log_shows_ide_refusal "$log" || return 1
    sdk="$(_ios_sdk_version || true)"
    device="$(_device_ios_version || true)"
    [ -n "$sdk" ] && [ -n "$device" ] || return 1
    sdk="$(_os_major_minor "$sdk")"
    device="$(_os_major_minor "$device")"
    _version_lt "$sdk" "$device" || return 1
    XCODE_TOO_OLD_MESSAGE="iPhone runs iOS $device but this Xcode's SDK is iOS $sdk — install an Xcode that supports iOS $device (a beta Xcode for a beta iOS)"
    return 0
}

# Every retry launches an app on the phone and cannot succeed until Xcode is
# replaced, so KeepAlive waits its longest backoff (see _record_keepalive_failure).
_report_xcode_too_old() {
    _setstatus building-fail xcode_too_old "$XCODE_TOO_OLD_MESSAGE"
    KEEPALIVE_FAILURE_KIND="xcode_too_old"
    die "$XCODE_TOO_OLD_MESSAGE. Retrying or reconnecting cannot fix this; KeepAlive waits 15 minutes between attempts. Log: $RUN_LOG"
}

# Lowest iOS deployment target the selected iPhoneOS SDK accepts, read from
# the SDK's own SDKSettings. Xcode 27 is known to require 15.0, so fall back to
# that when the settings cannot be read there; older Xcodes keep the project's
# own value (no override) when nothing can be read.
_ios_sdk_min_deployment_target() {
    local sdk minimum="" major
    sdk="$(xcrun --sdk iphoneos --show-sdk-path 2>/dev/null || true)"
    if [ -n "$sdk" ] && [ -f "$sdk/SDKSettings.plist" ]; then
        minimum="$(plutil -extract SupportedTargets.iphoneos.MinimumDeploymentTarget \
            raw -o - "$sdk/SDKSettings.plist" 2>/dev/null || true)"
    fi
    if ! _valid_os_version "$minimum"; then
        minimum=""
        major="$(_xcode_major)"
        if [ -n "$major" ] && [ "$major" -ge 27 ]; then
            minimum="15.0"
        fi
    fi
    [ -n "$minimum" ] && printf '%s\n' "$minimum"
}

# Prints "<lowest> <highest>" IPHONEOS_DEPLOYMENT_TARGET in the runner project
# ($1 is the .xcodeproj).
_wda_project_deployment_targets() {
    local pbxproj="$1/project.pbxproj" value low="" high=""
    [ -f "$pbxproj" ] || return 1
    while IFS= read -r value; do
        _valid_os_version "$value" || continue
        if [ -z "$low" ] || _version_lt "$value" "$low"; then low="$value"; fi
        if [ -z "$high" ] || _version_lt "$high" "$value"; then high="$value"; fi
    done < <(sed -n 's/.*IPHONEOS_DEPLOYMENT_TARGET = "\{0,1\}\([0-9.]*\)"\{0,1\};.*/\1/p' "$pbxproj")
    [ -n "$low" ] || return 1
    printf '%s %s\n' "$low" "$high"
}

# Decide the deployment target the build needs: nothing when every target in
# the project is already supported by this Xcode, else max(highest project
# value, SDK minimum). The highest value is used because the xcconfig applies
# to every target; raising a target is safe, lowering one below what its
# author chose is not.
_wda_required_deployment_target() {
    local minimum targets low high
    minimum="$(_ios_sdk_min_deployment_target || true)"
    [ -n "$minimum" ] || return 0
    targets="$(_wda_project_deployment_targets "$1" || true)"
    [ -n "$targets" ] || return 0
    low="${targets%% *}"
    high="${targets##* }"
    _version_lt "$low" "$minimum" || return 0
    if _version_lt "$high" "$minimum"; then
        printf '%s\n' "$minimum"
    else
        printf '%s\n' "$high"
    fi
}

# Setup only: write (or retire) the generated xcconfig and export it.
_prepare_wda_xcconfig() {
    local target inherited="" tmp
    WDA_IOS_SDK_VERSION="$(_ios_sdk_version || true)"
    target="$(_wda_required_deployment_target "$RUNNER_PROJECT")"
    if [ -L "$WDA_XCCONFIG_FILE" ]; then
        warn "refusing to use a symlinked xcconfig: $WDA_XCCONFIG_FILE"
        return 1
    fi
    if [ -z "$target" ]; then
        WDA_DEPLOYMENT_TARGET_OVERRIDE=""
        if [ "${XCODE_XCCONFIG_FILE:-}" = "$WDA_XCCONFIG_FILE" ]; then
            unset XCODE_XCCONFIG_FILE
        fi
        rm -f -- "$WDA_XCCONFIG_FILE"
        return 0
    fi
    # Keep an xcconfig the caller already exported (for example a manual
    # workaround in the supervisor plist) in effect; ours is applied after it.
    if [ -n "${XCODE_XCCONFIG_FILE:-}" ] \
        && [ "$XCODE_XCCONFIG_FILE" != "$WDA_XCCONFIG_FILE" ]; then
        case "$XCODE_XCCONFIG_FILE" in
            *[\"$'\n']*)
                warn "ignoring inherited XCODE_XCCONFIG_FILE with an unsupported path"
                ;;
            /*) [ -f "$XCODE_XCCONFIG_FILE" ] && inherited="$XCODE_XCCONFIG_FILE" ;;
        esac
    fi
    tmp="$(mktemp "$STATE_DIR/wda-xcode-compat.xcconfig.new.XXXXXX")" || return 1
    {
        printf '%s\n' "// Generated by setup-wda.sh on every run; edits are overwritten."
        printf '%s\n' "// The selected Xcode rejects the runner project's iOS deployment target."
        [ -z "$inherited" ] || printf '#include? "%s"\n' "$inherited"
        printf 'IPHONEOS_DEPLOYMENT_TARGET = %s\n' "$target"
    } > "$tmp" || { rm -f "$tmp"; return 1; }
    if ! chmod 600 "$tmp" || ! mv -f "$tmp" "$WDA_XCCONFIG_FILE"; then
        rm -f "$tmp"
        return 1
    fi
    WDA_DEPLOYMENT_TARGET_OVERRIDE="$target"
    export XCODE_XCCONFIG_FILE="$WDA_XCCONFIG_FILE"
}

# The runner launches from its built product with `test-without-building
# -xctestrun`, which installs the product as built (a plain `xcodebuild test`
# would rebuild on every launch).
#
# xcodebuild names the file after the SDK it built against
# (IPhoneUseRunner_iphoneos27.0-arm64.xctestrun) and never deletes the one an
# older Xcode left behind, so after an Xcode upgrade the directory holds two.
# When there is more than one, take the one for the SDK this build used ($2;
# else the selected SDK). Anything other than exactly one match stays ambiguous
# and fails rather than launching a stale product.
_resolve_xctestrun() {
    local products_dir="$1" sdk="${2:-}" parent match count
    [ -n "$products_dir" ] || return 1
    parent="$(dirname "$products_dir")"
    [ -d "$parent" ] || return 1
    count="$(find "$parent" -maxdepth 1 -name '*.xctestrun' 2>/dev/null | wc -l | tr -d ' ')"
    if [ "$count" = "1" ]; then
        match="$(find "$parent" -maxdepth 1 -name '*.xctestrun' 2>/dev/null | head -1)"
    elif [ "$count" -gt 1 ] 2>/dev/null; then
        [ -n "$sdk" ] || sdk="$(_ios_sdk_version || true)"
        _valid_os_version "$sdk" || return 1
        count="$(find "$parent" -maxdepth 1 -name "*_iphoneos${sdk}-*.xctestrun" 2>/dev/null | wc -l | tr -d ' ')"
        [ "$count" = "1" ] || return 1
        match="$(find "$parent" -maxdepth 1 -name "*_iphoneos${sdk}-*.xctestrun" 2>/dev/null | head -1)"
    else
        return 1
    fi
    # The path rides in the PID-identity signature, which is space-delimited.
    case "$match" in
        ''|*[[:space:]]*) return 1 ;;
    esac
    printf '%s\n' "$match"
}

# Read-only doctor checks for the two ways an Xcode upgrade wedged setup.
_doctor_xcode_compat() {
    local fail=0 major minimum targets required current dir runs sdk matches name
    major="$(_xcode_major)"
    minimum="$(_ios_sdk_min_deployment_target || true)"
    targets="$(_wda_project_deployment_targets "$RUNNER_PROJECT" || true)"
    required="$(_wda_required_deployment_target "$RUNNER_PROJECT")"
    if [ -n "$required" ]; then
        current=""
        if [ -f "$WDA_XCCONFIG_FILE" ] && [ ! -L "$WDA_XCCONFIG_FILE" ]; then
            current="$(sed -n 's/^IPHONEOS_DEPLOYMENT_TARGET = \([0-9.]*\)$/\1/p' "$WDA_XCCONFIG_FILE" | tail -1)"
        fi
        if [ -f "$SELF_INSTALL" ] && ! grep -q 'XCODE_XCCONFIG_FILE' "$SELF_INSTALL" 2>/dev/null; then
            warn "X Xcode ${major:-?} supports iOS deployment targets from $minimum, but the runner project sets ${targets%% *}; the installed $SELF_INSTALL predates the override, so KeepAlive builds fail. Rerun setup to install the fixed script"
            fail=1
        elif [ "$current" = "$required" ]; then
            ok "Xcode ${major:-?} deployment target override: IPHONEOS_DEPLOYMENT_TARGET = $required ($WDA_XCCONFIG_FILE)"
        else
            warn "~ Xcode ${major:-?} supports iOS deployment targets from $minimum, but the runner project sets ${targets%% *}; no override is in place yet. Setup writes $WDA_XCCONFIG_FILE (IPHONEOS_DEPLOYMENT_TARGET = $required) on its next run"
        fi
    fi

    sdk="$(_ios_sdk_version || true)"
    for dir in "$RUNNER_DERIVED_DATA/Build/Products"; do
        [ -d "$dir" ] || continue
        runs="$(find "$dir" -maxdepth 1 -name '*.xctestrun' 2>/dev/null | sort)"
        [ "$(printf '%s' "$runs" | awk 'NF { c++ } END { print c + 0 }')" -gt 1 ] || continue
        matches=0
        if [ -n "$sdk" ]; then
            matches="$(printf '%s\n' "$runs" | awk -v s="_iphoneos${sdk}-" 'index($0, s) { c++ } END { print c + 0 }')"
        fi
        warn "~ multiple .xctestrun files in $dir (left by an earlier Xcode):"
        while IFS= read -r name; do
            [ -n "$name" ] || continue
            printf '     %s\n' "$(basename "$name")"
        done <<< "$runs"
        if [ "$matches" = "1" ]; then
            printf '     %s\n' "setup uses the one for the current SDK (iphoneos$sdk); the others are stale and can be deleted"
        else
            printf '     %s\n' "none is uniquely for the current SDK (${sdk:+iphoneos$sdk}); setup cannot pick a product to launch until the stale files are deleted"
        fi
    done
    return $fail
}
# END Xcode compatibility helpers.

# One-shot preflight: report the FIRST blocker as a checklist instead of a blind
# wait loop.  `setup-wda.sh doctor`
cmd_doctor() {
    info "Device runner preflight"
    local fail=0
    local xcode_version
    if [ -d "$STATE_DIR" ]; then
        ok "setup state directory present: $STATE_DIR"
    else
        warn "~ setup state is not initialized; doctor will not create it"
    fi
    xcode_version="$(xcodebuild -version 2>/dev/null | head -1 || true)"
    if [ -n "$xcode_version" ]; then
        ok "Full Xcode: $xcode_version"
        _doctor_xcode_compat || fail=1
    else
        warn "X Xcode is not installed: get it from the App Store ($XCODE_APP_STORE_URL), open it once, then rerun"
        fail=1
    fi
    if _resolve_signing_identity; then
        ok "Dev team: $TEAM_ID"
        if [ "$BUNDLE_ID_DERIVED" = "1" ]; then
            ok "Runner bundle ID: $WDA_BUNDLE_ID (derived for this team)"
        else
            ok "Runner bundle ID: $WDA_BUNDLE_ID (explicit or persisted)"
        fi
    else
        warn "X $SIGNING_ERROR"
        _open_xcode_for_account
        fail=1
    fi
    if _runner_source_valid; then
        local source_hash
        source_hash="$(_runner_source_hash 2>/dev/null || true)"
        if [ -n "$source_hash" ]; then
            ok "Device runner source: $RUNNER_SRC (sha256 ${source_hash:0:12})"
        else
            warn "X device runner sources could not be read: $RUNNER_SRC"
            fail=1
        fi
    else
        warn "X $RUNNER_SOURCE_ERROR"
        fail=1
    fi
    if [ -d "$WDA_DIR/.git" ]; then
        warn "~ a WebDriverAgent checkout from an earlier release is still at $WDA_DIR; nothing uses it any more (uninstall.sh removes it when it can prove ownership)"
    fi
    if _valid_port "$WDA_PORT" && _valid_port "$MJPEG_PORT" \
        && [ "$WDA_PORT" != "$MJPEG_PORT" ]; then
        ok "Loopback ports: control $WDA_PORT, video $MJPEG_PORT"
    else
        warn "X WDA_PORT and MJPEG_PORT must be distinct TCP ports from 1 to 65535"
        fail=1
    fi
    if _warp_preflight; then
        if _warp_on; then
            ok "$(_warp_ready_summary)"
        else
            ok "WARP: off / not present"
        fi
    else
        warn "X $WARP_PREFLIGHT_ERROR"
        fail=1
    fi
    if ! _system_proxy_check; then
        warn "X $SYSTEM_PROXY_ERROR"
        fail=1
    fi
    local usb usb_count relay_bin
    usb="$(_usb_udids)"
    usb_count="$(printf '%s' "$usb" | wc -w | tr -d '[:space:]')"
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ -z "$usb" ]; then
        warn "X the default device layer requires an iPhone connected over USB"
        fail=1
    elif [ "$WDA_ALLOW_LAN" = "0" ] && [ "$usb_count" -gt 1 ] \
        && [ -z "${WDA_UDID:-}" ]; then
        warn "X multiple USB iPhones found ($usb); set WDA_UDID=<one exact UDID>"
        fail=1
    elif [ "$WDA_ALLOW_LAN" = "0" ] && [ -n "${WDA_UDID:-}" ] \
        && ! _target_on_usb; then
        warn "X configured target $WDA_UDID is not connected over USB"
        fail=1
    elif [ -n "$usb" ]; then
        ok "iPhone on USB: $usb"
    else
        warn "~ WDA_ALLOW_LAN=1: no USB iPhone; setup will require one unambiguous paired destination"
    fi
    # Informational, never a failure: Xcode usually drives an iOS one minor
    # release ahead of its SDK, so only the runner's own refusal is decisive.
    local target_udid device_ios sdk_ios
    target_udid="${WDA_UDID:-}"
    if [ -z "$target_udid" ] && [ "$usb_count" = "1" ]; then
        target_udid="$usb"
    fi
    sdk_ios="$(_ios_sdk_version || true)"
    device_ios=""
    if [ -n "$target_udid" ]; then
        device_ios="$(_device_ios_version "$target_udid" || true)"
    fi
    if [ -n "$device_ios" ] && [ -n "$sdk_ios" ]; then
        if _version_lt "$(_os_major_minor "$sdk_ios")" "$(_os_major_minor "$device_ios")"; then
            warn "~ iPhone runs iOS $device_ios but the Xcode SDK is iOS $sdk_ios. A one-minor bump often still works; a beta iOS or a newer major makes the runner exit with code 74. If setup then reports xcode_too_old, install an Xcode that supports iOS $(_os_major_minor "$device_ios")"
        else
            ok "iPhone iOS $device_ios is covered by the Xcode SDK (iOS $sdk_ios)"
        fi
    elif [ -n "$target_udid" ]; then
        warn "~ could not read the iPhone iOS version to compare with the Xcode SDK${sdk_ios:+ (iOS $sdk_ios)}"
    fi
    if command -v lsof >/dev/null 2>&1; then ok "lsof present for listener ownership checks"; else warn "X lsof is required"; fail=1; fi
    if relay_bin="$(_relay_binary)"; then
        ok "USB relay: $relay_bin relay (macOS usbmuxd)"
    elif command -v iproxy >/dev/null 2>&1; then
        ok "USB relay: iproxy (this iphone-use app predates the built-in relay; upgrade with: iphone-use upgrade)"
    elif [ "$WDA_ALLOW_LAN" = "0" ]; then
        warn "X no USB relay: the iPhoneUse app is missing or too old — reinstall or run: iphone-use upgrade"
        fail=1
    fi
    if [ "$WDA_ALLOW_LAN" = "1" ] && ! command -v socat >/dev/null 2>&1 \
        && ! _relay_binary >/dev/null && ! command -v iproxy >/dev/null 2>&1; then
        warn "X WDA_ALLOW_LAN=1 needs a USB relay or socat"
        fail=1
    fi
    if _valid_port "$WDA_PORT"; then
        curl -s -m 4 "http://127.0.0.1:$WDA_PORT/status" >/dev/null 2>&1 \
            && ok "device runner already serving on 127.0.0.1:$WDA_PORT"
    fi
    # Caveats the installer used to print to everyone; they matter only when
    # something above goes wrong, so they live here.
    printf '%s\n' "${BOLD}Notes${RST}"
    printf '  %s\n' "• The device runner on the iPhone has no password of its own. The Mac relays it on 127.0.0.1 only;"
    printf '  %s\n' "  WDA_ALLOW_LAN=1 (a socat relay over Wi-Fi) is an explicit, unsafe fallback for trusted networks."
    printf '  %s\n' "• Cloudflare WARP or another tunnel VPN can break Xcode's connection to the phone; disconnect it during setup if setup stalls."
    if [ "$fail" = 0 ]; then
        ok "preflight checks passed; build, signing, device trust, and launch still require setup verification"
    else
        warn "fix the X items above, then re-run"
    fi
    return $fail
}

_refresh_legacy_contracts() {
    local legacy_udid="${WDA_UDID:-__missing_udid__}"
    local legacy_team="${WDA_TEAM_ID:-__missing_team__}"
    local legacy_bundle="${WDA_BUNDLE_ID:-__missing_bundle__}"
    LEGACY_RUNNER_EXPECTED="legacy-runner:$legacy_udid:$legacy_team:$legacy_bundle"
    LEGACY_RELAY_EXPECTED="legacy-relay:$WDA_PORT:8100:$legacy_udid"
    LEGACY_MJPEG_EXPECTED="legacy-mjpeg:$MJPEG_PORT:9100:$legacy_udid"
}
_refresh_legacy_contracts
MJPEG_RELAY_PID_FILE="$STATE_DIR/wda-mjpeg-relay.pid"
VALIDATED_PID=""
PID_RECORD_PID=""
PID_RECORD_LSTART=""
PID_RECORD_EXPECTED=""
PID_RECORD_LEGACY=0

_pid_exists() {
    case "$1" in
        ''|0|1|0*|*[!0-9]*) return 1 ;;
    esac
    ps -p "$1" -o pid= >/dev/null 2>&1
}

_safe_expected() {
    case "$1" in
        ''|*'|'*|*$'\n'*|*$'\r'*) return 1 ;;
        *) return 0 ;;
    esac
}

_store_pid_record() {
    local file="$1"
    local pid="$2"
    local lstart="$3"
    local expected="$4"
    local tmp="${file}.tmp.$$"
    _pid_exists "$pid" || return 1
    [ -n "$lstart" ] || return 1
    _safe_expected "$expected" || return 1
    printf '%s|%s|%s\n' "$pid" "$lstart" "$expected" > "$tmp" || return 1
    chmod 600 "$tmp" || { rm -f "$tmp"; return 1; }
    mv -f "$tmp" "$file"
}

_pid_record_parse() {
    local file="$1"
    local legacy_expected="$2"
    local record rest
    PID_RECORD_PID=""
    PID_RECORD_LSTART=""
    PID_RECORD_EXPECTED=""
    PID_RECORD_LEGACY=0
    [ -f "$file" ] || return 1
    [ "$(awk 'END { print NR }' "$file" 2>/dev/null)" = "1" ] || return 1
    record="$(sed -n '1p' "$file" 2>/dev/null || true)"
    case "$record" in
        *'|'*)
            PID_RECORD_PID="${record%%|*}"
            rest="${record#*|}"
            case "$rest" in
                *'|'*) ;;
                *) return 1 ;;
            esac
            PID_RECORD_LSTART="${rest%%|*}"
            PID_RECORD_EXPECTED="${rest#*|}"
            [ -n "$PID_RECORD_LSTART" ] || return 1
            _safe_expected "$PID_RECORD_EXPECTED" || return 1
            ;;
        *)
            # Legacy files contained only a PID. They remain compatible, but the
            # caller supplies a UDID/port-specific expected command contract.
            PID_RECORD_PID="$record"
            PID_RECORD_EXPECTED="$legacy_expected"
            PID_RECORD_LEGACY=1
            ;;
    esac
    case "$PID_RECORD_PID" in
        ''|0|1|0*|*[!0-9]*) return 1 ;;
    esac
    _safe_expected "$PID_RECORD_EXPECTED" || return 1
    return 0
}

_expected_role_valid() {
    local expected="$1"
    local role="$2"
    case "$role:$expected" in
        runner:runner:?*|runner:legacy-runner:?*) return 0 ;;
        relay:relay:?*|relay:legacy-relay:?*) return 0 ;;
        mjpeg:mjpeg:?*|mjpeg:legacy-mjpeg:?*) return 0 ;;
        *) return 1 ;;
    esac
}

# The optional ASC suffix must be complete and in the order emitted by
# _prepare_xcodebuild_args. Do not replace it with an arbitrary-arguments tail:
# these signatures authorize signalling the recorded process.
#
# The first form is the device runner this script starts. The two
# WebDriverAgent forms are what releases before it started; they stay
# recognised so the first setup/stop/pause after an upgrade can still stop a
# WDA runner the previous release left behind.
_runner_signature_valid() {
    local asc_suffix
    _safe_expected "$1" || return 1
    asc_suffix=' -authenticationKeyPath /[^|[:cntrl:]]+\.p8 -authenticationKeyID [A-Za-z0-9]+ -authenticationKeyIssuerID [A-Za-z0-9-]+ -allowProvisioningDeviceRegistration'
    printf '%s\n' "$1" | LC_ALL=C grep -Eq \
        "^(/[^ ]*/)?xcodebuild (-destination platform=iOS,id=[0-9A-Fa-f-]+ test-without-building -xctestrun /[^ ]+/IPhoneUseRunner_[^ /]+\\.xctestrun -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe( -allowProvisioningUpdates$asc_suffix)?|-project WebDriverAgent\\.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=[0-9A-Fa-f-]+ -allowProvisioningUpdates DEVELOPMENT_TEAM=[A-Z0-9]{10} PRODUCT_BUNDLE_IDENTIFIER=[A-Za-z0-9.-]+ test($asc_suffix)?|-destination platform=iOS,id=[0-9A-Fa-f-]+ test-without-building -xctestrun /[^ ]+/WebDriverAgentRunner_[^ /]+\\.xctestrun( -allowProvisioningUpdates$asc_suffix)?)$"
}

_command_matches_expected() {
    local command="$1"
    local expected="$2"
    local signature rest local_port device_port target_udid team_id bundle_id
    local legacy_argv xctestrun_argv base_command
    case "$expected" in
        runner:*)
            signature="${expected#*:}"
            _runner_signature_valid "$signature" || return 1
            [ "$command" = "$signature" ]
            ;;
        relay:*|mjpeg:*)
            signature="${expected#*:}"
            if printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^/[^ ]+/iphone-use relay --udid [0-9A-Fa-f-]+ --listen 127\.0\.0\.1:[0-9]+ --device-port [0-9]+$'; then
                :
            elif printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?iproxy -s 127\.0\.0\.1 [0-9]+:[0-9]+ -u [0-9A-Fa-f-]+$'; then
                :
            elif printf '%s\n' "$signature" | LC_ALL=C grep -Eq \
                '^(/[^ ]*/)?socat TCP-LISTEN:[0-9]+,fork,reuseaddr,bind=127\.0\.0\.1 TCP:[A-Za-z0-9.:%_-]+:[0-9]+$'; then
                :
            else
                return 1
            fi
            [ "$command" = "$signature" ]
            ;;
        legacy-runner:*)
            rest="${expected#legacy-runner:}"
            target_udid="${rest%%:*}"
            rest="${rest#*:}"
            team_id="${rest%%:*}"
            bundle_id="${rest#*:}"
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            _valid_team_id "$team_id" || return 1
            _valid_bundle_id "$bundle_id" || return 1
            _runner_signature_valid "$command" || return 1
            # Strip only the complete, validated suffix before checking the
            # legacy UDID/team/bundle contract. New PID records still require
            # exact equality with the full command, including the key path.
            base_command="${command%% -authenticationKeyPath *}"
            base_command="${base_command% -allowProvisioningUpdates}"
            # PID-only records predate the device runner: they name a
            # WebDriverAgent runner in either of its two launch forms (the
            # xctestrun form cannot be combined with -project/-scheme, so it
            # shares only the destination with the `test` form).
            legacy_argv="xcodebuild -project WebDriverAgent.xcodeproj -scheme WebDriverAgentRunner -destination platform=iOS,id=$target_udid -allowProvisioningUpdates DEVELOPMENT_TEAM=$team_id PRODUCT_BUNDLE_IDENTIFIER=$bundle_id"
            xctestrun_argv="xcodebuild -destination platform=iOS,id=$target_udid test-without-building -xctestrun"
            case "$base_command" in
                "$legacy_argv test"|*/"$legacy_argv test") return 0 ;;
                "$xctestrun_argv /"*/WebDriverAgentRunner_*.xctestrun \
                |*/"$xctestrun_argv /"*/WebDriverAgentRunner_*.xctestrun)
                    # One path argument, no embedded spaces.
                    case "${base_command#*-xctestrun }" in
                        *" "*) return 1 ;;
                    esac
                    return 0
                    ;;
                *) return 1 ;;
            esac
            ;;
        legacy-relay:*|legacy-mjpeg:*)
            rest="${expected#*:}"
            local_port="${rest%%:*}"
            rest="${rest#*:}"
            device_port="${rest%%:*}"
            target_udid="${rest#*:}"
            _valid_port "$local_port" || return 1
            _valid_port "$device_port" || return 1
            case "$target_udid" in
                ''|__missing_udid__|*[!0-9A-Fa-f-]*) return 1 ;;
            esac
            case "$command" in
                "iproxy $local_port $device_port -u $target_udid"|\
                */"iproxy $local_port $device_port -u $target_udid"|\
                "iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid"|\
                */"iproxy -s 127.0.0.1 $local_port:$device_port -u $target_udid")
                    return 0
                    ;;
                # A legacy socat argv has no UDID, so it cannot prove which
                # phone it belongs to. Refuse to adopt or kill it.
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

_validate_legacy_runner_cwd() {
    local pid="$1"
    local cwd_data process_cwd expected_cwd
    command -v lsof >/dev/null 2>&1 || return 1
    [ -d "$WDA_DIR" ] || return 1
    expected_cwd="$(cd "$WDA_DIR" 2>/dev/null && pwd -P)" || return 1
    cwd_data="$(lsof -nP -a -p "$pid" -d cwd -Fn 2>/dev/null)" || return 1
    process_cwd="$(printf '%s\n' "$cwd_data" | sed -n 's/^n//p' | head -1)"
    [ -n "$process_cwd" ] && [ "$process_cwd" = "$expected_cwd" ]
}

_verify_listener_owner_pid() {
    local pid="$1"
    local port="$2"
    local listener_data listener_pids unexpected
    listener_data="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN -Fp 2>/dev/null)" \
        || return 1
    listener_pids="$(printf '%s\n' "$listener_data" | sed -n 's/^p//p' | sort -u)"
    [ -n "$listener_pids" ] || return 1
    unexpected="$(printf '%s\n' "$listener_pids" \
        | awk -v p="$pid" '$0 != p { print; exit }')"
    [ -z "$unexpected" ]
}

_legacy_migration_hint() {
    warn "legacy pid-only state was found but could not be proven safe to stop automatically.
   Retry with the exact old values (do not guess):
     WDA_UDID=<old-device-udid> WDA_TEAM_ID=<10-char-team> \\
     WDA_BUNDLE_ID=<old-runner-bundle-id> WDA_DIR=<old-wda-checkout> \\
       $SELF_INSTALL stop
   If the old relay used socat, inspect its numeric PID with both:
     ps -ww -p <pid> -o uid=,lstart=,command=
     lsof -nP -a -p <pid> -iTCP:<8100-or-9100> -sTCP:LISTEN
   This script intentionally will not turn an unproven legacy PID into a global kill."
}

_validate_pid_record() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local adopt_legacy="${4:-0}"
    local process_uid process_lstart process_command
    VALIDATED_PID=""
    _pid_record_parse "$file" "$legacy_expected" || return 1
    _expected_role_valid "$PID_RECORD_EXPECTED" "$role" || return 1
    _pid_exists "$PID_RECORD_PID" || return 1
    process_uid="$(ps -p "$PID_RECORD_PID" -o uid= 2>/dev/null | tr -d '[:space:]')"
    [ "$process_uid" = "$UID_NUM" ] || return 1
    process_lstart="$(LC_ALL=C ps -p "$PID_RECORD_PID" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    [ -n "$process_lstart" ] || return 1
    if [ -n "$PID_RECORD_LSTART" ] \
        && [ "$process_lstart" != "$PID_RECORD_LSTART" ]; then
        return 1
    fi
    process_command="$(LC_ALL=C ps -ww -p "$PID_RECORD_PID" -o command= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
    _command_matches_expected "$process_command" "$PID_RECORD_EXPECTED" || return 1
    if [ "$PID_RECORD_LEGACY" = "1" ]; then
        case "$PID_RECORD_EXPECTED" in
            legacy-runner:*)
                _validate_legacy_runner_cwd "$PID_RECORD_PID" || return 1
                ;;
        esac
        if [ "$adopt_legacy" = "1" ]; then
            # Only a mutating setup/stop path may adopt a target-verified legacy
            # PID. Status validates the same evidence without rewriting state.
            _store_pid_record "$file" "$PID_RECORD_PID" "$process_lstart" \
                "$PID_RECORD_EXPECTED" || return 1
            _validate_pid_record "$file" "$legacy_expected" "$role" 0
            return $?
        fi
    fi
    [ "$(LC_ALL=C ps -p "$PID_RECORD_PID" -o lstart= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "$process_lstart" ] \
        || return 1
    [ "$(LC_ALL=C ps -ww -p "$PID_RECORD_PID" -o command= 2>/dev/null \
        | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" = "$process_command" ] \
        || return 1
    VALIDATED_PID="$PID_RECORD_PID"
    return 0
}

_write_pid_record() {
    local file="$1"
    local pid="$2"
    local expected="$3"
    local role="$4"
    local process_uid process_lstart process_command tries
    _expected_role_valid "$expected" "$role" || return 1
    _safe_expected "$expected" || return 1
    tries=0
    while [ "$tries" -lt 30 ]; do
        tries=$((tries + 1))
        _pid_exists "$pid" || return 1
        process_uid="$(ps -p "$pid" -o uid= 2>/dev/null | tr -d '[:space:]')"
        process_lstart="$(LC_ALL=C ps -p "$pid" -o lstart= 2>/dev/null \
            | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        process_command="$(LC_ALL=C ps -ww -p "$pid" -o command= 2>/dev/null \
            | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
        if [ "$process_uid" = "$UID_NUM" ] \
            && [ -n "$process_lstart" ] \
            && _command_matches_expected "$process_command" "$expected"; then
            break
        fi
        sleep 0.1
    done
    [ "$process_uid" = "$UID_NUM" ] || return 1
    [ -n "$process_lstart" ] || return 1
    _command_matches_expected "$process_command" "$expected" || return 1
    _store_pid_record "$file" "$pid" "$process_lstart" "$expected" || return 1
    _validate_pid_record "$file" "$expected" "$role"
}

_stop_managed_process() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local candidate tries was_legacy legacy_rest legacy_port
    [ -f "$file" ] || return 0
    _pid_record_parse "$file" "$legacy_expected" || {
        warn "invalid managed PID record; refusing to act: $file"
        return 1
    }
    candidate="$PID_RECORD_PID"
    was_legacy="$PID_RECORD_LEGACY"
    if ! _validate_pid_record "$file" "$legacy_expected" "$role" 1; then
        if _pid_exists "$candidate"; then
            [ "$was_legacy" = "1" ] && _legacy_migration_hint
            warn "PID $candidate does not match the current-user $role identity; refusing to kill it"
            return 1
        fi
        rm -f "$file"
        return 0
    fi
    if [ "$was_legacy" = "1" ]; then
        case "$legacy_expected" in
            legacy-relay:*|legacy-mjpeg:*)
                legacy_rest="${legacy_expected#*:}"
                legacy_port="${legacy_rest%%:*}"
                if ! _verify_listener_owner_pid "$VALIDATED_PID" "$legacy_port"; then
                    warn "legacy $role PID does not exclusively own TCP $legacy_port; refusing to kill it"
                    _legacy_migration_hint
                    return 1
                fi
                _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
                ;;
        esac
    fi
    kill -TERM "$VALIDATED_PID" 2>/dev/null || true
    tries=0
    while _pid_exists "$VALIDATED_PID" && [ "$tries" -lt 20 ]; do
        tries=$((tries + 1))
        sleep 0.25
    done
    if _pid_exists "$VALIDATED_PID"; then
        warn "managed $role pid $VALIDATED_PID did not stop after SIGTERM"
        return 1
    fi
    rm -f "$file"
    return 0
}

_assert_port_free() {
    local port="$1"
    local listeners lsof_error lsof_status
    command -v lsof >/dev/null 2>&1 || {
        warn "lsof is required to prove TCP $port is free"
        return 1
    }
    lsof_error="$STATE_DIR/.lsof-error.$$"
    if listeners="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN 2>"$lsof_error")"; then
        lsof_status=0
    else
        lsof_status=$?
    fi
    if [ "$lsof_status" -eq 1 ] && [ ! -s "$lsof_error" ]; then
        rm -f "$lsof_error"
        return 0
    fi
    if [ "$lsof_status" -ne 0 ]; then
        warn "lsof could not prove TCP $port is free:"
        sed 's/^/    /' "$lsof_error" >&2
        rm -f "$lsof_error"
        return 1
    fi
    if [ -s "$lsof_error" ]; then
        warn "lsof returned diagnostics, so TCP $port is not proven free:"
        sed 's/^/    /' "$lsof_error" >&2
        rm -f "$lsof_error"
        return 1
    fi
    rm -f "$lsof_error"
    [ -z "$listeners" ] && return 0
    warn "TCP $port is already owned by a non-managed listener:"
    printf '%s\n' "$listeners" | sed 's/^/    /' >&2
    return 1
}

_verify_loopback_listener() {
    local file="$1"
    local legacy_expected="$2"
    local role="$3"
    local port="$4"
    local pid listener_data listener_pids listener_name_data listener_names unexpected
    command -v lsof >/dev/null 2>&1 || return 1
    _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
    pid="$VALIDATED_PID"
    listener_data="$(lsof -nP -a -iTCP:"$port" -sTCP:LISTEN -Fp 2>/dev/null)" \
        || return 1
    listener_pids="$(printf '%s\n' "$listener_data" | sed -n 's/^p//p' | sort -u)"
    [ -n "$listener_pids" ] || return 1
    unexpected="$(printf '%s\n' "$listener_pids" | awk -v p="$pid" '$0 != p { print; exit }')"
    [ -z "$unexpected" ] || return 1
    listener_name_data="$(lsof -nP -a -p "$pid" -iTCP:"$port" -sTCP:LISTEN -Fn 2>/dev/null)" \
        || return 1
    listener_names="$(printf '%s\n' "$listener_name_data" | sed -n 's/^n//p')"
    [ -n "$listener_names" ] || return 1
    unexpected="$(printf '%s\n' "$listener_names" \
        | awk -v n="127.0.0.1:$port" '$0 != n { print; exit }')"
    [ -z "$unexpected" ] || return 1
    _validate_pid_record "$file" "$legacy_expected" "$role" || return 1
    [ "$VALIDATED_PID" = "$pid" ] || return 1
    return 0
}

cmd_stop() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; there are no PID-owned processes to stop safely"
        return 1
    fi
    info "Stopping the dedicated runner supervisor and its managed processes"
    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    _wait_job_gone "$WDA_AGENT_LABEL" || failed=1
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || failed=1
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || failed=1
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || failed=1
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        failed=1
    fi
    if [ "$failed" != "0" ]; then
        warn "stop was not fully verified; no unowned process was killed"
        return 1
    fi
    ok "runner supervisor and all PID-verified managed processes stopped"
}

cmd_pause() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; there is no managed runner stack to pause"
        return 1
    fi
    info "Pausing the managed device runner and giving the phone back to the user"
    # Disable the exact launchd label before bootout so KeepAlive cannot race
    # the PID-verified shutdown. Never use pkill: another xcodebuild or relay
    # may belong to the user rather than this setup.
    launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || failed=1
    launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
    _wait_job_gone "$WDA_AGENT_LABEL" || failed=1
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || failed=1
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || failed=1
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || failed=1
    _reset_keepalive_retry || failed=1
    [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" = "1" ] \
        || failed=1
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        failed=1
    fi
    if [ "$failed" != "0" ]; then
        warn "pause was not fully verified; no process without a matching PID/argv record was killed"
        return 1
    fi
    ok "device runner paused: supervisor disabled and all PID-verified runner/relay processes stopped"
    printf '  Resume: %s resume\n' "$SELF_INSTALL"
}

cmd_resume() {
    local label program interpreter
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; run setup before resume"
        return 1
    fi
    if ! _marker_file_secure "$WDA_AGENT_PLIST" \
        || ! plutil -lint "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
        warn "managed runner supervisor plist is missing or unsafe: $WDA_AGENT_PLIST; run setup again"
        return 1
    fi
    label="$(/usr/libexec/PlistBuddy -c 'Print :Label' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    interpreter="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    program="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:1' "$WDA_AGENT_PLIST" 2>/dev/null || true)"
    if [ "$label" != "$WDA_AGENT_LABEL" ] \
        || [ "$interpreter" != "/bin/bash" ] \
        || [ "$program" != "$SELF_INSTALL" ] \
        || [ ! -x "$SELF_INSTALL" ]; then
        warn "managed runner supervisor identity is not the expected setup helper; run setup again"
        return 1
    fi

    info "Resuming the managed runner supervisor"
    _reset_keepalive_retry || return 1
    if ! launchctl enable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        warn "could not enable the runner supervisor"
        return 1
    fi
    if ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
        && ! launchctl bootstrap "$GUI_DOMAIN" "$WDA_AGENT_PLIST" >/dev/null 2>&1; then
        launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        warn "could not bootstrap the runner supervisor; it remains paused"
        return 1
    fi
    if [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" != "0" ] \
        || ! launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        launchctl disable "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 || true
        warn "resume was not verified; the runner supervisor remains paused"
        return 1
    fi
    ok "device runner resume requested; lock-screen failures will retry with quiet backoff"
    printf '  Status: %s status\n' "$SELF_INSTALL"
    printf '  Log   : %s\n' "$WDA_AGENT_LOG"
}

cmd_status() {
    local failed=0
    if [ ! -d "$STATE_DIR" ]; then
        warn "setup state is not initialized at $STATE_DIR; run setup before requesting runtime status"
        return 1
    fi
    if ! _valid_port "$WDA_PORT" || ! _valid_port "$MJPEG_PORT" \
        || [ "$WDA_PORT" = "$MJPEG_PORT" ]; then
        warn "WDA_PORT and MJPEG_PORT must be distinct decimal TCP ports from 1 to 65535"
        return 1
    fi
    if [ "$(_job_disabled_state "$WDA_AGENT_LABEL" 2>/dev/null || true)" = "1" ]; then
        warn "the device runner is paused; run $SELF_INSTALL resume before the next agent session"
        return 1
    fi
    if ! command -v lsof >/dev/null 2>&1; then
        warn "lsof is required to verify relay PID ownership and loopback-only binds"
        return 1
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        ok "runner supervisor loaded: $GUI_DOMAIN/$WDA_AGENT_LABEL"
    else
        warn "runner supervisor not loaded"
        failed=1
    fi
    if _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        ok "PID-verified device runner alive: $VALIDATED_PID"
    else
        warn "device runner PID record is absent, stale, or does not match its process"
        failed=1
    fi
    if _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT"; then
        ok "control relay PID owns only 127.0.0.1:$WDA_PORT"
    else
        warn "control relay ownership/bind could not be verified"
        failed=1
    fi
    if _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT"; then
        ok "video relay PID owns only 127.0.0.1:$MJPEG_PORT"
    else
        warn "video relay ownership/bind could not be verified"
        failed=1
    fi
    if curl -fsS -m 4 "http://127.0.0.1:$WDA_PORT/status" >/dev/null 2>&1; then
        ok "device runner /status reachable through the loopback relay"
    else
        warn "device runner /status is not reachable"
        failed=1
    fi
    return "$failed"
}

case "$COMMAND" in
    stop)   cmd_stop;   exit $? ;;
    pause)  cmd_pause;  exit $? ;;
    resume) cmd_resume; exit $? ;;
    status) cmd_status; exit $? ;;
    doctor) cmd_doctor; exit $? ;;
    setup)  ;;
    *) die "unknown command: $1 (use: setup|status|stop|pause|resume|doctor|instance-context)" ;;
esac

# Before anything is paused or built, so stopping here leaves nothing to undo.
if [ "${WDA_KEEPALIVE:-0}" != "1" ] && [ -t 1 ]; then
    if ! _first_run_checklist; then
        _rerun="iphone-use setup"
        [ "$INSTANCE_NAME" = default ] || _rerun="$_rerun --instance $INSTANCE_NAME"
        die "fix the ✗ items above, then run: $_rerun"
    fi
fi

if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    _wait_for_keepalive_retry
    KEEPALIVE_ATTEMPT_ACTIVE=1
fi
_status_begin_run || die "could not initialize the setup status owner"

# A manual setup temporarily owns the lifecycle so an already-running KeepAlive
# job cannot race its build or relays. A successful run installs and bootstraps
# the same supervisor again after the runner has been proven reachable.
if [ "${WDA_KEEPALIVE:-0}" != "1" ]; then
    SUPERVISOR_TRANSACTION_ACTIVE=1
    PREVIOUS_SUPERVISOR_DISABLED="$(_job_disabled_state "$WDA_AGENT_LABEL")" \
        || die "could not snapshot the runner supervisor's launchd disabled policy"
    if [ -f "$WDA_AGENT_PLIST" ]; then
        PREVIOUS_SUPERVISOR_PLIST_PRESENT=1
        cp -p "$WDA_AGENT_PLIST" "$WDA_AGENT_ROLLBACK_PLIST" \
            || die "could not save the existing runner supervisor plist for rollback"
    fi
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1; then
        [ "$PREVIOUS_SUPERVISOR_PLIST_PRESENT" = "1" ] \
            || die "runner supervisor is loaded but its plist is missing; refusing an unrecoverable handoff"
        info "Pausing the existing runner supervisor for interactive setup"
        PREVIOUS_SUPERVISOR_LOADED=1
        launchctl bootout "$GUI_DOMAIN/$WDA_AGENT_LABEL" 2>/dev/null || true
        _wait_job_gone "$WDA_AGENT_LABEL" \
            || die "existing runner supervisor did not stop; refusing to race it"
    fi
fi

# ── 0. Prereqs ────────────────────────────────────────────────────────────────
# Keep the last known blocker visible while KeepAlive starts its next pass.
# Clearing it before the corresponding check completed made `/agent/status`
# flicker between the real cause and an empty blocker every few seconds.
_PREVIOUS_BLOCKER="$(sed -n 's/.*"blocked_on":"\([^"]*\)".*/\1/p' "$STATUS_FILE" 2>/dev/null | head -1)"
case "$_PREVIOUS_BLOCKER" in
    warp|proxy|usb|trust|ddi|automation_mode_disabled|xcode_too_old|wda) ;;
    *) _PREVIOUS_BLOCKER="" ;;
esac
# A USB blocker from an earlier default-mode attempt is incompatible with an
# explicit LAN run. Keeping it visible while the LAN setup builds made status
# falsely tell callers to attach a cable even though socat recovery was active.
if [ "$WDA_ALLOW_LAN" = "1" ] && [ "$_PREVIOUS_BLOCKER" = "usb" ]; then
    _PREVIOUS_BLOCKER=""
fi
# Trust is checked only once xcodebuild reaches the on-device runner. Keep that
# blocker visible across KeepAlive's next prerequisite/build pass; clearing it
# at `building` made status oscillate back to an empty blocker while the phone
# still required the same manual approval. `serving` below is the first
# authoritative evidence that trust was restored, and clears it there. The
# UI-automation switch is the same kind of on-phone approval, seen at the same
# point, and is kept visible the same way; so is an Xcode too old for the
# phone, which only a different Xcode clears.
case "$_PREVIOUS_BLOCKER" in
    trust|automation_mode_disabled|xcode_too_old) _BUILD_BLOCKER="$_PREVIOUS_BLOCKER" ;;
    *) _BUILD_BLOCKER="" ;;
esac
_setstatus prereq "$_PREVIOUS_BLOCKER" "checking prerequisites"
info "Checking prerequisites"
_warp_check   # permits route-safe WARP or explicit-only Local proxy mode
if ! _system_proxy_check; then
    _setstatus prereq proxy "macOS system proxy is enabled but unusable"
    die "$SYSTEM_PROXY_ERROR"
fi
command -v lsof >/dev/null 2>&1 || die "lsof is required to verify exclusive loopback relay ownership"
XCODEBUILD_BIN="$(command -v xcodebuild || true)"
[ -n "$XCODEBUILD_BIN" ] \
    || die "Xcode is not installed. Get it from the App Store ($XCODE_APP_STORE_URL), open it once, then rerun"
_valid_port "$WDA_PORT" \
    || die "WDA_PORT must be a decimal TCP port from 1 to 65535 (got '$WDA_PORT')"
_valid_port "$MJPEG_PORT" \
    || die "MJPEG_PORT must be a decimal TCP port from 1 to 65535 (got '$MJPEG_PORT')"
[ "$WDA_PORT" != "$MJPEG_PORT" ] \
    || die "WDA_PORT and MJPEG_PORT must be different (both are '$WDA_PORT')"
XCODE_VERSION="$(_xcode_version_cached "$XCODEBUILD_BIN")"
[ -n "$XCODE_VERSION" ] \
    || die "full Xcode is not selected. Install it from the App Store ($XCODE_APP_STORE_URL), then run: sudo xcode-select -s /Applications/Xcode.app"
ok "Xcode: $XCODE_VERSION"

# Resolve and validate one identity before touching the managed checkout.
if ! _resolve_signing_identity; then
    _open_xcode_for_account
    die "$SIGNING_ERROR"
fi
ok "Team: $TEAM_ID"
if [ "$BUNDLE_ID_DERIVED" = "1" ]; then
    ok "Runner bundle ID: $WDA_BUNDLE_ID (derived for this team)"
else
    ok "Runner bundle ID: $WDA_BUNDLE_ID (explicit or persisted)"
fi

# ── 1. Resolve device ─────────────────────────────────────────────────────────
info "Resolving target device"
# One target contract across installer, daemon, supervisor, and manual reruns:
# explicit WDA_UDID > explicit PHONE_REMOTE_UDID > daemon plist > existing WDA
# supervisor > safe USB auto-detection.
if [ -z "${WDA_UDID:-}" ] && [ -n "${PHONE_REMOTE_UDID:-}" ]; then
    WDA_UDID="$PHONE_REMOTE_UDID"
fi
if [ -z "${WDA_UDID:-}" ] && [ -f "$DAEMON_PLIST" ]; then
    WDA_UDID="$(/usr/libexec/PlistBuddy \
        -c "Print :EnvironmentVariables:PHONE_REMOTE_UDID" "$DAEMON_PLIST" \
        2>/dev/null || true)"
fi
if [ -z "${WDA_UDID:-}" ] && [ -f "$WDA_AGENT_PLIST" ]; then
    WDA_UDID="$(/usr/libexec/PlistBuddy \
        -c "Print :EnvironmentVariables:WDA_UDID" "$WDA_AGENT_PLIST" \
        2>/dev/null || true)"
fi
if [ -n "${WDA_UDID:-}" ] \
    && ! printf '%s' "$WDA_UDID" | LC_ALL=C grep -Eq '^[0-9A-Fa-f-]+$'; then
    die "target UDID contains invalid characters (expected hex and dashes)"
fi
# A second instance exists to drive one specific phone; guessing could hand it
# the phone another daemon is driving.
if [ "$INSTANCE_NAME" != default ] && [ -z "${WDA_UDID:-}" ]; then
    die "instance $INSTANCE_NAME has no target iPhone; set WDA_UDID or rerun install.sh --instance $INSTANCE_NAME --udid <UDID>"
fi
# Prefer the iPhone physically on USB — with several paired phones, auto-detect
# otherwise grabs the first -showdestinations hit, which is often a dead one.
if [ -z "${WDA_UDID:-}" ]; then
    _USB="$(_usb_udids)"
    if [ "$(printf '%s' "$_USB" | wc -w)" = 1 ]; then
        WDA_UDID="$_USB"; ok "using USB-connected iPhone: $WDA_UDID"
    elif [ -n "$_USB" ]; then
        die "multiple iPhones are connected over USB ($_USB). Set WDA_UDID=<one>; refusing to guess."
    fi
fi
if [ "$WDA_ALLOW_LAN" = "0" ]; then
    if [ -z "${WDA_UDID:-}" ]; then
        _setstatus prereq usb "no USB iPhone is connected"
        die "the device layer defaults to USB, but no USB iPhone was found.
   Plug in and unlock one iPhone, or set WDA_UDID=<USB UDID>; no build was started."
    fi
    if ! _target_on_usb; then
        _setstatus prereq usb "the configured iPhone is not connected over USB"
        die "target $WDA_UDID is not currently connected over USB.
   Plug in that iPhone, or set WDA_UDID to the exact USB-connected device; refusing a slow Wi-Fi fallback."
    fi
elif [ -z "${WDA_UDID:-}" ]; then
    warn "WDA_ALLOW_LAN=1: no USB target; paired destinations will be enumerated from the runner project"
fi
_instance_check_bindings_cached "$WDA_PORT" "$MJPEG_PORT" 2>&1 \
    || die "refusing to set up instance $INSTANCE_NAME (see above)"
_setstatus prereq "" "prerequisites passed"

# ── 2. Device runner source ──────────────────────────────────────────────────
info "Device runner source"
_runner_source_valid || { _setstatus prereq wda "device runner sources are missing or unsafe"; die "$RUNNER_SOURCE_ERROR"; }
RUNNER_SOURCE_HASH="$(_runner_source_hash)" \
    || { _setstatus prereq wda "device runner sources could not be read"; die "could not read the device runner sources in $RUNNER_SRC"; }
ok "Device runner source: $RUNNER_SRC (sha256 ${RUNNER_SOURCE_HASH:0:12})"
mkdir -p "$RUNNER_DERIVED_DATA" || die "could not create $RUNNER_DERIVED_DATA"

# Must run before the first xcodebuild that reads the project (see the Xcode
# compatibility helpers): the exported XCODE_XCCONFIG_FILE reaches every build,
# settings query, destination listing and the long-running runner alike.
_prepare_wda_xcconfig \
    || die "could not write the Xcode compatibility xcconfig at $WDA_XCCONFIG_FILE"
if [ -n "$WDA_DEPLOYMENT_TARGET_OVERRIDE" ]; then
    ok "iOS deployment target raised to $WDA_DEPLOYMENT_TARGET_OVERRIDE for this Xcode (via $WDA_XCCONFIG_FILE; sources untouched)"
fi

# BEGIN runner product validation.
_validate_runner_bundle() {
    local app="$1" detail
    if ! detail="$(python3 - "$app" <<'PY_RUNNER'
import os
from pathlib import Path
import plistlib
import sys

app = Path(sys.argv[1])
def fail(message):
    print(message)
    raise SystemExit(1)

if app.is_symlink() or not app.is_dir():
    fail("runner is missing or symlinked")
for directory, dirs, files in os.walk(app, followlinks=False):
    for name in dirs + files:
        if name.endswith('.cstemp'):
            fail("signing temporary file found (possible interrupted signing): "
                 + str((Path(directory) / name).relative_to(app)))
tests = list((app / 'PlugIns').glob('*.xctest'))
if not tests:
    fail("runner contains no PlugIns/*.xctest bundle")
bundles = [app]
for directory, dirs, _files in os.walk(app, followlinks=False):
    for name in dirs:
        if name.endswith(('.framework', '.xctest')):
            bundles.append(Path(directory) / name)
for bundle in bundles:
    relative = str(bundle.relative_to(app))
    info_path = bundle / 'Info.plist'
    if not info_path.is_file():
        fail(relative + '/Info.plist is missing')
    try:
        with info_path.open('rb') as stream:
            info = plistlib.load(stream)
    except (OSError, ValueError, plistlib.InvalidFileException):
        fail(relative + '/Info.plist is invalid')
    executable = info.get('CFBundleExecutable') if isinstance(info, dict) else None
    if (not isinstance(executable, str) or not executable or '/' in executable
            or executable in {'.', '..'}):
        fail(relative + ': invalid CFBundleExecutable')
    binary = bundle / executable
    if not binary.is_file() or not os.access(binary, os.X_OK):
        fail(relative + '/' + executable + ' is missing or not executable')
PY_RUNNER
    )"; then
        WDA_RUNNER_VALIDATION_ERROR="$detail"
        return 1
    fi
    if ! detail="$(codesign --verify --deep --strict "$app" 2>&1)"; then
        WDA_RUNNER_VALIDATION_ERROR="runner signature verification failed: $detail"
        return 1
    fi
    WDA_RUNNER_VALIDATION_ERROR=""
}

# One build-for-testing of the runner. A lock screen seen during the build is
# not a build failure: RUNNER_BUILD_LOCKED hands it to the lock backoff.
_run_runner_prebuild() {
    local build_log="$1"
    _setstatus building "${_BUILD_BLOCKER:-}" "building the device runner"
    : > "$build_log"
    if ! (
        cd "$STATE_DIR" || exit 1
        _runner_xcodebuild \
            -destination "platform=iOS,id=$WDA_UDID" \
            -allowProvisioningUpdates \
            DEVELOPMENT_TEAM="$TEAM_ID" PRODUCT_BUNDLE_IDENTIFIER="$WDA_BUNDLE_ID" \
            build-for-testing
    ) >>"$build_log" 2>&1; then
        if grep -Eiq 'Unlock iPhone to Continue|device is locked|deviceprep.*Code=-3|Code=-3.*deviceprep' "$build_log"; then
            RUNNER_BUILD_LOCKED=1
        fi
        WDA_RUNNER_VALIDATION_ERROR="build-for-testing failed (log: $build_log)"
        return 1
    fi
}

_repair_runner_if_invalid() {
    local products="$1" app="$2" build_log="$3" reason
    if _validate_runner_bundle "$app"; then
        return 0
    fi
    reason="$WDA_RUNNER_VALIDATION_ERROR"
    warn "Runner product invalid: $reason"
    _setstatus building "${_BUILD_BLOCKER:-}" "runner product invalid: $reason; rebuilding once"
    if [ "${WDA_RUNNER_REPAIR_ATTEMPTED:-0}" = "1" ]; then
        _setstatus building-fail wda "runner still invalid after one repair: $reason"
        return 1
    fi
    # Only this instance's own runner app, at its fixed products path, may be
    # discarded. Never clean all of DerivedData or follow an app link.
    case "$products" in /*/Build/Products/*) ;; *) return 1 ;; esac
    if [ "$products" != "$RUNNER_PRODUCTS_DIR" ] \
        || [ "$app" != "$products/$RUNNER_APP_NAME" ] \
        || [ -L "$app" ]; then
        WDA_RUNNER_VALIDATION_ERROR="refusing to remove an unowned runner product"
        return 1
    fi
    if ! python3 - "$products" "$app" <<'PY_PRODUCT_PATH'
from pathlib import Path
import sys
products, app = map(Path, sys.argv[1:])
if (not products.is_absolute() or '/Build/Products/' not in str(products.resolve())
        or app.is_symlink() or app.parent.resolve() != products.resolve()):
    raise SystemExit(1)
PY_PRODUCT_PATH
    then
        WDA_RUNNER_VALIDATION_ERROR="refusing to remove a runner outside canonical build products"
        return 1
    fi
    WDA_RUNNER_REPAIR_ATTEMPTED=1
    rm -rf -- "$app" || return 1
    # Deleting the exact poisoned app also discards its .cstemp leftovers;
    # merely unlinking those files would leave missing framework contents.
    # The rebuild gets its own log: the failed build's log is the only record
    # of which step produced the invalid product (#75).
    local repair_log="${build_log%.log}.repair.log"
    warn "Runner repair: keeping the failed build's log at $build_log; rebuild log at $repair_log"
    if ! _run_runner_prebuild "$repair_log" || ! _validate_runner_bundle "$app"; then
        _setstatus building-fail wda "runner repair failed: $WDA_RUNNER_VALIDATION_ERROR"
        return 1
    fi
    _setstatus building "${_BUILD_BLOCKER:-}" "runner product rebuilt and verified"
}

# Build (incrementally) and verify the runner product, then pick its .xctestrun.
# Sets RUNNER_BUILT_PRODUCTS, RUNNER_APP_PATH and WDA_XCTESTRUN on success.
_ensure_launchable_runner() {
    local products="$RUNNER_PRODUCTS_DIR" app build_log
    build_log="$STATE_DIR/wda-runner-product-build.log"
    app="$products/$RUNNER_APP_NAME"
    _run_runner_prebuild "$build_log" || return 1
    if [ ! -e "$app" ] && [ ! -L "$app" ]; then
        WDA_RUNNER_VALIDATION_ERROR="build-for-testing produced no $RUNNER_APP_NAME (log: $build_log)"
        return 1
    fi
    _repair_runner_if_invalid "$products" "$app" "$build_log" || return 1
    WDA_XCTESTRUN="$(_resolve_xctestrun "$products" "" || true)"
    if [ -z "$WDA_XCTESTRUN" ]; then
        WDA_RUNNER_VALIDATION_ERROR="could not resolve a unique .xctestrun next to $products (run doctor)"
        return 1
    fi
    RUNNER_BUILT_PRODUCTS="$products"
    RUNNER_APP_PATH="$app"
}
# END runner product validation.

# BEGIN runner product cache.
# A verified runner product is recorded before its launch (its products dir +
# .xctestrun) so the next reconnect installs it as-is with
# `test-without-building` instead of running build-for-testing again — even an
# up-to-date incremental build costs xcodebuild seconds on every reconnect.
#
# The record is keyed on everything that changes the product: the runner
# source hash, the signing identity (team, bundle id, ASC key or Xcode
# account), the target device, and the Xcode / SDK / deployment target it was
# built with. Any mismatch, a missing file, or a product that no longer
# validates falls through to the normal build. A launch that names a failure of
# the product itself drops the record so the following round rebuilds.
WDA_RUNNER_CACHE="$STATE_DIR/wda-runner-product.json"
WDA_RUNNER_FROM_CACHE=0

_runner_cache_key() {
    local signer="account"
    if _asc_signing_enabled; then
        signer="asc:${WDA_ASC_KEY_ID:-}"
    fi
    printf 'v3|%s|%s|%s|%s|%s|%s|%s|%s' "${RUNNER_SOURCE_HASH:-}" "${WDA_BUNDLE_ID:-}" \
        "${TEAM_ID:-}" "$signer" "${WDA_UDID:-}" "${XCODE_VERSION:-}" \
        "${WDA_IOS_SDK_VERSION:-}" "${WDA_DEPLOYMENT_TARGET_OVERRIDE:-}"
}

_runner_cache_drop() {
    if [ -L "$WDA_RUNNER_CACHE" ]; then
        warn "refusing to remove a symlinked runner cache record: $WDA_RUNNER_CACHE"
        return 1
    fi
    rm -f "$WDA_RUNNER_CACHE"
}

# Record the verified product. Atomic write; a symlinked path is never followed.
_runner_cache_write() {
    [ -n "${RUNNER_BUILT_PRODUCTS:-}" ] && [ -n "${WDA_XCTESTRUN:-}" ] || return 1
    [ -n "${RUNNER_SOURCE_HASH:-}" ] || return 1
    if [ -L "$WDA_RUNNER_CACHE" ]; then
        warn "refusing to write a symlinked runner cache record: $WDA_RUNNER_CACHE"
        return 1
    fi
    python3 - "$WDA_RUNNER_CACHE" "$(_runner_cache_key)" "$RUNNER_BUILT_PRODUCTS" "$WDA_XCTESTRUN" <<'PY_CACHE'
import json, os, sys, tempfile, time
path, key, products, xctestrun = sys.argv[1:]
record = {"schema_version": 1, "key": key, "products_dir": products,
          "xctestrun": xctestrun, "recorded_at": int(time.time())}
fd, tmp = tempfile.mkstemp(prefix=".wda-runner-product.", dir=os.path.dirname(path))
with os.fdopen(fd, "w", encoding="utf-8") as handle:
    json.dump(record, handle)
os.chmod(tmp, 0o600)
os.replace(tmp, path)
PY_CACHE
}

# Reuse the recorded product when it still matches and still validates.
# On success sets RUNNER_BUILT_PRODUCTS, RUNNER_APP_PATH and WDA_XCTESTRUN.
_runner_cache_read() {
    local products xctestrun app
    [ -n "${RUNNER_SOURCE_HASH:-}" ] || return 1
    [ -f "$WDA_RUNNER_CACHE" ] && [ ! -L "$WDA_RUNNER_CACHE" ] || return 1
    products="$(python3 - "$WDA_RUNNER_CACHE" "$(_runner_cache_key)" "$RUNNER_PRODUCTS_DIR" <<'PY_CACHE'
import json, posixpath, sys
path, key, expected_products = sys.argv[1:]
try:
    with open(path, encoding="utf-8") as handle:
        record = json.load(handle)
except (OSError, ValueError):
    raise SystemExit(1)
if record.get("schema_version") != 1 or record.get("key") != key:
    raise SystemExit(1)
products, xctestrun = record.get("products_dir"), record.get("xctestrun")
if not (isinstance(products, str) and isinstance(xctestrun, str)):
    raise SystemExit(1)
# Only the products directory of this instance is ever reused.
if products.rstrip("/") != expected_products.rstrip("/"):
    raise SystemExit(1)
if "/Build/Products/" not in products or not xctestrun.endswith(".xctestrun"):
    raise SystemExit(1)
# xcodebuild writes the .xctestrun beside the configuration directory:
# .../Build/Products/X.xctestrun next to .../Build/Products/Debug-iphoneos.
parent = posixpath.dirname(products.rstrip("/"))
if posixpath.dirname(xctestrun) != parent:
    raise SystemExit(1)
print(products)
print(xctestrun)
PY_CACHE
)" || return 1
    xctestrun="${products#*$'\n'}"
    products="${products%%$'\n'*}"
    [ -d "$products" ] && [ -f "$xctestrun" ] && [ ! -L "$xctestrun" ] || return 1
    app="$products/$RUNNER_APP_NAME"
    [ -d "$app" ] && [ ! -L "$app" ] || return 1
    _validate_runner_bundle "$app" || return 1
    RUNNER_BUILT_PRODUCTS="$products"
    RUNNER_APP_PATH="$app"
    WDA_XCTESTRUN="$xctestrun"
    WDA_RUNNER_FROM_CACHE=1
    return 0
}
# A launch timeout on a cached product is usually the phone (locked, asleep,
# unplugged), not the product. Only evict when the runner log names an
# install/launch failure of the product itself.
_runner_log_shows_product_failure() {
    grep -Eiq '0xe8008001|Failed to install|MIInstaller|could not launch|not launchable|code signature|invalid signature|provisioning profile' \
        "${1:-/dev/null}" 2>/dev/null
}
# END runner product cache.

if [ -z "${WDA_UDID:-}" ]; then
    # xcodebuild exposes the classic UDID the runner needs. Never use `head -1`:
    # with multiple paired phones, guessing can build/sign/drive the wrong device.
    WDA_DESTINATION_UDIDS="$(cd "$STATE_DIR" \
        && _runner_xcodebuild -showdestinations 2>/dev/null \
        | sed -n 's/.*platform:iOS, arch:arm64.*id:\([0-9A-F-]*\),.*/\1/p' \
        | sort -u || true)"
    WDA_DESTINATION_COUNT="$(printf '%s\n' "$WDA_DESTINATION_UDIDS" \
        | awk 'NF { count++ } END { print count + 0 }')"
    case "$WDA_DESTINATION_COUNT" in
        0) die "no iOS device found — pair the iPhone, enable Developer Mode, and rerun" ;;
        1) WDA_UDID="$WDA_DESTINATION_UDIDS" ;;
        *)
            printf '%s\n' "$WDA_DESTINATION_UDIDS" | sed 's/^/    /' >&2
            die "multiple paired iOS destinations are available; set WDA_UDID=<one exact UDID>"
            ;;
    esac
fi
case "$WDA_UDID" in
    ''|*[!0123456789ABCDEFabcdef-]*) die "target UDID contains invalid characters (expected hex and dashes)" ;;
esac
_refresh_legacy_contracts
# Show WHICH phone was picked (auto-detect grabs the first destination; with
# several paired iPhones it can choose an unavailable one — let the user catch it).
# Over USB lockdownd names the phone in milliseconds, and a target it finds
# attached by cable is the phone in hand, so the paired-device count (a full
# `devicectl list devices`) is only worth reading when it cannot.
PICKED_NAME=""
PICKED_OVER_USB=0
if _device_query info && [ "$(_device_field connection)" = "usb" ]; then
    PICKED_OVER_USB=1
    PICKED_NAME="$(_device_field name)"
    PICKED_IOS="$(_device_field product_version)"
    PICKED_NAME="${PICKED_NAME}${PICKED_IOS:+, iOS $PICKED_IOS}"
else
    PICKED_NAME="$(_devicectl_t 8 device info details --device "$WDA_UDID" \
                  | sed -nE 's/.*[Mm]arketing ?[Nn]ame: *//p' | head -1 || true)"
fi
ok "Device UDID: $WDA_UDID${PICKED_NAME:+  ($PICKED_NAME)}"
if [ "$PICKED_OVER_USB" != "1" ]; then
    IOS_COUNT="$(_devicectl_t 8 list devices | grep -ciE 'iPhone|iPad' || true)"
    if [ "${IOS_COUNT:-0}" -gt 1 ]; then
        warn "$IOS_COUNT iOS devices are paired — if the wrong one was picked, re-run with WDA_UDID=<classic-udid> (the 00008…/8-… id)."
    fi
fi

# ── 3. Wait for dev services (DDI) ────────────────────────────────────────────
# Pitfall: 'Developer Disk Image is not mounted' usually means the phone is
# LOCKED or just-connected — not an Xcode version problem. Keep it unlocked.
_DDI_BLOCKER="$_BUILD_BLOCKER"
if [ "$WDA_ALLOW_LAN" = "1" ]; then
    # Carry the sticky trust blocker through this early phase too: clearing it
    # here made status flip back to a generic "waiting for developer services"
    # between failed attempts while the phone still needed the same manual
    # trust approval (operator-reported: "who knows what we're waiting for").
    _setstatus ddi-wait "$_BUILD_BLOCKER" "waiting for developer services — unlock and keep the iPhone awake"
    if [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
        info "Waiting for developer services (UNLOCK the iPhone and keep it awake)"
    fi
else
    _DDI_BLOCKER="usb"
    _setstatus ddi-wait usb "waiting for developer services — unlock + USB"
    if [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
        info "Waiting for developer services (UNLOCK the iPhone, keep it awake, and plug it in via USB)"
    fi
fi
TRIES=0
# Readiness comes from devicectl's JSON output: current Xcode dropped the
# `ddiServicesAvailable` line from the human-readable text, so grepping the
# text could never pass (#81). The text form stays as a fallback for older
# Xcode builds whose -j output may lack the key.
_ddi_ready() {
    # lockdownd's image mounter says directly whether the developer image is
    # mounted (~50 ms over USB). Anything short of "mounted" falls through to
    # CoreDevice, which also covers Wi-Fi-only phones.
    if _device_query ddi && [ "$(_device_field mounted)" = "true" ]; then
        return 0
    fi
    local j; j="$(mktemp)"
    local text; text="$(_devicectl_t 10 device info details --device "$WDA_UDID" -j "$j")"
    local r=1
    if grep -Eq '"ddiServicesAvailable" *: *true' "$j" 2>/dev/null \
        || printf '%s\n' "$text" | grep -q "ddiServicesAvailable: true"; then
        r=0
    fi
    rm -f "$j"
    return $r
}
until _ddi_ready; do
    TRIES=$((TRIES+1))
    # Keep per-step diagnostics fresh too; the independent 15s heartbeat
    # covers a devicectl call (or another blocking stage) that stalls.
    _setstatus ddi-wait "$_DDI_BLOCKER" "waiting for developer services (attempt $TRIES)"
    if [ $TRIES -gt 45 ]; then
        warn "developer services never became available for $WDA_UDID."
        warn "Most reliable fix: connect this iPhone to the Mac with a USB cable"
        warn "(Wi-Fi-only often sits in 'connecting' and never mounts the disk image),"
        warn "keep it unlocked + awake, then re-run. Devices the Mac currently sees:"
        _devicectl_t 8 list devices | sed 's/^/    /' >&2 || true
        warn "If the wrong phone was picked, re-run with WDA_UDID=<classic-udid>."
        warn "If WARP is connected, verify its effective Excluded routes contain fe80::/10 and fd00::/8."
        warn "Temporarily disconnect WARP only when those Zero Trust Split Tunnel exclusions cannot be added."
        _setstatus ddi-fail ddi "developer services never became available"
        die "developer services not available (docs/wda-setup.html pitfall ①; check WARP/USB)"
    fi
    if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
        if [ "$TRIES" -eq 1 ] && [ "$KEEPALIVE_LOCK_RETRY" != "1" ]; then
            warn "developer services are not ready; KeepAlive will not repeat this prompt on every poll"
        fi
    elif [ $((TRIES % 8)) -eq 1 ]; then
        if [ "$WDA_ALLOW_LAN" = "1" ]; then
            warn "still waiting — UNLOCK the phone and keep the screen on ..."
        else
            warn "still waiting — UNLOCK the phone, keep the screen on, and plug in USB ..."
        fi
    fi
    sleep 4
done
ok "Developer Disk Image mounted"

# ── 3b. Wait for the phone to be unlocked ─────────────────────────────────────
# Launching the runner on a locked phone fails only after xcodebuild's ~70 s
# automation-mode timeout, then KeepAlive retries — minutes of "connecting"
# while the phone simply needed unlocking. Ask CoreDevice first (~0.5-2 s,
# works before WDA exists; hardware-verified: passcodeRequired flips true on
# lock and back to false the moment the phone unlocks, faster than WDA's own
# /wda/locked), publish `locked` at once, and launch the moment it unlocks.
_device_passcode_required() {
    local j out
    j="$(mktemp)"
    _devicectl_t 5 device info lockState --device "$WDA_UDID" -j "$j" >/dev/null
    out="$(python3 - "$j" <<'PY_LOCK' 2>/dev/null || echo unknown
import json, sys
try:
    result = json.load(open(sys.argv[1]))["result"]
    print("true" if result.get("passcodeRequired") is True else "false")
except Exception:
    print("unknown")
PY_LOCK
)"
    rm -f "$j"
    printf '%s\n' "$out"
}
WDA_LOCK_WAIT_SECS="${WDA_LOCK_WAIT_SECS:-300}"
case "$WDA_LOCK_WAIT_SECS" in
    ''|*[!0-9]*) warn "WDA_LOCK_WAIT_SECS='$WDA_LOCK_WAIT_SECS' is not a whole number of seconds; using 300"
        WDA_LOCK_WAIT_SECS=300 ;;
esac
if [ "$(_device_passcode_required)" = "true" ]; then
    info "Waiting for the iPhone to be unlocked"
    _setstatus lock-wait locked "the iPhone is locked — unlock it and connecting continues on its own"
    _lock_wait_started=$SECONDS
    # Unlocked only after two explicit "not required" readings in a row. A
    # reading devicectl failed to produce is not "unlocked": on hardware,
    # failed reads while the phone stayed locked launched the runner into
    # xcodebuild's "Unlock iPhone to Continue".
    _unlocked_reads=0
    while [ "$_unlocked_reads" -lt 2 ]; do
        case "$(_device_passcode_required)" in
            false)
                _unlocked_reads=$((_unlocked_reads + 1))
                continue
                ;;
            true) _unlocked_reads=0 ;;
            *) ;;  # unreadable: neither proof of unlock nor of lock
        esac
        if [ $((SECONDS - _lock_wait_started)) -ge "$WDA_LOCK_WAIT_SECS" ]; then
            if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
                # The quiet locked backoff (5 s → 1 min) takes over.
                _prepare_locked_retry
                exit 1
            fi
            _setstatus building-fail locked "the iPhone stayed locked for ${WDA_LOCK_WAIT_SECS}s"
            die "the iPhone stayed locked for ${WDA_LOCK_WAIT_SECS}s. Unlock it, then rerun setup."
        fi
        sleep 1
    done
    ok "iPhone unlocked after $((SECONDS - _lock_wait_started))s"
fi
_setstatus building "$_BUILD_BLOCKER" "building + launching the device runner"

# ── 4. Build + run the device runner (stays running; this is the server) ─────
info "Building + launching the device runner on the phone (the first build takes a minute or two)"
_stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
    || die "the prior runner PID record does not safely identify a process; refusing to kill anything"
: > "$RUN_LOG"
# `/usr/bin/xcodebuild` is a dispatcher. Once it execs the selected developer
# tool, `ps` reports the real path under Xcode.app; recording the dispatcher
# path therefore makes the exact-process identity check reject a legitimate
# runner. Resolve the executable that the process will actually become.
XCODEBUILD_BIN="$(xcrun --find xcodebuild 2>/dev/null || true)"
[ -n "$XCODEBUILD_BIN" ] && [ -x "$XCODEBUILD_BIN" ] \
    || die "could not resolve the selected Xcode's xcodebuild executable"
WDA_XCTESTRUN=""
if [ "${WDA_RUNNER_REBUILD:-0}" != "1" ] && _runner_cache_read; then
    ok "Reusing the runner product from the last bring-up (no rebuild; WDA_RUNNER_REBUILD=1 forces one)"
    _setstatus building "${_BUILD_BLOCKER:-}" "reusing the verified runner product from the last bring-up"
elif ! _ensure_launchable_runner; then
    if [ "$RUNNER_BUILD_LOCKED" = "1" ]; then
        if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
            _prepare_locked_retry
            exit 1
        fi
        die "the phone is locked and the runner build exited. Unlock it, then rerun setup."
    fi
    if grep -q "No Accounts:" "$STATE_DIR/wda-runner-product-build.log" 2>/dev/null; then
        _report_missing_xcode_account
    fi
    if grep -q "No profiles for .* were found\|requires a provisioning profile" \
        "$STATE_DIR/wda-runner-product-build.log" 2>/dev/null; then
        _setstatus signing-fail account "Xcode could not create the runner provisioning profile"
        # "could not find or create a development provisioning" is the
        # phrase the daemon maps to its `account` blocker; keep it verbatim.
        die "Xcode could not find or create a development provisioning profile for the device runner.
   In Xcode → Settings → Accounts, refresh the selected team, keep the iPhone
   registered, then rerun. With WDA_ASC_* API-key signing, check that the key
   can manage profiles. Build log: $STATE_DIR/wda-runner-product-build.log"
    fi
    _setstatus building-fail wda "$WDA_RUNNER_VALIDATION_ERROR"
    die "device runner product is not launchable: $WDA_RUNNER_VALIDATION_ERROR"
fi
# Record the product as soon as it is verified launchable, not after the runner
# serves. A round that builds a good product and then fails for a reason
# outside it (a locked phone, a dropped device link, a relay error) otherwise
# leaves no record, so every KeepAlive retry would build again (#75).
# A launch that names a failure of the product itself still drops it below.
if [ "$WDA_RUNNER_FROM_CACHE" != "1" ]; then
    _runner_cache_write && ok "Recorded the verified runner product; the next reconnect installs it without rebuilding" \
        || warn "could not record the runner product for reuse; the next reconnect rebuilds"
fi
# Readiness straight from the runner's device port (see the wait below). The
# runner mints a new session id per launch, so the one answering now, if any
# (a runner from the previous round still exiting), is recorded first and
# only a different one counts as this launch serving.
RUNNER_DEVICE_PORT=8100
RUNNER_PROBE=0
RUNNER_PREVIOUS_SESSION=""
if _target_on_usb && _device_tool; then
    RUNNER_PROBE=1
    if _device_query runner-status; then
        RUNNER_PREVIOUS_SESSION="$(_device_field session_id)"
    fi
fi
# Keep `RUNNER_COMMAND=` at column 0 (tests isolate the launch block by it).
# One argv source builds the launch command and its signing suffix, and is
# also the exact PID identity record. No eval or string-based execution.
_prepare_runner_args || die "could not prepare the device runner launch arguments"
RUNNER_COMMAND="$XCODEBUILD_BIN $RUNNER_ARGS"
RUNNER_EXPECTED="runner:$RUNNER_COMMAND"
(
    cd "$STATE_DIR" || exit 1
    exec nohup "$XCODEBUILD_BIN" "${RUNNER_ARGV[@]}"
) > "$RUN_LOG" 2>&1 &
RUNNER_PID=$!
if ! _write_pid_record "$RUNNER_PID_FILE" "$RUNNER_PID" "$RUNNER_EXPECTED" runner; then
    die "xcodebuild did not become the exact expected runner process; no unverified PID was signalled.
   Inspect $RUN_LOG and any listener before retrying."
fi
STARTED_RUNNER=1
RUNNER_PID="$VALIDATED_PID"
ok "PID-verified runner $RUNNER_PID (log: $RUN_LOG)"

info "Waiting for ServerURLHere (or a trust error) ..."
PHONE_URL=""
TRIES=0
BUILD_STARTED_AT="$(date +%s)"
# The runner serves ~2-3 s after launch on a cached product, but xcodebuild
# copies its ServerURLHere line into $RUN_LOG through a block buffer: on an
# iPhone 13 the line landed ~4 s after the server was up, and the old 3 s poll
# added up to 3 s more. Over USB the runner's port is asked directly every
# 0.2 s (a new session id means this launch is serving); the log marker still
# counts, and is what a Wi-Fi-only phone waits for. The failure checks below
# spawn ps/grep and run once per second (TRIES counts those rounds; 360 of
# them is the old 120 x 3 s budget).
READY_POLL_TICK=0
PHONE_URL_FROM_PROBE=0
while [ -z "$PHONE_URL" ]; do
    PHONE_URL="$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' "$RUN_LOG" | head -1)"
    [ -z "$PHONE_URL" ] || break
    if [ "$RUNNER_PROBE" = "1" ] && _device_query runner-status; then
        _probe_session="$(_device_field session_id)"
        if [ -n "$_probe_session" ] && [ "$_probe_session" != "$RUNNER_PREVIOUS_SESSION" ]; then
            PHONE_URL="http://127.0.0.1:$RUNNER_DEVICE_PORT"
            PHONE_URL_FROM_PROBE=1
            break
        fi
    fi
    READY_POLL_TICK=$((READY_POLL_TICK + 1))
    if [ $((READY_POLL_TICK % 5)) -ne 1 ]; then
        sleep 0.2
        continue
    fi
    TRIES=$((TRIES+1))
    if [ $TRIES -gt 360 ]; then
        if [ -n "${WDA_XCTESTRUN:-}" ] && _runner_log_shows_product_failure "$RUN_LOG"; then
            _runner_cache_drop || true
            warn "the recorded runner product failed to install or launch; the next round rebuilds it"
        fi
        if _runner_failure_is_xcode_too_old "$RUN_LOG"; then
            _report_xcode_too_old
        fi
        if _runner_log_shows_automation_mode_disabled "$RUN_LOG"; then
            _report_automation_mode_disabled
        fi
        _setstatus building-fail wda "the device runner did not report its server URL before the startup timeout"
        die "timed out waiting for the device runner to start — check $RUN_LOG"
    fi
    # Read actionable xcodebuild failures before checking whether its PID is
    # still alive. Fast failures can exit between polls; validating the process
    # first used to hide the real account/profile error behind a generic
    # "runner exited" message.
    if grep -q "No Accounts:" "$RUN_LOG" 2>/dev/null; then
        _report_missing_xcode_account
    fi
    if grep -q "No profiles for .* were found\|requires a provisioning profile" \
        "$RUN_LOG" 2>/dev/null; then
        # The product is recorded before launch; one its launch rejected for
        # signing must not be reused by the next round.
        if [ -n "${WDA_XCTESTRUN:-}" ]; then
            _runner_cache_drop || true
        fi
        _setstatus signing-fail account "Xcode could not create the runner provisioning profile"
        # "could not find or create a development provisioning" is the
        # phrase the daemon maps to its `account` blocker; keep it verbatim.
        die "Xcode could not find or create a development provisioning profile for the device runner.
   In Xcode → Settings → Accounts, refresh the selected team, keep the iPhone
   registered, then rerun. If WARP is connected, its effective Excluded routes
   must contain fe80::/10 and fd00::/8 (otherwise disconnect it temporarily)."
    fi
    if grep -Eq "Failed to establish communication with the test runner|A connection to this device could not be established" \
        "$RUN_LOG" 2>/dev/null; then
        warn "the device link dropped while starting the runner (CoreDevice tunnel). Over Wi-Fi this is usually the phone's address changing or the phone sleeping; a USB cable makes it immune."
    fi
    if grep -q "not trusted" "$RUN_LOG" 2>/dev/null; then
        _setstatus trust trust "trust the Apple Development cert on the iPhone"
        die "Developer cert not trusted. On the iPhone: 设置 → 通用 → VPN与设备管理 → 信任 'Apple Development: …', then re-run. (pitfall ②)"
    fi
    if ! _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        if [ "${WDA_KEEPALIVE:-0}" = "1" ] \
            && _wda_failure_is_lock_related log-only; then
            _prepare_locked_retry
            exit 1
        fi
        if _wda_failure_is_lock_related log-only; then
            _setstatus building-fail wda "phone is locked and the device runner exited"
            die "the phone is locked and xcodebuild exited. Unlock it, then rerun setup."
        fi
        # Only evict for a failure of the product itself. A runner can also
        # exit because the device link dropped ("Failed to establish
        # communication with the test runner", the CoreDevice tunnel over
        # Wi-Fi), which says nothing about the built product — evicting there
        # made every flaky connection cost a full rebuild.
        if [ -n "${WDA_XCTESTRUN:-}" ] && _runner_log_shows_product_failure "$RUN_LOG"; then
            _runner_cache_drop || true
            warn "the recorded runner product failed to install or launch; the next round rebuilds it"
        fi
        # After the lock checks (a locked phone has its own blocker) and
        # before the automation check: a refusal from an Xcode too old for
        # the phone otherwise reads as a generic runner failure (#126).
        if _runner_failure_is_xcode_too_old "$RUN_LOG"; then
            _report_xcode_too_old
        fi
        if _runner_log_shows_automation_mode_disabled "$RUN_LOG"; then
            _report_automation_mode_disabled
        fi
        _setstatus building-fail wda "the device runner exited before reporting its server URL"
        die "the PID-verified device runner exited before reporting its server URL — check $RUN_LOG"
    fi
    if _wda_failure_is_lock_related log-only \
        && [ -z "$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' \
            "$RUN_LOG" | head -1)" ]; then
        if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
            _prepare_locked_retry
            exit 1
        fi
        _interactive_lock_wait_tick \
            || die "the phone remained locked for 5 minutes. Unlock it, then rerun setup."
    elif [ $((TRIES % 30)) -eq 0 ]; then
        BUILD_ELAPSED="$(( $(date +%s) - BUILD_STARTED_AT ))"
        _setstatus building "$_BUILD_BLOCKER" "launching the device runner (${BUILD_ELAPSED}s elapsed)"
    fi
    sleep 0.2
done
case "$PHONE_URL" in
    http://*) ;;
    *) die "the device runner reported an unexpected server URL '$PHONE_URL' (plain http:// expected)" ;;
esac
if [ "$PHONE_URL_FROM_PROBE" = "1" ]; then
    ok "device runner serving on device port $RUNNER_DEVICE_PORT (answered over USB, session ${_probe_session:0:8}…)"
else
    ok "device runner serving at $PHONE_URL"
fi
_setstatus serving "" "device runner serving — starting relay"

# ── 5. Localhost relay ────────────────────────────────────────────────────────
# Pitfall (macOS 15+/26): the daemon is a background LaunchAgent and macOS
# Local Network privacy silently blocks its LAN egress — so it must reach WDA
# via 127.0.0.1 (exempt). WDA itself has no HTTP authentication, so a USB relay
# (`iphone-use relay` over usbmuxd, or a legacy iproxy) is mandatory by default. A LAN relay is available only behind the explicit,
# security-reducing WDA_ALLOW_LAN=1 escape hatch.
info "Starting localhost relay on 127.0.0.1:$WDA_PORT"
PHONE_HOSTPORT="${PHONE_URL#http://}"; PHONE_HOSTPORT="${PHONE_HOSTPORT%/}"
PHONE_IP="${PHONE_HOSTPORT%%:*}"; PHONE_WDA_PORT="${PHONE_HOSTPORT##*:}"
_valid_port "$PHONE_WDA_PORT" \
    || die "the device runner reported an invalid device port in '$PHONE_URL'"
_stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay \
    || die "the prior control-relay PID record is not safe to stop; refusing to reuse TCP $WDA_PORT"
_assert_port_free "$WDA_PORT" \
    || die "TCP $WDA_PORT must be free before starting the managed control relay"
: > "$STATE_DIR/wda-relay.log"
TARGET_IS_USB=0
_target_on_usb && TARGET_IS_USB=1
# Readiness seen over USB carries no LAN address. Only the socat fallback (the
# phone left USB since, with WDA_ALLOW_LAN=1) needs one: take it from the
# ServerURLHere line once xcodebuild's buffer delivers it.
if [ "$PHONE_URL_FROM_PROBE" = "1" ] && [ "$TARGET_IS_USB" != "1" ]; then
    _lan_url=""
    for _ in $(seq 1 50); do
        _lan_url="$(sed -n 's/.*ServerURLHere->\(http[^<]*\)<-ServerURLHere.*/\1/p' "$RUN_LOG" | head -1)"
        [ -z "$_lan_url" ] || break
        sleep 0.2
    done
    case "$_lan_url" in
        http://*)
            PHONE_URL="$_lan_url"
            PHONE_HOSTPORT="${PHONE_URL#http://}"; PHONE_HOSTPORT="${PHONE_HOSTPORT%/}"
            PHONE_IP="${PHONE_HOSTPORT%%:*}"; PHONE_WDA_PORT="${PHONE_HOSTPORT##*:}"
            ;;
        *)
            _setstatus serving usb "the configured iPhone disconnected before the control relay started"
            die "the iPhone left USB after its runner started, and its network address is not known yet; reconnect the cable and retry"
            ;;
    esac
fi
RELAY_BIN=""
IPROXY_BIN=""
if [ "$TARGET_IS_USB" = "1" ]; then
    RELAY_BIN="$(_relay_binary || true)"
    [ -n "$RELAY_BIN" ] || IPROXY_BIN="$(command -v iproxy 2>/dev/null || true)"
fi
if [ -n "$RELAY_BIN" ]; then
    RELAY_COMMAND="$RELAY_BIN relay --udid $WDA_UDID --listen 127.0.0.1:$WDA_PORT --device-port $PHONE_WDA_PORT"
    RELAY_EXPECTED="relay:$RELAY_COMMAND"
    nohup "$RELAY_BIN" relay --udid "$WDA_UDID" --listen "127.0.0.1:$WDA_PORT" \
        --device-port "$PHONE_WDA_PORT" > "$STATE_DIR/wda-relay.log" 2>&1 &
    RELAY_PID=$!
    RELAY_DESC="USB relay (usbmuxd) on 127.0.0.1:$WDA_PORT"
elif [ -n "$IPROXY_BIN" ]; then
    RELAY_COMMAND="$IPROXY_BIN -s 127.0.0.1 $WDA_PORT:$PHONE_WDA_PORT -u $WDA_UDID"
    RELAY_EXPECTED="relay:$RELAY_COMMAND"
    nohup "$IPROXY_BIN" -s 127.0.0.1 "$WDA_PORT:$PHONE_WDA_PORT" -u "$WDA_UDID" \
        > "$STATE_DIR/wda-relay.log" 2>&1 &
    RELAY_PID=$!
    RELAY_DESC="USB iproxy on 127.0.0.1:$WDA_PORT"
elif [ "$WDA_ALLOW_LAN" = "1" ] && command -v socat >/dev/null; then
    warn "WDA_ALLOW_LAN=1: the device runner has no authentication; use only on a trusted, isolated LAN"
    printf '%s\n' "$PHONE_IP" | LC_ALL=C grep -Eq '^[A-Za-z0-9.:%_-]+$' \
        || die "the device runner reported a LAN host that is unsafe for socat: '$PHONE_IP'"
    SOCAT_BIN="$(command -v socat)"
    RELAY_COMMAND="$SOCAT_BIN TCP-LISTEN:$WDA_PORT,fork,reuseaddr,bind=127.0.0.1 TCP:$PHONE_IP:$PHONE_WDA_PORT"
    RELAY_EXPECTED="relay:$RELAY_COMMAND"
    nohup "$SOCAT_BIN" "TCP-LISTEN:$WDA_PORT,fork,reuseaddr,bind=127.0.0.1" \
        "TCP:$PHONE_IP:$PHONE_WDA_PORT" > "$STATE_DIR/wda-relay.log" 2>&1 &
    RELAY_PID=$!
    RELAY_DESC="LAN socat on 127.0.0.1:$WDA_PORT to $PHONE_IP:$PHONE_WDA_PORT"
else
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ "$TARGET_IS_USB" != "1" ]; then
        _setstatus serving usb "the configured iPhone disconnected before the control relay started"
    else
        _setstatus serving wda "no permitted control relay tool is available"
    fi
    die "the device layer relays over USB by default. Keep this iPhone connected over USB; if it is,
   the iPhoneUse app is missing or too old to relay — reinstall it or run: iphone-use upgrade.
   The on-phone runner has no HTTP authentication. A LAN relay is therefore disabled
   unless WDA_ALLOW_LAN=1 is explicitly set for a trusted, isolated network."
fi
if ! _write_pid_record "$RELAY_PID_FILE" "$RELAY_PID" "$RELAY_EXPECTED" relay; then
    die "control relay did not become the exact expected process; no unverified PID was signalled.
   Inspect $STATE_DIR/wda-relay.log and TCP $WDA_PORT before retrying."
fi
STARTED_CONTROL_RELAY=1
RELAY_PID="$VALIDATED_PID"
_wait_tcp_listening "$WDA_PORT" || true
_verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
    || die "control relay ownership/bind verification failed.
   Expected only PID $RELAY_PID on 127.0.0.1:$WDA_PORT; inspect $STATE_DIR/wda-relay.log"
ok "PID-verified control relay $RELAY_PID: $RELAY_DESC"
curl -fsS -m 5 "http://127.0.0.1:$WDA_PORT/status" >/dev/null \
    || die "relay up but the device runner is not answering through it — check $STATE_DIR/wda-relay.log"
ok "device runner reachable at http://127.0.0.1:$WDA_PORT"
warn "The Mac relay is loopback-only, but the runner on the iPhone has no HTTP authentication.
   Keep the iPhone on a trusted, isolated network even when the Mac relay uses USB."

# ── 5b. MJPEG relay (live video for agent mode — /agent/mjpeg) ─────────────────
# The runner serves an MJPEG screen stream on the device's :9100, inside the
# same XCUITest session as control — so live video and driving coexist. The daemon
# needs this stream, so setup does not publish a video URL unless relay ownership and
# an initial stream byte are both verified.
PHONE_MJPEG_PORT=9100
_stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg \
    || die "the prior video-relay PID record is not safe to stop; refusing to reuse TCP $MJPEG_PORT"
_assert_port_free "$MJPEG_PORT" \
    || die "TCP $MJPEG_PORT must be free before starting the managed video relay"
: > "$STATE_DIR/wda-mjpeg-relay.log"
if [ -n "$RELAY_BIN" ]; then
    MJPEG_RELAY_COMMAND="$RELAY_BIN relay --udid $WDA_UDID --listen 127.0.0.1:$MJPEG_PORT --device-port $PHONE_MJPEG_PORT"
    MJPEG_RELAY_EXPECTED="mjpeg:$MJPEG_RELAY_COMMAND"
    nohup "$RELAY_BIN" relay --udid "$WDA_UDID" --listen "127.0.0.1:$MJPEG_PORT" \
        --device-port "$PHONE_MJPEG_PORT" > "$STATE_DIR/wda-mjpeg-relay.log" 2>&1 &
    MJPEG_RELAY_PID=$!
    MJPEG_RELAY_DESC="USB relay (usbmuxd) on 127.0.0.1:$MJPEG_PORT"
elif [ -n "$IPROXY_BIN" ]; then
    MJPEG_RELAY_COMMAND="$IPROXY_BIN -s 127.0.0.1 $MJPEG_PORT:$PHONE_MJPEG_PORT -u $WDA_UDID"
    MJPEG_RELAY_EXPECTED="mjpeg:$MJPEG_RELAY_COMMAND"
    nohup "$IPROXY_BIN" -s 127.0.0.1 "$MJPEG_PORT:$PHONE_MJPEG_PORT" -u "$WDA_UDID" \
        > "$STATE_DIR/wda-mjpeg-relay.log" 2>&1 &
    MJPEG_RELAY_PID=$!
    MJPEG_RELAY_DESC="USB iproxy on 127.0.0.1:$MJPEG_PORT"
elif [ "$WDA_ALLOW_LAN" = "1" ] && command -v socat >/dev/null; then
    MJPEG_RELAY_COMMAND="$SOCAT_BIN TCP-LISTEN:$MJPEG_PORT,fork,reuseaddr,bind=127.0.0.1 TCP:$PHONE_IP:$PHONE_MJPEG_PORT"
    MJPEG_RELAY_EXPECTED="mjpeg:$MJPEG_RELAY_COMMAND"
    nohup "$SOCAT_BIN" "TCP-LISTEN:$MJPEG_PORT,fork,reuseaddr,bind=127.0.0.1" \
        "TCP:$PHONE_IP:$PHONE_MJPEG_PORT" > "$STATE_DIR/wda-mjpeg-relay.log" 2>&1 &
    MJPEG_RELAY_PID=$!
    MJPEG_RELAY_DESC="LAN socat on 127.0.0.1:$MJPEG_PORT to $PHONE_IP:$PHONE_MJPEG_PORT"
else
    if [ "$WDA_ALLOW_LAN" = "0" ] && [ "$TARGET_IS_USB" != "1" ]; then
        _setstatus serving usb "the configured iPhone disconnected before the video relay started"
    else
        _setstatus serving wda "no permitted video relay tool is available"
    fi
    die "Direct video requires the same permitted relay path as control.
   Keep the iPhone on USB, or explicitly use WDA_ALLOW_LAN=1 only on a trusted, isolated LAN."
fi
if ! _write_pid_record "$MJPEG_RELAY_PID_FILE" "$MJPEG_RELAY_PID" \
    "$MJPEG_RELAY_EXPECTED" mjpeg; then
    die "video relay did not become the exact expected process; no unverified PID was signalled.
   Inspect $STATE_DIR/wda-mjpeg-relay.log and TCP $MJPEG_PORT before retrying."
fi
STARTED_MJPEG_RELAY=1
MJPEG_RELAY_PID="$VALIDATED_PID"
_wait_tcp_listening "$MJPEG_PORT" || true
_verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
    || die "video relay ownership/bind verification failed.
   Expected only PID $MJPEG_RELAY_PID on 127.0.0.1:$MJPEG_PORT; inspect $STATE_DIR/wda-mjpeg-relay.log"
MJPEG_PROBE_FILE="$STATE_DIR/.wda-mjpeg-probe.$$"
curl -fsS -m 8 "http://127.0.0.1:$MJPEG_PORT" 2>/dev/null \
    | head -c 1 > "$MJPEG_PROBE_FILE" || true
if [ ! -s "$MJPEG_PROBE_FILE" ]; then
    rm -f "$MJPEG_PROBE_FILE"
    die "video relay owns 127.0.0.1:$MJPEG_PORT but no MJPEG data arrived within 8s.
   The daemon configuration was not changed; inspect $STATE_DIR/wda-mjpeg-relay.log."
fi
rm -f "$MJPEG_PROBE_FILE"
ok "PID-verified video relay $MJPEG_RELAY_PID: $MJPEG_RELAY_DESC"

# ── 6. Point the daemon at the verified direct endpoints ───────────────────────
TARGET_URL="http://127.0.0.1:$WDA_PORT"
TARGET_MJPEG_URL="http://127.0.0.1:$MJPEG_PORT"
DAEMON_JOB_LOADED=0
DAEMON_HTTP_READY=0
DAEMON_PORT=44321

if [ -f "$DAEMON_PLIST" ]; then
    info "Configuring the iphone-use daemon for the direct backend"
    DAEMON_WAS_DISABLED="$(_job_disabled_state "$DAEMON_LABEL")" \
        || die "could not snapshot the daemon's launchd disabled policy"
    cp -p "$DAEMON_PLIST" "$DAEMON_ROLLBACK_PLIST" \
        || die "could not back up the daemon plist before changing its backend"
    if launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        DAEMON_JOB_WAS_LOADED=1
    fi
    DAEMON_TRANSACTION_ACTIVE=1
    DAEMON_STAGED_PLIST="${DAEMON_PLIST}.install.$$"
    cp -p "$DAEMON_PLIST" "$DAEMON_STAGED_PLIST" \
        || die "could not stage the daemon plist for an atomic update"
    chmod 600 "$DAEMON_STAGED_PLIST" \
        || die "could not secure the staged daemon plist"
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables" "$DAEMON_STAGED_PLIST" >/dev/null 2>&1 \
        || /usr/libexec/PlistBuddy -c "Add :EnvironmentVariables dict" "$DAEMON_STAGED_PLIST"

    CURRENT_BACKEND="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_BACKEND" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_UDID="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_UDID" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_URL="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_URL" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_MJPEG_URL="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_MJPEG_URL" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_MANAGED="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_WDA_MANAGED" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CURRENT_ALLOW_LAN="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:WDA_ALLOW_LAN" "$DAEMON_STAGED_PLIST" 2>/dev/null || true)"
    CONFIG_CHANGED=0
    # Restart the daemon only for settings it reads at startup. WDA_ALLOW_LAN
    # is not one of them (setup reads it from this plist), and a restart drops
    # every in-flight request, hold and owner lease — a USB<->Wi-Fi relay change
    # used to do exactly that mid-session.
    DAEMON_NEEDS_RESTART=0
    CHANGED_KEYS=""
    [ "$CURRENT_BACKEND" != "direct" ] && CHANGED_KEYS="$CHANGED_KEYS PHONE_REMOTE_BACKEND"
    [ "$CURRENT_UDID" != "$WDA_UDID" ] && CHANGED_KEYS="$CHANGED_KEYS PHONE_REMOTE_UDID"
    [ "$CURRENT_URL" != "$TARGET_URL" ] && CHANGED_KEYS="$CHANGED_KEYS PHONE_REMOTE_WDA_URL"
    [ "$CURRENT_MJPEG_URL" != "$TARGET_MJPEG_URL" ] && CHANGED_KEYS="$CHANGED_KEYS PHONE_REMOTE_WDA_MJPEG_URL"
    [ "$CURRENT_MANAGED" != "true" ] && CHANGED_KEYS="$CHANGED_KEYS PHONE_REMOTE_WDA_MANAGED"
    [ -n "$CHANGED_KEYS" ] && DAEMON_NEEDS_RESTART=1
    [ "$CURRENT_ALLOW_LAN" != "$WDA_ALLOW_LAN" ] && CHANGED_KEYS="$CHANGED_KEYS WDA_ALLOW_LAN"
    if [ -n "$CHANGED_KEYS" ]; then
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_BACKEND direct
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_UDID "$WDA_UDID"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_URL "$TARGET_URL"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_MJPEG_URL "$TARGET_MJPEG_URL"
        _plist_set_env "$DAEMON_STAGED_PLIST" PHONE_REMOTE_WDA_MANAGED true
        _plist_set_env "$DAEMON_STAGED_PLIST" WDA_ALLOW_LAN "$WDA_ALLOW_LAN"
        CONFIG_CHANGED=1
        ok "daemon plist set to managed direct + fixed device + runner control/video endpoints (changed:$CHANGED_KEYS)"
    else
        ok "daemon plist already has the managed direct + fixed device + runner endpoint configuration"
    fi

    plutil -lint "$DAEMON_STAGED_PLIST" >/dev/null 2>&1 \
        || die "staged daemon LaunchAgent plist is invalid after configuration"
    if [ "$CONFIG_CHANGED" = "1" ]; then
        DAEMON_TOUCHED=1
        mv -f "$DAEMON_STAGED_PLIST" "$DAEMON_PLIST" \
            || die "could not atomically install the configured daemon plist"
    else
        rm -f "$DAEMON_STAGED_PLIST"
    fi
    DAEMON_STAGED_PLIST=""

    if [ "$DAEMON_NEEDS_RESTART" = "1" ] \
        || ! launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        DAEMON_TOUCHED=1
        launchctl bootout "$GUI_DOMAIN/$DAEMON_LABEL" 2>/dev/null || true
        _wait_job_gone "$DAEMON_LABEL" \
            || die "daemon LaunchAgent did not finish stopping"
        launchctl enable "$GUI_DOMAIN/$DAEMON_LABEL" 2>/dev/null || true
        if ! launchctl bootstrap "$GUI_DOMAIN" "$DAEMON_PLIST" 2>/dev/null; then
            die "the device runner is reachable, but the daemon LaunchAgent could not be bootstrapped"
        fi
    fi
    if launchctl print "$GUI_DOMAIN/$DAEMON_LABEL" >/dev/null 2>&1; then
        DAEMON_JOB_LOADED=1
        ok "daemon LaunchAgent job loaded"
    else
        warn "daemon LaunchAgent loaded state could not be verified"
    fi

    DAEMON_PORT="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_PORT" "$DAEMON_PLIST" 2>/dev/null || true)"
    [ -n "$DAEMON_PORT" ] || DAEMON_PORT=44321
    if [ "$DAEMON_JOB_LOADED" = "1" ]; then
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            if curl -sS -m 2 -o /dev/null "http://127.0.0.1:$DAEMON_PORT/" 2>/dev/null; then
                DAEMON_HTTP_READY=1
                break
            fi
            sleep 0.5
        done
        if [ "$DAEMON_HTTP_READY" = "1" ]; then
            ok "daemon HTTP endpoint verified on 127.0.0.1:$DAEMON_PORT"
        else
            warn "daemon job is loaded, but its HTTP endpoint is not verified; check its error log"
        fi
    fi
else
    warn "daemon LaunchAgent not found; the runner can be verified, but the product daemon cannot be started"
    printf '    PHONE_REMOTE_BACKEND=direct PHONE_REMOTE_WDA_URL=%s PHONE_REMOTE_WDA_MJPEG_URL=%s iphone-use serve\n' \
        "$TARGET_URL" "$TARGET_MJPEG_URL"
fi

# ── 7. Hand the proven runner/relays to the dedicated launchd supervisor ───────
SUPERVISOR_VERIFIED=0
MJPEG_READY=0
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    if launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
        && _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
        RPID="$VALIDATED_PID"
    else
        RPID=""
    fi
    if [ -n "$RPID" ] \
        && _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
        && _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
        && curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1; then
        SUPERVISOR_VERIFIED=1
        MJPEG_READY=1
    fi
else
    _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner \
        || die "interactive runner identity was lost before launchd handoff"
    HANDOFF_OLD_ID="$PID_RECORD_PID|$PID_RECORD_LSTART"
    _setstatus supervisor "" "handing the device runner to its launchd supervisor"
    info "Handing the verified runner setup to its dedicated launchd supervisor"
    _install_wda_supervisor \
        || die "the device runner is reachable now, but its launchd supervisor could not be installed"

    # Bootstrap starts a fresh supervisor-owned setup process. Verify that it
    # replaced the interactive runner (not merely that launchctl accepted XML)
    # and that the replacement still answers /status.
    HANDOFF_TRIES=0
    while [ "$HANDOFF_TRIES" -lt 60 ]; do
        HANDOFF_TRIES=$((HANDOFF_TRIES + 1))
        HANDOFF_NEW_ID=""
        if _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner; then
            HANDOFF_NEW_ID="$PID_RECORD_PID|$PID_RECORD_LSTART"
        fi
        if [ -n "$HANDOFF_NEW_ID" ] \
            && [ "$HANDOFF_NEW_ID" != "$HANDOFF_OLD_ID" ] \
            && [ -n "$PID_RECORD_LSTART" ] \
            && launchctl print "$GUI_DOMAIN/$WDA_AGENT_LABEL" >/dev/null 2>&1 \
            && _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
            && _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
            && grep -q '"phase":"ready"' "$STATUS_FILE" 2>/dev/null \
            && curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1; then
            SUPERVISOR_VERIFIED=1
            MJPEG_READY=1
            break
        fi
        sleep 2
    done
    if [ "$SUPERVISOR_VERIFIED" != "1" ]; then
        _setstatus supervisor-fail wda "launchd handoff not verified"
        die "runner launchd job loaded, but its replacement runner was not verified within 120s.
   Check: $WDA_AGENT_LOG
   Then:  $SELF_INSTALL status"
    fi
    ok "launchd replacement verified: runner identity, both loopback relays, and runner /status"
fi

if [ "$SUPERVISOR_VERIFIED" != "1" ] || [ "$MJPEG_READY" != "1" ]; then
    _setstatus supervisor-fail wda "the device runner is reachable but launchd supervision is unverified"
    die "the device runner endpoint is up, but dedicated launchd supervision could not be verified"
fi

# Post-handoff verdict from one `/agent/status` body. Prints exactly one word:
#   drivable   — the phone can act right now (setup and runtime both good)
#   reachable  — the daemon reaches WDA through the relays; the phone cannot
#                act yet (locked, automation pending, or the probe is running)
#   down       — the daemon does not see WDA at all (real handoff failure)
# `"wda"` is matched with its opening quote so `managed_wda` / `wda_actionable`
# never satisfy it.
_daemon_product_verdict() {
    local status="${1:-}"
    if printf '%s' "$status" | grep -Eq '"drivable"[[:space:]]*:[[:space:]]*true'; then
        printf 'drivable\n'
    elif printf '%s' "$status" | grep -Eq '"wda"[[:space:]]*:[[:space:]]*true'; then
        printf 'reachable\n'
    else
        printf 'down\n'
    fi
}

# Whether the daemon read the device as locked (`wda_locked:true`). `null`
# (unknown) and `false` both return 1.
_daemon_status_reports_locked() {
    printf '%s' "${1:-}" | grep -Eq '"wda_locked"[[:space:]]*:[[:space:]]*true'
}

# Polling budget for the verdict: 0.5s per try. The daemon probes WDA every
# ~2s with a 20s action timeout, so `wda:true` can take a couple of probe
# rounds to surface even when the relay came up instantly.
DAEMON_STATUS_MAX_TRIES="${DAEMON_STATUS_MAX_TRIES:-120}"
# Once WDA is reachable, keep waiting this many tries for drivable before
# accepting the handoff as-is (an unlocked phone usually clears within it).
DAEMON_REACHABLE_GRACE_TRIES="${DAEMON_REACHABLE_GRACE_TRIES:-20}"

# The daemon intentionally refreshes Direct health in the background: the first
# /agent/status after a cold start can return its conservative cached `offline`
# value while also starting the real WDA actionability probe. Do not announce a
# successful product setup during that window. Poll the authenticated product
# status until callers can actually use it, and fail closed if the cache never
# becomes actionable.
DAEMON_PRODUCT_READY=0
if [ "$DAEMON_HTTP_READY" = "1" ]; then
    DAEMON_AGENT_SECRET="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_AGENT_TOKEN" "$DAEMON_PLIST" 2>/dev/null || true)"
    if [ -z "$DAEMON_AGENT_SECRET" ]; then
        DAEMON_AGENT_SECRET="$(/usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:PHONE_REMOTE_PASSWORD" "$DAEMON_PLIST" 2>/dev/null || true)"
    fi
    # Two different questions hide behind that status, and only the first is a
    # setup verdict:
    #   1. did the handoff work — does the daemon reach WDA through the relays
    #      (`wda:true`)?
    #   2. can the phone act right now — unlocked, automation mode granted
    #      (`drivable:true`)?
    # The daemon keeps `reconnecting=true` (hence drivable=false) until its own
    # action-level probe succeeds, which a locked phone defers indefinitely.
    # Gating on drivable therefore failed every daemon-initiated rebuild while
    # the phone sat locked, and the rollback below killed a perfectly healthy
    # WDA each time — 62 KeepAlive failures in one evening (2026-09-05).
    # A reachable-but-locked phone is a runtime hint for the daemon and the web
    # client, never a reason to tear the runner down.
    DAEMON_STATUS_TRIES=0
    DAEMON_PRODUCT_VERDICT=down
    DAEMON_REACHABLE_TRIES=0
    while [ "$DAEMON_STATUS_TRIES" -lt "$DAEMON_STATUS_MAX_TRIES" ]; do
        DAEMON_STATUS_TRIES=$((DAEMON_STATUS_TRIES + 1))
        if [ -n "$DAEMON_AGENT_SECRET" ]; then
            DAEMON_STATUS="$(curl -sS -m 2 -H "Authorization: Bearer $DAEMON_AGENT_SECRET" \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        else
            DAEMON_STATUS="$(curl -sS -m 2 \
                "http://127.0.0.1:$DAEMON_PORT/agent/status" 2>/dev/null || true)"
        fi
        DAEMON_PRODUCT_VERDICT="$(_daemon_product_verdict "$DAEMON_STATUS")"
        if [ "$DAEMON_PRODUCT_VERDICT" = "drivable" ]; then
            DAEMON_PRODUCT_READY=1
            break
        fi
        if [ "$DAEMON_PRODUCT_VERDICT" = "reachable" ]; then
            # Give an unlocked phone a moment to finish the action probe, then
            # accept the handoff as-is instead of waiting out the whole budget.
            DAEMON_REACHABLE_TRIES=$((DAEMON_REACHABLE_TRIES + 1))
            if [ "$DAEMON_REACHABLE_TRIES" -ge "$DAEMON_REACHABLE_GRACE_TRIES" ]; then
                DAEMON_PRODUCT_READY=1
                break
            fi
        fi
        sleep 0.5
    done
    DAEMON_LOCKED_HINT=0
    if _daemon_status_reports_locked "$DAEMON_STATUS"; then
        DAEMON_LOCKED_HINT=1
    fi
    DAEMON_STATUS=""
    DAEMON_AGENT_SECRET=""
    if [ "$DAEMON_PRODUCT_READY" != "1" ]; then
        _setstatus daemon-fail wda "daemon never reached the device runner after a verified handoff"
        die "the device runner, relays, and launchd supervision are verified, but the daemon did not report wda=true within $((DAEMON_STATUS_MAX_TRIES / 2))s.
   Inspect: ~/Library/Logs/iPhoneUse/iphone-use.err"
    fi
    if [ "$DAEMON_PRODUCT_VERDICT" = "drivable" ]; then
        ok "daemon product status verified: drivable=true"
    elif [ "$DAEMON_LOCKED_HINT" = "1" ]; then
        ok "daemon product status verified: device runner reachable through the relays"
        warn "the iPhone is locked — unlock it once; the daemon keeps probing and reports drivable=true as soon as the runner can act"
    else
        ok "daemon product status verified: device runner reachable through the relays"
        warn "the device runner answers but cannot act yet (drivable=false) — keep the iPhone unlocked and awake; the daemon keeps probing"
    fi
fi

SUPERVISOR_HANDOFF_COMPLETE=1
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    _reset_keepalive_retry \
        || warn "could not clear KeepAlive retry state after a verified recovery"
fi
rm -f "$WDA_AGENT_ROLLBACK_PLIST" "$DAEMON_ROLLBACK_PLIST" "$SELF_INSTALL_ROLLBACK"
SUPERVISOR_TRANSACTION_ACTIVE=0
DAEMON_TRANSACTION_ACTIVE=0
SELF_INSTALL_REPLACED_THIS_RUN=0
_setstatus ready "" "device runner and launchd supervisor verified"
printf '\n%s\n' "${BOLD}━━━ Device layer verified ━━━${RST}"
printf '  Runner    : %s (on-phone), %s (verified relay)\n' "$PHONE_URL" "$TARGET_URL"
printf '  Supervisor: %s (job, runner, and /status verified)\n' "$GUI_DOMAIN/$WDA_AGENT_LABEL"
printf '  Video     : %s (startup stream + relay ownership verified)\n' "$TARGET_MJPEG_URL"
if [ "$DAEMON_HTTP_READY" = "1" ]; then
    printf '  Daemon    : http://127.0.0.1:%s (verified)\n' "$DAEMON_PORT"
else
    printf '  Daemon    : not HTTP-verified; inspect ~/Library/Logs/iPhoneUse/iphone-use.err\n'
fi
printf '  Try       : curl -H "Authorization: Bearer %s" http://127.0.0.1:%s/agent/elements\n' \
    "\$PW" "$DAEMON_PORT"
printf '  Stop      : %s stop\n' "$SELF_INSTALL"
printf '  Pause     : %s pause  (give the phone back without auto-restart)\n' "$SELF_INSTALL"
printf '  Resume    : %s resume\n' "$SELF_INSTALL"
printf '  Source    : %s (sha256 %s)\n' "$RUNNER_SRC" "${RUNNER_SOURCE_HASH:0:12}"
printf '  Signing   : free Apple ID profiles may expire after 7 days; re-run setup when needed.\n'

# One health probe, and what it means. Returns 0 to keep holding, 1 to rebuild.
#
# A single unanswered probe cannot tell a transient busy period, a network
# hiccup, and a genuinely dead runner apart — and treating one 4s timeout as a
# verdict tore down the runner and both relays during heavy element reads.
# Requiring consecutive non-answers keeps the distinction the probe cannot
# make from becoming a decision. Process death and a vanished listener are
# unambiguous and still exit at once.
_keepalive_probe_verdict() {
    if curl -fsS -m 4 "$TARGET_URL/status" >/dev/null 2>&1; then
        KEEPALIVE_PROBE_FAILURES=0
        return 0
    fi
    KEEPALIVE_PROBE_FAILURES=$((KEEPALIVE_PROBE_FAILURES + 1))
    if [ "$KEEPALIVE_PROBE_FAILURES" -ge "$KEEPALIVE_PROBE_MAX_FAILURES" ]; then
        return 1
    fi
    info "the device runner did not answer /status within 4s (${KEEPALIVE_PROBE_FAILURES}/${KEEPALIVE_PROBE_MAX_FAILURES}); the runner and relays are alive, so holding"
    return 0
}

# In supervisor mode, stay alive while the runner and verified relay do. launchd
# sees an exit as a failure and rebuilds incrementally after sleep/USB/tunnel
# failures. Interactive setup returned above only after handing off to this job.
if [ "${WDA_KEEPALIVE:-0}" = "1" ]; then
    info "KeepAlive mode: holding while the PID-verified runner and relays stay healthy"
    KEEPALIVE_EXIT_CAUSE=runner
    # Consecutive `/status` non-answers before a rebuild (~32s at a 4s probe
    # on a 10s interval). Fixed on purpose: a configurable threshold would need
    # validating, and a non-numeric or zero value would break the bound it
    # exists to provide.
    KEEPALIVE_PROBE_MAX_FAILURES=3
    KEEPALIVE_PROBE_FAILURES=0
    while :; do
        KEEPALIVE_EXIT_CAUSE=runner
        _validate_pid_record "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || break
        RPID="$VALIDATED_PID"
        KEEPALIVE_EXIT_CAUSE=relay
        _verify_loopback_listener "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay "$WDA_PORT" \
            || break
        _verify_loopback_listener "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg "$MJPEG_PORT" \
            || break
        # The processes are alive; whether WDA is answering is the one question
        # left, and how many unanswered probes make that a failure belongs to
        # `_keepalive_probe_verdict`.
        KEEPALIVE_EXIT_CAUSE=unreachable
        _keepalive_probe_verdict || break
        sleep 10
    done
    if _wda_failure_is_lock_related; then
        _prepare_locked_retry
        exit 1
    fi
    # Name the cause. "runner/relay went down" was the same sentence for a
    # dead runner and for the common LAN case where everything is alive but
    # the phone's Wi-Fi address moved — which cost a session an hour of
    # reading logs before someone compared the relay's target IP to the
    # phone's current one.
    case "$KEEPALIVE_EXIT_CAUSE" in
        unreachable)
            # Report the observation, not a diagnosis: this loop cannot tell
            # which cause it was, and naming one sent a session hunting the
            # wrong thing.
            warn "the device runner did not answer /status ${KEEPALIVE_PROBE_FAILURES} times in a row while the runner and both relays stayed alive — rebuilding"
            _setstatus building "" "the device runner did not answer ${KEEPALIVE_PROBE_FAILURES} consecutive /status probes — rebuilding"
            ;;
        relay)
            warn "the runner relay stopped listening — exiting so launchd KeepAlive rebuilds it"
            ;;
        *)
            warn "the device runner exited — exiting so launchd KeepAlive rebuilds it"
            ;;
    esac
    _stop_managed_process "$MJPEG_RELAY_PID_FILE" "$LEGACY_MJPEG_EXPECTED" mjpeg || true
    _stop_managed_process "$RELAY_PID_FILE" "$LEGACY_RELAY_EXPECTED" relay || true
    _stop_managed_process "$RUNNER_PID_FILE" "$LEGACY_RUNNER_EXPECTED" runner || true
    exit 1
fi
