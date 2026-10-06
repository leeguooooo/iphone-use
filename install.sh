#!/usr/bin/env bash
# install.sh — install iPhoneUse.app and register the GUI-session LaunchAgent
#
# USAGE (recommended — no auth required; GITHUB_TOKEN not needed for public releases):
#   curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
#
# Local / dev usage (skip download; supply a pre-built .app):
#   ./install.sh /path/to/iPhoneUse.app
#
# A second phone, driven by its own daemon (#67):
#   ./install.sh --instance NAME --udid <UDID> [--port N] [/path/to/iPhoneUse.app]
#   curl -fsSL .../install.sh | sh -s -- --instance NAME --udid <UDID>
# A named instance gets its own copy of the app, state dir
# (~/.iphone-use/instances/NAME), launchd labels, token and ports. It never
# touches the default instance or the shared app; then run its setup-wda.sh.
#
# Requirements:
#   • Must run as the LOGGED-IN GUI user (not root, not over a bare SSH session).
#   • Aqua (WindowServer) session must be active.
#   • macOS 14 Sonoma or later.
#
set -eu

# Configuration paths are derived immediately below, so reject an unset HOME
# before `set -u` can turn it into an opaque unbound-variable failure.
if [ -z "${HOME:-}" ]; then
    printf 'ERROR: HOME is not set. Run as the logged-in desktop user, not root.\n' >&2
    exit 1
fi
umask 077

# ── Inline ad-hoc signer ──────────────────────────────────────────────────────
# Direct mode uses this only when the supplied app has no valid signature. It
# creates no certificate and never changes the user's keychain search list.
#
# Order matters and is not obvious: `iphone-use` is the bundle's MAIN
# executable, so codesign treats signing it as signing the bundle and validates
# nested code as it goes. With `iphone-use-mcp` still only linker-signed at
# that point, that validation fails with "code object is not signed at all /
# In subcomponent: .../iphone-use-mcp" and the whole install rolls back. The
# nested helper must be signed BEFORE anything that pulls in bundle
# validation. Covered by scripts/test-install-adhoc-signing.sh.
_inline_sign() {
    local app="$1"
    codesign --force --sign - "$app/Contents/MacOS/iphone-use-mcp"
    codesign --force --sign - "$app/Contents/MacOS/iphone-use"
    codesign --force --sign - "$app"
    ok "Ad-hoc signed without creating a certificate or changing keychain state"
}

# ── Configuration ─────────────────────────────────────────────────────────────
BUNDLE_ID="com.leeguoo.iphone-use"
APP_NAME="iPhoneUse.app"
INSTALL_DIR="$HOME/Applications"
PLIST_LABEL="com.leeguoo.iphone-use"
PLIST_DST="$HOME/Library/LaunchAgents/${PLIST_LABEL}.plist"
OLD_PLIST_LABEL="work.pwtk.iphone-remote"
OLD_PLIST="$HOME/Library/LaunchAgents/${OLD_PLIST_LABEL}.plist"
WDA_PLIST_LABEL="${PLIST_LABEL}.wda"
WDA_PLIST_DST="$HOME/Library/LaunchAgents/${WDA_PLIST_LABEL}.plist"
LOG_DIR="$HOME/Library/Logs/iPhoneUse"
REPO="leeguooooo/iphone-use"
BINARY_INSIDE_APP="Contents/MacOS/iphone-use"
MCP_BINARY_INSIDE_APP="Contents/MacOS/iphone-use-mcp"

# --instance/--udid/--port select a named instance; anything else is the
# optional local app path, as before.
INSTANCE_NAME="default"
INSTANCE_UDID=""
INSTANCE_PORT=""
INSTALL_ARGS=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        --instance|--udid|--port)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                printf 'ERROR: %s requires a value\n' "$1" >&2
                exit 2
            fi
            case "$1" in
                --instance) INSTANCE_NAME="$2" ;;
                --udid) INSTANCE_UDID="$2" ;;
                --port) INSTANCE_PORT="$2" ;;
            esac
            shift 2
            ;;
        *)
            INSTALL_ARGS+=("$1")
            shift
            ;;
    esac
done
set -- ${INSTALL_ARGS[@]+"${INSTALL_ARGS[@]}"}
if [ "$INSTANCE_NAME" = default ] && { [ -n "$INSTANCE_UDID" ] || [ -n "$INSTANCE_PORT" ]; }; then
    printf 'ERROR: --udid/--port are accepted only with --instance NAME\n' >&2
    exit 2
fi

# A piped script has no trustworthy sibling directory. In particular, `$0`
# commonly names the shell, so deriving `./scripts/...` from it would allow the
# caller's current working directory to inject helpers into `curl | sh`.
SCRIPT_SOURCE="${BASH_SOURCE[0]:-}"
SCRIPT_IS_LOCAL=0
SCRIPT_DIR=""
case "$SCRIPT_SOURCE" in
    ""|"-"|/dev/fd/*|/proc/*) ;;
    *)
        if [ "$SCRIPT_SOURCE" = "$0" ] && [ -f "$SCRIPT_SOURCE" ]; then
            SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_SOURCE")" 2>/dev/null && pwd || true)"
            [ -z "$SCRIPT_DIR" ] || SCRIPT_IS_LOCAL=1
        fi
        ;;
esac

TMPDIR_INSTALL=""
BOOTSTRAP_TMP=""
SIGN_SH_DL=""
APP_STAGE=""
APP_BACKUP=""
APP_HAD_EXISTING=0
APP_REPLACED=0
APP_COMMITTED=0
PLIST_STAGE=""
PLIST_BACKUP=""
PLIST_HAD_EXISTING=0
PLIST_REPLACED=0
PLIST_COMMITTED=0
SETUP_WDA_STAGE=""
SETUP_WDA_DL=""
SETUP_WDA_BACKUP=""
SETUP_WDA_HAD_EXISTING=0
SETUP_WDA_REPLACED=0
SETUP_WDA_COMMITTED=0
UNINSTALL_STAGE=""
UNINSTALL_DL=""
UNINSTALL_BACKUP=""
UNINSTALL_HAD_EXISTING=0
UNINSTALL_REPLACED=0
UNINSTALL_COMMITTED=0
WDA_RUNTIME_TOUCHED=0
WDA_TRANSITION_COMMITTED=0
PREINSTALL_WDA_LOADED=0
PREINSTALL_WDA_DISABLED=0
DAEMON_RUNTIME_TOUCHED=0
DAEMON_RUNTIME_COMMITTED=0
PREINSTALL_DAEMON_LOADED=0
PREINSTALL_DAEMON_DISABLED=0
PREINSTALL_MANUAL_DAEMON_STOPPED=0
PREINSTALL_OLD_DAEMON_LOADED=0
PREINSTALL_OLD_DAEMON_DISABLED=0
OLD_PLIST_DISABLED="${OLD_PLIST}.disabled"
OLD_PLIST_STAGED=""
OLD_DISABLED_BACKUP=""
OLD_DISABLED_HAD_EXISTING=0
OLD_PLIST_REPLACED=0
OLD_PLIST_COMMITTED=0
RELEASE_REF=""
RELEASE_COMMIT=""
LAUNCHD_DISABLED_SNAPSHOT=""
SKILL_CANONICAL_DIR="$HOME/.agents/skills/iphone-use"
SKILL_DEFAULT_LOCK_PATH="$HOME/.agents/.skill-lock.json"
SKILL_XDG_LOCK_PATH=""
SKILL_BACKUP_DIR=""
SKILL_STAGE_DIR=""
SKILL_EXPECTED_DL=""
SKILL_SYNC_TOUCHED=0
SKILL_SYNC_COMMITTED=0
SKILL_SYNCED=0
SKILL_SYNC_KIND=""
SKILL_SYNC_REF=""
SKILL_SYNC_COMMIT=""
SKILL_SYNC_SHA256=""
SKILL_DEFAULT_LOCK_BEFORE_STATE=""
SKILL_DEFAULT_LOCK_AFTER_STATE=""
SKILL_XDG_LOCK_BEFORE_STATE=""
SKILL_XDG_LOCK_AFTER_STATE=""
SKILL_CANONICAL_BEFORE_STATE=""
SKILL_CANONICAL_AFTER_STATE=""
SKILL_CLAUDE_LINK="$HOME/.claude/skills/iphone-use"
SKILL_CLAUDE_LINK_TOUCHED=0
SKILL_CLAUDE_HOME_CREATED=0
SKILL_CLAUDE_SKILLS_CREATED=0
SKILL_CLAUDE_DISCOVERY=0
SKILL_CLAUDE_BEFORE_STATE=""
SKILL_CLAUDE_AFTER_STATE=""
SKILL_INSTALL_SIGNAL_PENDING=0
BOOTSTRAP_SIGNAL_PENDING=0
INSTALL_COMMIT_SIGNAL_PENDING=0
INSTALL_DAEMON_COMMITTED=0

_restore_app() {
    [ "$APP_REPLACED" = "1" ] || return 0
    [ "$APP_COMMITTED" = "0" ] || return 0
    if [ "$APP_HAD_EXISTING" = "1" ] && [ -d "$APP_BACKUP" ]; then
        rm -rf "$DEST" 2>/dev/null || true
        if mv "$APP_BACKUP" "$DEST" 2>/dev/null; then
            APP_BACKUP=""
            warn "Restored the previous iPhoneUse.app after an incomplete install."
        else
            warn "Could not restore the previous app from $APP_BACKUP"
        fi
    else
        rm -rf "$DEST" 2>/dev/null || true
    fi
}

_restore_daemon_plist() {
    [ "$PLIST_REPLACED" = "1" ] || return 0
    [ "$PLIST_COMMITTED" = "0" ] || return 0
    if [ "$PLIST_HAD_EXISTING" = "1" ] && [ -f "$PLIST_BACKUP" ]; then
        rm -f "$PLIST_DST" 2>/dev/null || true
        if mv "$PLIST_BACKUP" "$PLIST_DST" 2>/dev/null; then
            PLIST_BACKUP=""
            warn "Restored the previous LaunchAgent plist after an incomplete install."
        else
            warn "Could not restore the previous plist from $PLIST_BACKUP"
        fi
    else
        rm -f "$PLIST_DST" 2>/dev/null || true
    fi
}

_restore_setup_wda() {
    [ "$SETUP_WDA_REPLACED" = "1" ] || return 0
    [ "$SETUP_WDA_COMMITTED" = "0" ] || return 0
    if [ "$SETUP_WDA_HAD_EXISTING" = "1" ] && [ -f "$SETUP_WDA_BACKUP" ]; then
        local restore_tmp
        restore_tmp="${SETUP_WDA_DST}.restore.$$"
        if cp -p "$SETUP_WDA_BACKUP" "$restore_tmp" 2>/dev/null \
            && mv -f "$restore_tmp" "$SETUP_WDA_DST" 2>/dev/null; then
            rm -f "$SETUP_WDA_BACKUP" 2>/dev/null || true
            SETUP_WDA_BACKUP=""
            warn "Restored the previous setup-wda.sh after an incomplete install."
        else
            rm -f "$restore_tmp" 2>/dev/null || true
            warn "Could not restore the previous setup-wda.sh from $SETUP_WDA_BACKUP"
        fi
    else
        rm -f "$SETUP_WDA_DST" 2>/dev/null || true
    fi
}

_restore_uninstall() {
    [ "$UNINSTALL_REPLACED" = "1" ] || return 0
    [ "$UNINSTALL_COMMITTED" = "0" ] || return 0
    if [ "$UNINSTALL_HAD_EXISTING" = "1" ] && [ -f "$UNINSTALL_BACKUP" ]; then
        local restore_tmp
        restore_tmp="${UNINSTALL_DST}.restore.$$"
        if cp -p "$UNINSTALL_BACKUP" "$restore_tmp" 2>/dev/null \
            && mv -f "$restore_tmp" "$UNINSTALL_DST" 2>/dev/null; then
            rm -f "$UNINSTALL_BACKUP" 2>/dev/null || true
            UNINSTALL_BACKUP=""
            warn "Restored the previous uninstall.sh after an incomplete install."
        else
            rm -f "$restore_tmp" 2>/dev/null || true
            warn "Could not restore the previous uninstall.sh from $UNINSTALL_BACKUP"
        fi
    else
        rm -f "$UNINSTALL_DST" 2>/dev/null || true
    fi
}

_restore_old_daemon_plist() {
    [ "$OLD_PLIST_REPLACED" = "1" ] || return 0
    [ "$OLD_PLIST_COMMITTED" = "0" ] || return 0

    if [ -f "$OLD_PLIST_DISABLED" ]; then
        if mv -f "$OLD_PLIST_DISABLED" "$OLD_PLIST" 2>/dev/null; then
            OLD_PLIST_STAGED=""
        else
            warn "Could not restore the previous legacy LaunchAgent plist from $OLD_PLIST_DISABLED"
        fi
    elif [ -n "$OLD_PLIST_STAGED" ] && [ -f "$OLD_PLIST_STAGED" ]; then
        if mv -f "$OLD_PLIST_STAGED" "$OLD_PLIST" 2>/dev/null; then
            OLD_PLIST_STAGED=""
        else
            warn "Could not restore the previous legacy LaunchAgent plist from $OLD_PLIST_STAGED"
        fi
    fi

    if [ "$OLD_DISABLED_HAD_EXISTING" = "1" ] \
        && [ -n "$OLD_DISABLED_BACKUP" ] \
        && [ -f "$OLD_DISABLED_BACKUP" ]; then
        if mv -f "$OLD_DISABLED_BACKUP" "$OLD_PLIST_DISABLED" 2>/dev/null; then
            OLD_DISABLED_BACKUP=""
        else
            warn "Could not restore the previous disabled legacy plist from $OLD_DISABLED_BACKUP"
        fi
    fi
}

_skill_lock_state() {
    local path="$1"
    local sha256
    if [ -L "$path" ]; then
        printf 'unsafe\n'
    elif [ -f "$path" ]; then
        sha256="$(/usr/bin/shasum -a 256 "$path" 2>/dev/null \
            | awk '{print $1}')" || {
                printf 'unsafe\n'
                return 0
            }
        if printf '%s' "$sha256" | grep -Eq '^[0-9a-f]{64}$'; then
            printf 'sha256:%s\n' "$sha256"
        else
            printf 'unsafe\n'
        fi
    elif [ -e "$path" ]; then
        printf 'unsafe\n'
    else
        printf 'absent\n'
    fi
}

_verify_skill_lock_without_floating_entry() {
    local path="$1"
    local state
    local owner

    state="$(_skill_lock_state "$path")"
    case "$state" in
        absent)
            return 0
            ;;
        sha256:*)
            owner="$(stat -f '%u' "$path" 2>/dev/null || true)"
            [ "$owner" = "$UID_NUM" ] || return 1
            /usr/bin/plutil -convert json -o - "$path" >/dev/null 2>&1 \
                || return 1
            ;;
        *)
            return 1
            ;;
    esac
    if /usr/bin/plutil -extract 'skills.iphone-use' json -o - \
        "$path" >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

_restore_skill_lock() {
    local key="$1"
    local path="$2"
    local expected_state="$3"
    local present_marker="$SKILL_BACKUP_DIR/${key}.present"
    local backup="$SKILL_BACKUP_DIR/${key}.original"
    local parent
    local current_state

    [ -n "$path" ] || return 0
    [ -n "$expected_state" ] || return 1
    current_state="$(_skill_lock_state "$path")"
    if [ "$current_state" != "$expected_state" ]; then
        warn "Skills lock changed concurrently; preserving it instead of overwriting: $path"
        return 1
    fi
    parent="$(dirname "$path")"
    if [ -e "$path" ] || [ -L "$path" ]; then
        rm -f "$path" 2>/dev/null || return 1
    fi
    if [ -f "$present_marker" ]; then
        [ -f "$backup" ] || return 1
        mkdir -p "$parent" 2>/dev/null || return 1
        mv "$backup" "$path" 2>/dev/null || return 1
    fi
}

_transaction_node_state() {
    local path="$1"
    local target
    local identity
    if [ -L "$path" ]; then
        target="$(readlink "$path" 2>/dev/null || true)"
        [ -n "$target" ] && printf 'symlink:%s\n' "$target" || printf 'unsafe\n'
    elif [ -e "$path" ]; then
        identity="$(stat -f '%d:%i' "$path" 2>/dev/null || true)"
        [ -n "$identity" ] && printf 'node:%s\n' "$identity" || printf 'unsafe\n'
    else
        printf 'absent\n'
    fi
}

_claude_skill_link_state() {
    _transaction_node_state "$SKILL_CLAUDE_LINK"
}

_skill_path_digest() {
    local path="$1"
    local sha256
    if [ -L "$path" ]; then
        printf 'unsafe\n'
    elif [ -f "$path" ]; then
        sha256="$(/usr/bin/shasum -a 256 "$path" 2>/dev/null \
            | awk '{print $1}')" || {
                printf 'unsafe\n'
                return 0
            }
        if printf '%s' "$sha256" | grep -Eq '^[0-9a-f]{64}$'; then
            printf 'sha256:%s\n' "$sha256"
        else
            printf 'unsafe\n'
        fi
    elif [ -e "$path" ]; then
        printf 'unsafe\n'
    else
        printf 'absent\n'
    fi
}

_canonical_skill_state() {
    local root="${1:-$SKILL_CANONICAL_DIR}"
    local identity
    local entry_count
    local skill_state
    local marker_state
    local target
    if [ -L "$root" ]; then
        target="$(readlink "$root" 2>/dev/null || true)"
        [ -n "$target" ] && printf 'symlink:%s\n' "$target" || printf 'unsafe\n'
    elif [ -d "$root" ]; then
        identity="$(stat -f '%d:%i' "$root" 2>/dev/null || true)"
        entry_count="$(find "$root" -mindepth 1 -maxdepth 1 \
            -print 2>/dev/null | wc -l | tr -d '[:space:]')"
        skill_state="$(_skill_path_digest "$root/SKILL.md")"
        marker_state="$(_skill_path_digest "$root/.iphone-use-release")"
        if [ -n "$identity" ] \
            && printf '%s' "$entry_count" | grep -Eq '^[0-9]+$' \
            && [ "$skill_state" != "unsafe" ] \
            && [ "$marker_state" != "unsafe" ]; then
            printf 'dir:%s:%s:%s:%s\n' \
                "$identity" "$entry_count" "$skill_state" "$marker_state"
        else
            printf 'unsafe\n'
        fi
    elif [ -e "$root" ]; then
        identity="$(stat -f '%d:%i' "$root" 2>/dev/null || true)"
        [ -n "$identity" ] && printf 'node:%s\n' "$identity" || printf 'unsafe\n'
    else
        printf 'absent\n'
    fi
}

_restore_claude_skill_link() {
    local failed=0
    local expected_state
    local current_state

    if [ "$SKILL_CLAUDE_LINK_TOUCHED" = "1" ]; then
        expected_state="${SKILL_CLAUDE_AFTER_STATE:-$SKILL_CLAUDE_BEFORE_STATE}"
        current_state="$(_claude_skill_link_state)"
        if [ -z "$expected_state" ] \
            || [ "$current_state" != "$expected_state" ]; then
            warn "Claude skill target changed concurrently; preserving it instead of overwriting: $SKILL_CLAUDE_LINK"
            failed=1
        elif [ -f "$SKILL_BACKUP_DIR/claude-link.present" ]; then
            if [ -e "$SKILL_BACKUP_DIR/claude-link.original" ] \
                || [ -L "$SKILL_BACKUP_DIR/claude-link.original" ]; then
                if [ -e "$SKILL_CLAUDE_LINK" ] || [ -L "$SKILL_CLAUDE_LINK" ]; then
                    rm -rf "$SKILL_CLAUDE_LINK" 2>/dev/null || failed=1
                fi
                if [ "$failed" = "0" ]; then
                    mv "$SKILL_BACKUP_DIR/claude-link.original" \
                        "$SKILL_CLAUDE_LINK" 2>/dev/null || failed=1
                fi
            elif [ ! -e "$SKILL_CLAUDE_LINK" ] && [ ! -L "$SKILL_CLAUDE_LINK" ]; then
                failed=1
            fi
        elif [ -e "$SKILL_CLAUDE_LINK" ] || [ -L "$SKILL_CLAUDE_LINK" ]; then
            rm -rf "$SKILL_CLAUDE_LINK" 2>/dev/null || failed=1
        fi
    fi

    if [ "$SKILL_CLAUDE_SKILLS_CREATED" = "1" ]; then
        rmdir "$HOME/.claude/skills" 2>/dev/null || true
    fi
    if [ "$SKILL_CLAUDE_HOME_CREATED" = "1" ]; then
        rmdir "$HOME/.claude" 2>/dev/null || true
    fi
    return "$failed"
}

_restore_skill_state() {
    local failed=0
    local canonical_failed=0
    local canonical_expected_state
    local canonical_current_state
    [ "$SKILL_SYNC_TOUCHED" = "1" ] || return 0
    [ "$SKILL_SYNC_COMMITTED" = "0" ] || return 0
    [ -n "$SKILL_BACKUP_DIR" ] && [ -d "$SKILL_BACKUP_DIR" ] || return 1

    canonical_expected_state="${SKILL_CANONICAL_AFTER_STATE:-$SKILL_CANONICAL_BEFORE_STATE}"
    canonical_current_state="$(_canonical_skill_state)"
    if [ -z "$canonical_expected_state" ] \
        || [ "$canonical_current_state" != "$canonical_expected_state" ]; then
        warn "Canonical iphone-use skill changed concurrently; preserving it and its discovery/lock state: $SKILL_CANONICAL_DIR"
        failed=1
    else
        _restore_claude_skill_link || failed=1

        if [ -f "$SKILL_BACKUP_DIR/canonical.present" ]; then
            if [ -e "$SKILL_BACKUP_DIR/canonical.original" ] \
                || [ -L "$SKILL_BACKUP_DIR/canonical.original" ]; then
                if [ -e "$SKILL_CANONICAL_DIR" ] || [ -L "$SKILL_CANONICAL_DIR" ]; then
                    rm -rf "$SKILL_CANONICAL_DIR" 2>/dev/null || canonical_failed=1
                fi
                mkdir -p "$(dirname "$SKILL_CANONICAL_DIR")" 2>/dev/null \
                    || canonical_failed=1
                if [ "$canonical_failed" = "0" ]; then
                    mv "$SKILL_BACKUP_DIR/canonical.original" \
                        "$SKILL_CANONICAL_DIR" 2>/dev/null || canonical_failed=1
                fi
                [ "$canonical_failed" = "0" ] || failed=1
            elif [ ! -e "$SKILL_CANONICAL_DIR" ] && [ ! -L "$SKILL_CANONICAL_DIR" ]; then
                # The transaction was marked touched immediately before moving
                # the old directory. If that move never happened, the original
                # must still be present at the canonical path.
                failed=1
            fi
        elif [ -e "$SKILL_CANONICAL_DIR" ] || [ -L "$SKILL_CANONICAL_DIR" ]; then
            rm -rf "$SKILL_CANONICAL_DIR" 2>/dev/null || failed=1
        fi

        _restore_skill_lock \
            "lock-default" \
            "$SKILL_DEFAULT_LOCK_PATH" \
            "${SKILL_DEFAULT_LOCK_AFTER_STATE:-$SKILL_DEFAULT_LOCK_BEFORE_STATE}" \
            || failed=1
        if [ -n "$SKILL_XDG_LOCK_PATH" ] \
            && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ]; then
            _restore_skill_lock \
                "lock-xdg" \
                "$SKILL_XDG_LOCK_PATH" \
                "${SKILL_XDG_LOCK_AFTER_STATE:-$SKILL_XDG_LOCK_BEFORE_STATE}" \
                || failed=1
        fi
    fi

    if [ "$failed" = "0" ]; then
        SKILL_SYNC_TOUCHED=0
        rm -rf "$SKILL_BACKUP_DIR" 2>/dev/null || true
        SKILL_BACKUP_DIR=""
        warn "Restored the previous agent skill after an incomplete install."
        return 0
    fi
    warn "Agent skill rollback was incomplete; recovery backup retained at $SKILL_BACKUP_DIR"
    return 1
}

_restore_launchd_job() {
    local label="$1"
    local plist="$2"
    local was_loaded="$3"
    local was_disabled="$4"
    local target="gui/$UID_NUM/$label"
    local _

    launchctl bootout "$target" >/dev/null 2>&1 || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        launchctl print "$target" >/dev/null 2>&1 || break
        sleep 0.5
    done
    if [ "$was_loaded" = "1" ]; then
        if [ ! -f "$plist" ]; then
            warn "Cannot restore previously loaded $label because its plist is missing: $plist"
            return 1
        fi
        launchctl enable "$target" >/dev/null 2>&1 \
            || { warn "Could not enable $label while restoring its previous loaded state."; return 1; }
        if ! launchctl bootstrap "gui/$UID_NUM" "$plist" >/dev/null 2>&1; then
            sleep 1
            launchctl bootout "$target" >/dev/null 2>&1 || true
            sleep 1
            launchctl bootstrap "gui/$UID_NUM" "$plist" >/dev/null 2>&1 \
                || { warn "Could not bootstrap the restored job from $plist"; return 1; }
        fi
        launchctl kickstart -k "$target" >/dev/null 2>&1 \
            || { warn "Could not restart the restored job $target"; return 1; }
        launchctl print "$target" >/dev/null 2>&1 \
            || { warn "Restored job did not remain loaded: $target"; return 1; }
    fi

    if [ "$was_disabled" = "1" ]; then
        launchctl disable "$target" >/dev/null 2>&1 \
            || { warn "Could not restore the disabled state for $target"; return 1; }
    else
        launchctl enable "$target" >/dev/null 2>&1 \
            || { warn "Could not restore the enabled state for $target"; return 1; }
    fi
}

_restore_daemon_runtime() {
    local failed=0
    [ "$DAEMON_RUNTIME_TOUCHED" = "1" ] || return 0
    [ "$DAEMON_RUNTIME_COMMITTED" = "0" ] || return 0

    _restore_launchd_job "$PLIST_LABEL" "$PLIST_DST" \
        "$PREINSTALL_DAEMON_LOADED" "$PREINSTALL_DAEMON_DISABLED" \
        || failed=1
    _restore_launchd_job "$OLD_PLIST_LABEL" "$OLD_PLIST" \
        "$PREINSTALL_OLD_DAEMON_LOADED" "$PREINSTALL_OLD_DAEMON_DISABLED" \
        || failed=1
    if [ "$failed" = "0" ]; then
        DAEMON_RUNTIME_TOUCHED=0
        warn "Restored the previous daemon enabled/loaded state after an incomplete install."
    else
        warn "Daemon runtime rollback was incomplete; inspect the two product LaunchAgent labels."
    fi
}

_restore_wda_transition() {
    [ "$WDA_RUNTIME_TOUCHED" = "1" ] || return 0
    [ "$WDA_TRANSITION_COMMITTED" = "0" ] || return 0

    if _restore_launchd_job "$WDA_PLIST_LABEL" "$WDA_PLIST_DST" \
        "$PREINSTALL_WDA_LOADED" "$PREINSTALL_WDA_DISABLED"; then
        WDA_RUNTIME_TOUCHED=0
        warn "Restored the previous WDA supervisor enabled/loaded state after an incomplete install."
    else
        warn "WDA supervisor rollback was incomplete; recovery plist retained at $WDA_PLIST_DST"
        return 1
    fi
}

_installer_cleanup() {
    local status=$?
    trap - EXIT
    if [ "$status" -ne 0 ] \
        && [ "$SKILL_SYNC_KIND" = "bootstrap" ] \
        && [ "$INSTALL_DAEMON_COMMITTED" = "1" ]; then
        # The pinned outer installer owns the companion-skill commit. Once
        # this inner process has committed daemon state, it must report
        # success so the outer process cannot roll the matching skill back
        # because of a later signal, closed output pipe, or summary failure.
        status=0
    fi
    if [ "$status" -ne 0 ]; then
        _restore_skill_state || true
        _restore_daemon_plist
        _restore_old_daemon_plist
        _restore_uninstall
        _restore_setup_wda
        _restore_runner_sources
        _restore_app
        _restore_wda_transition || true
        _restore_daemon_runtime
        if [ "$PREINSTALL_MANUAL_DAEMON_STOPPED" = "1" ]; then
            warn "The previously manual daemon was safely stopped for LaunchAgent takeover and was not restarted after rollback. Rerun the installer after fixing the reported error."
        fi
    fi
    [ -z "$APP_STAGE" ] || rm -rf "$APP_STAGE" 2>/dev/null || true
    [ -z "$PLIST_STAGE" ] || rm -f "$PLIST_STAGE" 2>/dev/null || true
    [ -z "$SIGN_SH_DL" ] || rm -f "$SIGN_SH_DL" 2>/dev/null || true
    [ -z "$SETUP_WDA_STAGE" ] || rm -f "$SETUP_WDA_STAGE" 2>/dev/null || true
    [ -z "$SETUP_WDA_DL" ] || rm -f "$SETUP_WDA_DL" 2>/dev/null || true
    [ -z "$UNINSTALL_STAGE" ] || rm -f "$UNINSTALL_STAGE" 2>/dev/null || true
    [ -z "$UNINSTALL_DL" ] || rm -f "$UNINSTALL_DL" 2>/dev/null || true
    [ -z "$RUNNER_SRC_STAGE" ] || rm -rf "$RUNNER_SRC_STAGE" 2>/dev/null || true
    [ -z "$RUNNER_SRC_DL" ] || rm -rf "$RUNNER_SRC_DL" 2>/dev/null || true
    [ -z "$SKILL_STAGE_DIR" ] || rm -rf "$SKILL_STAGE_DIR" 2>/dev/null || true
    [ -z "$SKILL_EXPECTED_DL" ] || rm -f "$SKILL_EXPECTED_DL" 2>/dev/null || true
    [ -z "$TMPDIR_INSTALL" ] || rm -rf "$TMPDIR_INSTALL" 2>/dev/null || true
    [ -z "$BOOTSTRAP_TMP" ] || rm -rf "$BOOTSTRAP_TMP" 2>/dev/null || true
    [ -z "$APP_BACKUP" ] \
        || warn "App recovery backup retained at: $APP_BACKUP"
    [ -z "$PLIST_BACKUP" ] \
        || warn "LaunchAgent recovery backup retained at: $PLIST_BACKUP"
    [ -z "$SETUP_WDA_BACKUP" ] \
        || warn "setup-wda.sh recovery backup retained at: $SETUP_WDA_BACKUP"
    [ -z "$UNINSTALL_BACKUP" ] \
        || warn "uninstall.sh recovery backup retained at: $UNINSTALL_BACKUP"
    [ -z "$RUNNER_SRC_BACKUP" ] || [ ! -d "$RUNNER_SRC_BACKUP" ] \
        || warn "Device runner source recovery backup retained at: $RUNNER_SRC_BACKUP"
    [ -z "$OLD_DISABLED_BACKUP" ] \
        || warn "Legacy disabled-plist recovery backup retained at: $OLD_DISABLED_BACKUP"
    [ -z "$OLD_PLIST_STAGED" ] \
        || warn "Legacy active-plist recovery backup retained at: $OLD_PLIST_STAGED"
    [ -z "$SKILL_BACKUP_DIR" ] \
        || warn "Agent skill recovery backup retained at: $SKILL_BACKUP_DIR"
    exit "$status"
}
trap _installer_cleanup EXIT
trap 'exit 130' HUP INT TERM

# ── Colours ───────────────────────────────────────────────────────────────────
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BOLD='\033[1m'; RESET='\033[0m'
ok()   { printf "${GREEN}✓${RESET} %s\n" "$*"; }
warn() { printf "${YELLOW}⚠${RESET}  %s\n" "$*"; }
die()  { printf "${RED}✗ ERROR:${RESET} %s\n" "$*" >&2; exit 1; }
info() { printf "  %s\n" "$*"; }

# ── `iphone-use` on PATH ──────────────────────────────────────────────────────
# The daemon binary doubles as the CLI (`iphone-use upgrade`, `stop`, ...).
# Link it into ~/.local/bin (IPHONE_USE_BIN_DIR overrides) so the family's
# upgrade command works without knowing the app path. Only a missing path or a
# link that already points at an iPhoneUse.app executable is (re)written; any
# other file there belongs to someone else and is left alone. The link target
# is the stable app path, so an upgrade or rollback never leaves it dangling.
# Runs after the daemon transaction commits; a failure here only warns.
install_cli_link() {
    local target="$1"
    local dir="${IPHONE_USE_BIN_DIR:-$HOME/.local/bin}"
    local link="$dir/iphone-use"
    local current
    if [ -L "$link" ]; then
        current="$(readlink "$link" 2>/dev/null || true)"
        case "$current" in
            "$target") ;;
            */iPhoneUse.app/Contents/MacOS/iphone-use) ;;
            *)
                warn "Left $link alone: it points at ${current:-an unreadable target}, not iphone-use."
                info "Run the CLI as: $target"
                return 0
                ;;
        esac
    elif [ -e "$link" ]; then
        warn "Left $link alone: it is not a link this installer made."
        info "Run the CLI as: $target"
        return 0
    fi
    if [ "${current:-}" != "$target" ]; then
        if ! (umask 022 && mkdir -p "$dir") 2>/dev/null \
            || ! ln -sfn "$target" "$link" 2>/dev/null; then
            warn "Could not link $link; run the CLI as: $target"
            return 0
        fi
    fi
    ok "Command line: $link -> $target"
    case ":${PATH:-}:" in
        *":$dir:"*) ;;
        *) warn "$dir is not on PATH; add it to use \`iphone-use upgrade\` (or run $target)." ;;
    esac
    return 0
}

validate_release_ref() {
    printf '%s' "$1" | grep -Eq '^[A-Za-z0-9][A-Za-z0-9._-]*$'
}

validate_release_commit() {
    printf '%s' "$1" | grep -Eq '^[0-9A-Fa-f]{40}$'
}

# macOS before Sonoma prints `=> true`; current releases print `=> disabled`.
# Matching only `true` read every disabled label as enabled, so upgrades
# re-enabled a parked WDA supervisor and rollback restored the wrong state.
launchd_label_is_disabled() {
    local label="$1"
    printf '%s\n' "$LAUNCHD_DISABLED_SNAPSHOT" \
        | awk -v wanted="\"$label\"" \
            '$1 == wanted && $2 == "=>" && $3 ~ /^(true|disabled)[,;]?$/ { found = 1 }
             END { exit(found ? 0 : 1) }'
}

probe_daemon_control_plane() {
    local target="gui/$UID_NUM/$PLIST_LABEL"
    local lsof_bin="${IPHONE_USE_LSOF_BIN:-/usr/sbin/lsof}"
    local ps_bin="${IPHONE_USE_PS_BIN:-/bin/ps}"
    local before
    local after
    local pid
    local pid_after
    local program
    local program_after
    local process_uid
    local process_command

    [ -x "$lsof_bin" ] && [ -x "$ps_bin" ] || return 1
    before="$(launchctl print "$target" 2>/dev/null)" || return 1
    pid="$(printf '%s\n' "$before" \
        | sed -nE 's/^[[:space:]]*pid = ([0-9]+).*$/\1/p' \
        | head -1)"
    program="$(printf '%s\n' "$before" \
        | sed -n 's/^[[:space:]]*program = //p' \
        | head -1)"
    printf '%s' "$pid" | grep -Eq '^[0-9]+$' || return 1
    [ "$pid" -gt 1 ] 2>/dev/null || return 1
    [ "$program" = "$BINARY_PATH" ] || return 1

    process_uid="$("$ps_bin" -p "$pid" -o uid= 2>/dev/null \
        | tr -d '[:space:]')" || return 1
    [ "$process_uid" = "$UID_NUM" ] || return 1
    process_command="$("$ps_bin" -ww -p "$pid" -o command= 2>/dev/null \
        | sed -E 's/^[[:space:]]*//')" || return 1
    case "$process_command" in
        "$BINARY_PATH"|"${BINARY_PATH} "*) ;;
        *) return 1 ;;
    esac

    "$lsof_bin" -nP -a -p "$pid" -iTCP:"$PORT" -sTCP:LISTEN \
        >/dev/null 2>&1 || return 1
    curl -fsS --noproxy '*' -m 2 -o /dev/null "$DAEMON_PROBE_URL" \
        2>/dev/null || return 1

    after="$(launchctl print "$target" 2>/dev/null)" || return 1
    pid_after="$(printf '%s\n' "$after" \
        | sed -nE 's/^[[:space:]]*pid = ([0-9]+).*$/\1/p' \
        | head -1)"
    program_after="$(printf '%s\n' "$after" \
        | sed -n 's/^[[:space:]]*program = //p' \
        | head -1)"
    [ "$pid_after" = "$pid" ] && [ "$program_after" = "$BINARY_PATH" ] \
        || return 1
    DAEMON_PID="$pid"
}

is_loopback_wda_url() {
    local url="$1"
    local port
    printf '%s' "$url" \
        | grep -Eq '^http://(127\.0\.0\.1|localhost):[0-9]{1,5}/?$' \
        || return 1
    port="$(printf '%s' "$url" | sed -E 's#^http://[^:]+:([0-9]+)/?$#\1#')"
    [ "$port" -ge 1 ] 2>/dev/null && [ "$port" -le 65535 ] 2>/dev/null
}

is_network_wda_url() {
    local url="$1"
    local port
    printf '%s' "$url" \
        | grep -Eq '^https?://[A-Za-z0-9.-]+:[0-9]{1,5}/?$' \
        || return 1
    port="$(printf '%s' "$url" | sed -E 's#^https?://[^:]+:([0-9]+)/?$#\1#')"
    [ "$port" -ge 1 ] 2>/dev/null && [ "$port" -le 65535 ] 2>/dev/null
}

resolve_release_ref() {
    local ref="${IPHONE_USE_RELEASE_REF:-}"
    local effective
    if [ -n "$ref" ]; then
        validate_release_ref "$ref" \
            || die "Invalid IPHONE_USE_RELEASE_REF='$ref' (expected one release tag, without slashes)."
        printf '%s' "$ref"
        return 0
    fi

    command -v curl >/dev/null 2>&1 \
        || die "curl is required. Install Xcode Command Line Tools: xcode-select --install"
    effective="$(curl -fsSL -o /dev/null -w '%{url_effective}' \
        "https://github.com/$REPO/releases/latest")"
    case "$effective" in
        */releases/tag/*) ref="${effective##*/releases/tag/}" ;;
        *) die "Could not resolve the latest release tag (got '$effective')." ;;
    esac
    validate_release_ref "$ref" \
        || die "GitHub returned an unsafe release tag: '$ref'"
    printf '%s' "$ref"
}

resolve_release_commit() {
    local ref="$1"
    local requested_commit="${IPHONE_USE_RELEASE_COMMIT:-}"
    local commit=""
    local response=""
    local remote_refs=""
    local peeled_ref="refs/tags/$ref^{}"
    local tag_ref="refs/tags/$ref"

    validate_release_ref "$ref" \
        || die "Cannot resolve an unsafe release tag to a commit: '$ref'"
    if [ -n "$requested_commit" ]; then
        validate_release_commit "$requested_commit" \
            || die "Invalid IPHONE_USE_RELEASE_COMMIT='$requested_commit' (expected exactly 40 hexadecimal characters)."
        requested_commit="$(printf '%s' "$requested_commit" | tr '[:upper:]' '[:lower:]')"
        if [ "${IPHONE_USE_INSTALLER_PINNED:-0}" = "1" ]; then
            printf '%s' "$requested_commit"
            return 0
        fi
    fi

    # Resolve the human-readable release tag once, then use the resulting commit
    # for every raw helper URL. The commits endpoint peels annotated tags for us.
    # If the unauthenticated API is rate-limited, a read-only ls-remote provides
    # the same resolution without cloning the repository.
    if response="$(curl -fsSL \
        -H 'Accept: application/vnd.github+json' \
        -H 'X-GitHub-Api-Version: 2022-11-28' \
        "https://api.github.com/repos/$REPO/commits/$ref" 2>/dev/null)"; then
        commit="$(printf '%s\n' "$response" \
            | sed -nE 's/^[[:space:]]*"sha":[[:space:]]*"([0-9A-Fa-f]{40})".*/\1/p' \
            | head -1)"
    fi

    if ! validate_release_commit "$commit" && command -v git >/dev/null 2>&1; then
        remote_refs="$(git ls-remote "https://github.com/$REPO.git" \
            "$tag_ref" "$peeled_ref" 2>/dev/null || true)"
        commit="$(printf '%s\n' "$remote_refs" \
            | awk -v wanted="$peeled_ref" '$2 == wanted { print $1; exit }')"
        if ! validate_release_commit "$commit"; then
            commit="$(printf '%s\n' "$remote_refs" \
                | awk -v wanted="$tag_ref" '$2 == wanted { print $1; exit }')"
        fi
    fi

    validate_release_commit "$commit" \
        || die "Could not resolve release $ref to an exact commit SHA; refusing mutable raw helper URLs."
    commit="$(printf '%s' "$commit" | tr '[:upper:]' '[:lower:]')"
    if [ -n "$requested_commit" ] && [ "$requested_commit" != "$commit" ]; then
        die "IPHONE_USE_RELEASE_COMMIT does not match release $ref (expected $commit, got $requested_commit)."
    fi
    printf '%s' "$commit"
}

ensure_release_ref() {
    if [ -z "$RELEASE_REF" ]; then
        RELEASE_REF="$(resolve_release_ref)"
    fi
}

ensure_release_commit() {
    ensure_release_ref
    if [ -z "$RELEASE_COMMIT" ]; then
        RELEASE_COMMIT="$(resolve_release_commit "$RELEASE_REF")"
    fi
}

extract_verified_release_app_archive() {
    local ref="$1"
    local zip="$2"
    local destination="$3"
    local archive_entries
    local archive_entry

    archive_entries="$(/usr/bin/unzip -Z1 "$zip")" \
        || die "Could not enumerate the release archive."
    [ -n "$archive_entries" ] \
        || die "Release $ref contains an empty app archive."
    while IFS= read -r archive_entry; do
        case "$archive_entry" in
            "$APP_NAME"|"$APP_NAME/"*) ;;
            *) die "Release $ref archive contains an unexpected top-level entry: $archive_entry" ;;
        esac
        case "$archive_entry" in
            *//*|*/./*|*/.|*/../*|*/..)
                die "Release $ref archive contains an unsafe path: $archive_entry"
                ;;
        esac
    done <<EOF
$archive_entries
EOF
    if /usr/bin/zipinfo -l "$zip" \
        | grep -Eq '^l[-rwxstST]{9}[[:space:]]'; then
        die "Release $ref archive contains a symlink; refusing unsafe extraction."
    fi

    /usr/bin/unzip -q "$zip" -d "$destination"
    [ -d "$destination/$APP_NAME" ] \
        && [ ! -L "$destination/$APP_NAME" ] \
        || die "Release $ref did not extract $APP_NAME."
    if find "$destination/$APP_NAME" -type l -print -quit | grep -q .; then
        die "Release $ref extracted an app bundle containing an unexpected symlink."
    fi
}

download_verified_release_app() {
    local ref="$1"
    local destination="$2"
    local asset_url="https://github.com/$REPO/releases/download/$ref/${APP_NAME}.zip"
    local zip="$destination/${APP_NAME}.zip"
    local checksum="$zip.sha256"
    local expected
    local actual

    mkdir -p "$destination"
    info "Downloading $asset_url ..."
    curl -fsSL "$asset_url" -o "$zip"
    curl -fsSL "${asset_url}.sha256" -o "$checksum" \
        || die "Release $ref is missing the required ${APP_NAME}.zip.sha256 file."
    expected="$(awk 'NR == 1 { print $1 }' "$checksum")"
    printf '%s' "$expected" | grep -Eq '^[0-9A-Fa-f]{64}$' \
        || die "Release $ref has an invalid SHA-256 checksum file."
    actual="$(shasum -a 256 "$zip" | awk '{print $1}')"
    [ "$actual" = "$expected" ] \
        || die "SHA-256 mismatch for release $ref (expected $expected, got $actual)."
    ok "SHA-256 verified for release $ref"

    extract_verified_release_app_archive "$ref" "$zip" "$destination"
}

# ── Device runner sources ─────────────────────────────────────────────────────
# setup-wda.sh builds the iphone-use device runner (an XCTest UI-test bundle)
# from sources at ~/.iphone-use/runner. They come from the same local checkout
# or exact release as this installer: a local `runner/` directory, or the
# release asset iphone-use-runner.tar.gz verified against its .sha256 file.
# The swap joins the install transaction: the previous tree is kept aside
# until the daemon commits, and put back on any failure.
RUNNER_ASSET_NAME="iphone-use-runner.tar.gz"
RUNNER_SRC_DST="$HOME/.iphone-use/runner"
RUNNER_SRC_STAGE=""
RUNNER_SRC_BACKUP=""
RUNNER_SRC_DL=""
RUNNER_SRC_HAD_EXISTING=0
RUNNER_SRC_REPLACED=0
RUNNER_SRC_COMMITTED=0

# A runner tree: IPhoneUseRunner/ with its project file, and no symlinks.
runner_tree_valid() {
    local dir="$1"
    [ -d "$dir" ] && [ ! -L "$dir" ] || return 1
    [ -d "$dir/IPhoneUseRunner" ] && [ ! -L "$dir/IPhoneUseRunner" ] || return 1
    [ -f "$dir/IPhoneUseRunner/IPhoneUseRunner.xcodeproj/project.pbxproj" ] || return 1
    ! find "$dir" -type l -print -quit 2>/dev/null | grep -q .
}

# Downloads and verifies the release's runner sources into $2/runner.
# Returns 1 (without dying) only when the release has no such asset, so a
# release that predates the device runner can still be installed.
download_verified_release_runner() {
    local ref="$1"
    local destination="$2"
    local asset_url="https://github.com/$REPO/releases/download/$ref/$RUNNER_ASSET_NAME"
    local tarball="$destination/$RUNNER_ASSET_NAME"
    local expected actual
    mkdir -p "$destination"
    info "Downloading $asset_url ..."
    curl -fsSL "$asset_url" -o "$tarball" 2>/dev/null || return 1
    curl -fsSL "${asset_url}.sha256" -o "$tarball.sha256" \
        || die "Release $ref is missing the required $RUNNER_ASSET_NAME.sha256 file."
    expected="$(awk 'NR == 1 { print $1 }' "$tarball.sha256")"
    printf '%s' "$expected" | grep -Eq '^[0-9A-Fa-f]{64}$' \
        || die "Release $ref has an invalid $RUNNER_ASSET_NAME checksum file."
    actual="$(shasum -a 256 "$tarball" | awk '{print $1}')"
    [ "$actual" = "$expected" ] \
        || die "SHA-256 mismatch for $RUNNER_ASSET_NAME in release $ref (expected $expected, got $actual)."
    extract_verified_runner_archive "$ref" "$tarball" "$destination"
    ok "SHA-256 verified for the release's device runner sources"
}

# Only a runner/ tree, no absolute or parent paths, no links of any kind.
extract_verified_runner_archive() {
    local ref="$1"
    local tarball="$2"
    local destination="$3"
    local entries entry
    entries="$(/usr/bin/tar -tzf "$tarball")" \
        || die "Could not read the $RUNNER_ASSET_NAME archive of release $ref."
    [ -n "$entries" ] || die "Release $ref contains an empty $RUNNER_ASSET_NAME."
    while IFS= read -r entry; do
        case "$entry" in
            runner|runner/|runner/*) ;;
            *) die "Release $ref runner archive contains an unexpected entry: $entry" ;;
        esac
        case "$entry" in
            */../*|*/..|../*|*//*|*/./*)
                die "Release $ref runner archive contains an unsafe path: $entry"
                ;;
        esac
    done <<EOF
$entries
EOF
    if /usr/bin/tar -tvzf "$tarball" | grep -Eq '^[lh]'; then
        die "Release $ref runner archive contains a link; refusing unsafe extraction."
    fi
    /usr/bin/tar -xzf "$tarball" -C "$destination" \
        || die "Could not extract the $RUNNER_ASSET_NAME archive of release $ref."
    runner_tree_valid "$destination/runner" \
        || die "Release $ref runner archive did not extract a valid runner/ tree."
}

# Stage a copy of $1 (a runner/ tree) and swap it in at RUNNER_SRC_DST.
install_runner_sources() {
    local source="$1"
    runner_tree_valid "$source" || die "Not a device runner source tree: $source"
    mkdir -p "$HOME/.iphone-use"
    [ ! -L "$RUNNER_SRC_DST" ] || die "Refusing a symlinked runner source path: $RUNNER_SRC_DST"
    RUNNER_SRC_STAGE="$(mktemp -d "$HOME/.iphone-use/.runner.new.XXXXXX")" \
        || die "Could not stage the device runner sources."
    /usr/bin/ditto "$source/IPhoneUseRunner" "$RUNNER_SRC_STAGE/IPhoneUseRunner" \
        || die "Could not copy the device runner sources."
    if [ -f "$source/README.md" ]; then
        cp "$source/README.md" "$RUNNER_SRC_STAGE/README.md" \
            || die "Could not copy the device runner README."
    fi
    # Xcode's per-user state never belongs in the shared copy.
    find "$RUNNER_SRC_STAGE" -name xcuserdata -type d -prune -exec rm -rf {} + 2>/dev/null || true
    chmod -R go-w "$RUNNER_SRC_STAGE" && chmod 700 "$RUNNER_SRC_STAGE" \
        || die "Could not secure the staged device runner sources."
    runner_tree_valid "$RUNNER_SRC_STAGE" \
        || die "The staged device runner sources are incomplete."
    if [ -e "$RUNNER_SRC_DST" ]; then
        [ -d "$RUNNER_SRC_DST" ] || die "$RUNNER_SRC_DST exists but is not a directory."
        RUNNER_SRC_BACKUP="$HOME/.iphone-use/.runner.backup.$$"
        rm -rf "$RUNNER_SRC_BACKUP"
        mv "$RUNNER_SRC_DST" "$RUNNER_SRC_BACKUP" \
            || die "Could not move the previous device runner sources aside."
        RUNNER_SRC_HAD_EXISTING=1
    fi
    mv "$RUNNER_SRC_STAGE" "$RUNNER_SRC_DST" \
        || die "Could not install the device runner sources at $RUNNER_SRC_DST."
    RUNNER_SRC_STAGE=""
    RUNNER_SRC_REPLACED=1
    ok "Device runner sources atomically updated: $RUNNER_SRC_DST"
}

# Where this install's runner sources come from: the local checkout, else the
# release asset. Sets RUNNER_SOURCE_DIR; returns 1 when neither has them (a
# release that predates the device runner). A bad checksum or archive dies.
RUNNER_SOURCE_DIR=""
resolve_runner_source() {
    RUNNER_SOURCE_DIR=""
    if [ "$SCRIPT_IS_LOCAL" = "1" ] && runner_tree_valid "$SCRIPT_DIR/runner"; then
        RUNNER_SOURCE_DIR="$SCRIPT_DIR/runner"
        return 0
    fi
    ensure_release_ref
    RUNNER_SRC_DL="$(mktemp -d)"
    download_verified_release_runner "$RELEASE_REF" "$RUNNER_SRC_DL" || return 1
    RUNNER_SOURCE_DIR="$RUNNER_SRC_DL/runner"
}

_restore_runner_sources() {
    [ "$RUNNER_SRC_REPLACED" = "1" ] || return 0
    [ "$RUNNER_SRC_COMMITTED" = "0" ] || return 0
    rm -rf "$RUNNER_SRC_DST" 2>/dev/null || true
    if [ "$RUNNER_SRC_HAD_EXISTING" = "1" ] && [ -d "$RUNNER_SRC_BACKUP" ]; then
        if mv "$RUNNER_SRC_BACKUP" "$RUNNER_SRC_DST" 2>/dev/null; then
            RUNNER_SRC_BACKUP=""
            warn "Restored the previous device runner sources after an incomplete install."
        else
            warn "Could not restore the previous device runner sources from $RUNNER_SRC_BACKUP"
        fi
    fi
    RUNNER_SRC_REPLACED=0
}

_finish_runner_sources() {
    [ -z "$RUNNER_SRC_BACKUP" ] || rm -rf "$RUNNER_SRC_BACKUP" 2>/dev/null || true
    RUNNER_SRC_BACKUP=""
}

_configure_skill_lock_paths() {
    local xdg_state="${XDG_STATE_HOME:-}"
    local xdg_root
    local lock_parent
    local lock_parent_real
    local lock_parent_owner
    SKILL_XDG_LOCK_PATH=""
    if [ -n "$xdg_state" ]; then
        case "$xdg_state" in
            /*) ;;
            *) die "XDG_STATE_HOME must be absolute when set (got '$xdg_state')." ;;
        esac
        xdg_root="${xdg_state%/}"
        case "$xdg_root" in
            "$HOME"|"$HOME"/*) ;;
            *) die "XDG_STATE_HOME must stay inside this user's HOME during skill synchronization: $xdg_state" ;;
        esac
        case "$xdg_root" in
            *//*|*/./*|*/.|*/../*|*/..)
                die "XDG_STATE_HOME must be lexically normalized: $xdg_state"
                ;;
        esac
        case "$xdg_root" in
            "$HOME/.agents/skills"|"$HOME/.agents/skills/"*|\
            "$HOME/.iphone-use"|"$HOME/.iphone-use/"*|\
            "$HOME/.claude/skills/iphone-use"|"$HOME/.claude/skills/iphone-use/"*)
                die "XDG_STATE_HOME overlaps an iphone-use transaction directory: $xdg_state"
                ;;
        esac
        SKILL_XDG_LOCK_PATH="$xdg_root/skills/.skill-lock.json"
        lock_parent="$(dirname "$SKILL_XDG_LOCK_PATH")"
        if [ -e "$lock_parent" ] || [ -L "$lock_parent" ]; then
            [ -d "$lock_parent" ] \
                || die "XDG skills-lock parent is not a directory: $lock_parent"
            lock_parent_real="$(cd -P "$lock_parent" 2>/dev/null && pwd -P)" \
                || die "Could not resolve XDG skills-lock parent: $lock_parent"
            case "$lock_parent_real" in
                "$HOME"|"$HOME"/*) ;;
                *) die "XDG skills-lock parent resolves outside HOME: $lock_parent_real" ;;
            esac
            case "$lock_parent_real" in
                "$HOME/.agents/skills"|"$HOME/.agents/skills/"*|\
                "$HOME/.iphone-use"|"$HOME/.iphone-use/"*|\
                "$HOME/.claude/skills/iphone-use"|"$HOME/.claude/skills/iphone-use/"*)
                    die "XDG skills-lock parent resolves into an iphone-use transaction directory: $lock_parent_real"
                    ;;
            esac
            lock_parent_owner="$(stat -f '%u' "$lock_parent_real" 2>/dev/null || true)"
            [ "$lock_parent_owner" = "$UID_NUM" ] \
                || die "XDG skills-lock parent is not owned by uid $UID_NUM: $lock_parent_real"
        fi
    fi
}

_ensure_skill_namespace() {
    local path
    local owner
    for path in "$HOME/.iphone-use" "$HOME/.agents" "$HOME/.agents/skills"; do
        [ ! -L "$path" ] || die "Refusing symlinked agent-skill namespace: $path"
        if [ -e "$path" ]; then
            [ -d "$path" ] || die "Agent-skill namespace is not a directory: $path"
            owner="$(stat -f '%u' "$path" 2>/dev/null || true)"
            [ "$owner" = "$UID_NUM" ] \
                || die "Agent-skill namespace is not owned by uid $UID_NUM: $path"
        else
            mkdir -m 700 "$path" \
                || die "Could not create the agent-skill namespace: $path"
        fi
    done
    chmod 700 "$HOME/.iphone-use" \
        || die "Could not secure the iphone-use rollback namespace."
}

_snapshot_skill_lock() {
    local key="$1"
    local path="$2"
    local owner
    [ -n "$path" ] || return 0
    if [ -L "$path" ]; then
        return 1
    fi
    if [ -e "$path" ]; then
        [ -f "$path" ] || return 1
        owner="$(stat -f '%u' "$path" 2>/dev/null || true)"
        [ "$owner" = "$UID_NUM" ] || return 1
        cp -p "$path" "$SKILL_BACKUP_DIR/${key}.original" || return 1
        : > "$SKILL_BACKUP_DIR/${key}.present" || return 1
    fi
}

_snapshot_skill_state() {
    local canonical_owner
    [ -z "$SKILL_BACKUP_DIR" ] \
        || die "Internal error: an agent-skill transaction is already active."
    _ensure_skill_namespace
    _configure_skill_lock_paths

    if [ -e "$SKILL_CANONICAL_DIR" ] && [ ! -d "$SKILL_CANONICAL_DIR" ]; then
        die "Existing canonical iphone-use skill is not a directory: $SKILL_CANONICAL_DIR"
    fi
    if [ -d "$SKILL_CANONICAL_DIR" ]; then
        canonical_owner="$(stat -f '%u' "$SKILL_CANONICAL_DIR" 2>/dev/null || true)"
        [ "$canonical_owner" = "$UID_NUM" ] \
            || die "Existing iphone-use skill is not owned by uid $UID_NUM: $SKILL_CANONICAL_DIR"
    fi
    SKILL_CANONICAL_BEFORE_STATE="$(_canonical_skill_state)"
    [ "$SKILL_CANONICAL_BEFORE_STATE" != "unsafe" ] \
        || die "Could not fingerprint the existing canonical iphone-use skill."
    SKILL_DEFAULT_LOCK_BEFORE_STATE="$(_skill_lock_state "$SKILL_DEFAULT_LOCK_PATH")"
    [ "$SKILL_DEFAULT_LOCK_BEFORE_STATE" != "unsafe" ] \
        || die "Could not fingerprint the global skills lock: $SKILL_DEFAULT_LOCK_PATH"
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ]; then
        SKILL_XDG_LOCK_BEFORE_STATE="$(_skill_lock_state "$SKILL_XDG_LOCK_PATH")"
        [ "$SKILL_XDG_LOCK_BEFORE_STATE" != "unsafe" ] \
            || die "Could not fingerprint the XDG skills lock: $SKILL_XDG_LOCK_PATH"
    fi

    SKILL_BACKUP_DIR="$(mktemp -d "$HOME/.iphone-use/skill-backup.XXXXXX")" \
        || die "Could not create the agent-skill rollback directory."
    chmod 700 "$SKILL_BACKUP_DIR" \
        || { rm -rf "$SKILL_BACKUP_DIR"; SKILL_BACKUP_DIR=""; die "Could not secure the agent-skill rollback directory."; }

    if [ -e "$SKILL_CANONICAL_DIR" ] || [ -L "$SKILL_CANONICAL_DIR" ]; then
        : > "$SKILL_BACKUP_DIR/canonical.present" \
            || { rm -rf "$SKILL_BACKUP_DIR"; SKILL_BACKUP_DIR=""; die "Could not record the existing agent skill."; }
    fi
    if ! _snapshot_skill_lock "lock-default" "$SKILL_DEFAULT_LOCK_PATH"; then
        rm -rf "$SKILL_BACKUP_DIR"
        SKILL_BACKUP_DIR=""
        die "Could not safely snapshot the global skills lock: $SKILL_DEFAULT_LOCK_PATH"
    fi
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ] \
        && ! _snapshot_skill_lock "lock-xdg" "$SKILL_XDG_LOCK_PATH"; then
        rm -rf "$SKILL_BACKUP_DIR"
        SKILL_BACKUP_DIR=""
        die "Could not safely snapshot the XDG skills lock: $SKILL_XDG_LOCK_PATH"
    fi
}

_remove_floating_skill_lock_entry() {
    local path="$1"
    local before_state="$2"
    local lock_kind="$3"
    local stage
    local live_state
    local stage_state
    local intended_after_state
    [ -n "$path" ] || return 0
    live_state="$(_skill_lock_state "$path")"
    [ "$live_state" = "$before_state" ] \
        || die "Skills lock changed after snapshot; refusing to overwrite concurrent data: $path"
    if [ ! -f "$path" ]; then
        case "$lock_kind" in
            default) SKILL_DEFAULT_LOCK_AFTER_STATE="$live_state" ;;
            xdg) SKILL_XDG_LOCK_AFTER_STATE="$live_state" ;;
            *) die "Internal error: unknown skills-lock transaction kind '$lock_kind'." ;;
        esac
        return 0
    fi

    /usr/bin/plutil -convert json -o - "$path" >/dev/null 2>&1 \
        || die "The global skills lock is not valid JSON/plist data: $path"
    if /usr/bin/plutil -extract 'skills.iphone-use' json -o - "$path" \
        >/dev/null 2>&1; then
        stage="$(mktemp "${path}.iphone-use.XXXXXX")" \
            || die "Could not stage the skills lock update: $path"
        if ! cp -p "$path" "$stage"; then
            rm -f "$stage" 2>/dev/null || true
            die "Could not copy the skills lock into transaction staging: $path"
        fi
        stage_state="$(_skill_lock_state "$stage")"
        if [ "$stage_state" != "$before_state" ]; then
            rm -f "$stage" 2>/dev/null || true
            die "Skills lock changed while staging; refusing to overwrite concurrent data: $path"
        fi
        if ! /usr/bin/plutil -remove 'skills.iphone-use' "$stage" \
            || ! /usr/bin/plutil -convert json -o - "$stage" \
                >/dev/null 2>&1 \
            || /usr/bin/plutil -extract 'skills.iphone-use' json -o - "$stage" \
                >/dev/null 2>&1; then
            rm -f "$stage" 2>/dev/null || true
            die "Could not prepare removal of the floating iphone-use entry from $path"
        fi
        intended_after_state="$(_skill_lock_state "$stage")"
        [ "$intended_after_state" != "unsafe" ] \
            || { rm -f "$stage" 2>/dev/null || true; die "Could not fingerprint the staged skills-lock update: $path"; }
        live_state="$(_skill_lock_state "$path")"
        if [ "$live_state" != "$before_state" ]; then
            rm -f "$stage" 2>/dev/null || true
            die "Skills lock changed before commit; refusing to overwrite concurrent data: $path"
        fi
        case "$lock_kind" in
            default) SKILL_DEFAULT_LOCK_AFTER_STATE="$intended_after_state" ;;
            xdg) SKILL_XDG_LOCK_AFTER_STATE="$intended_after_state" ;;
            *) rm -f "$stage" 2>/dev/null || true; die "Internal error: unknown skills-lock transaction kind '$lock_kind'." ;;
        esac
        mv -f "$stage" "$path" \
            || { rm -f "$stage" 2>/dev/null || true; die "Could not atomically update the skills lock: $path"; }
        if [ "$lock_kind" = "default" ] \
            && [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_AFTER_MOVE:-0}" = "1" ] \
            && [ "${IPHONE_USE_INTERNAL_TEST_SKILL_ONLY:-0}" = "1" ]; then
            /usr/bin/plutil -insert 'skills.concurrent-after-move' \
                -json '{"source":"concurrent/after-move"}' \
                "$path" \
                || die "Could not create the internal post-move lock fixture."
        fi
        live_state="$(_skill_lock_state "$path")"
        [ "$live_state" = "$intended_after_state" ] \
            || die "Skills lock changed immediately after commit; preserving concurrent data during rollback: $path"
        return 0
    fi
    if [ "$lock_kind" = "default" ] \
        && [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_DURING_NOOP:-0}" = "1" ] \
        && [ "${IPHONE_USE_INTERNAL_TEST_SKILL_ONLY:-0}" = "1" ]; then
        /usr/bin/plutil -insert 'skills.concurrent-during-noop' \
            -json '{"source":"concurrent/during-noop"}' \
            "$path" \
            || die "Could not create the internal no-op lock fixture."
    fi
    live_state="$(_skill_lock_state "$path")"
    [ "$live_state" = "$before_state" ] \
        || die "Skills lock changed during no-op synchronization; refusing to absorb concurrent data: $path"
    case "$lock_kind" in
        default) SKILL_DEFAULT_LOCK_AFTER_STATE="$before_state" ;;
        xdg) SKILL_XDG_LOCK_AFTER_STATE="$before_state" ;;
        *) die "Internal error: unknown skills-lock transaction kind '$lock_kind'." ;;
    esac
}

_install_claude_skill_discovery() {
    local path
    local owner
    local claude_skills_real
    local canonical_parent_real
    local link_real
    local moved_state

    for path in "$HOME/.claude" "$HOME/.claude/skills"; do
        if [ -L "$path" ]; then
            if [ "$path" = "$HOME/.claude/skills" ]; then
                claude_skills_real="$(cd -P "$path" 2>/dev/null && pwd -P || true)"
                canonical_parent_real="$(cd -P "$HOME/.agents/skills" 2>/dev/null && pwd -P || true)"
                if [ -n "$claude_skills_real" ] \
                    && [ "$claude_skills_real" = "$canonical_parent_real" ]; then
                    SKILL_CLAUDE_DISCOVERY=1
                    return 0
                fi
            fi
            die "Refusing symlinked Claude skill namespace: $path"
        fi
        if [ -e "$path" ]; then
            [ -d "$path" ] || die "Claude skill namespace is not a directory: $path"
            owner="$(stat -f '%u' "$path" 2>/dev/null || true)"
            [ "$owner" = "$UID_NUM" ] \
                || die "Claude skill namespace is not owned by uid $UID_NUM: $path"
        else
            mkdir -m 700 "$path" \
                || die "Could not create Claude skill namespace: $path"
            if [ "$path" = "$HOME/.claude" ]; then
                SKILL_CLAUDE_HOME_CREATED=1
            else
                SKILL_CLAUDE_SKILLS_CREATED=1
            fi
        fi
    done

    SKILL_CLAUDE_BEFORE_STATE="$(_claude_skill_link_state)"
    [ "$SKILL_CLAUDE_BEFORE_STATE" != "unsafe" ] \
        || die "Could not fingerprint the existing Claude skill target."
    if [ -L "$SKILL_CLAUDE_LINK" ] && [ -d "$SKILL_CLAUDE_LINK" ]; then
        link_real="$(cd -P "$SKILL_CLAUDE_LINK" 2>/dev/null && pwd -P || true)"
        if [ "$link_real" = "$SKILL_CANONICAL_DIR" ]; then
            SKILL_CLAUDE_DISCOVERY=1
            return 0
        fi
    fi

    if [ -e "$SKILL_CLAUDE_LINK" ] || [ -L "$SKILL_CLAUDE_LINK" ]; then
        : > "$SKILL_BACKUP_DIR/claude-link.present" \
            || die "Could not record the existing Claude skill target."
    fi
    SKILL_CLAUDE_LINK_TOUCHED=1
    if [ -f "$SKILL_BACKUP_DIR/claude-link.present" ]; then
        [ "$(_claude_skill_link_state)" = "$SKILL_CLAUDE_BEFORE_STATE" ] \
            || die "Claude skill target changed before replacement; refusing to overwrite it."
        mv "$SKILL_CLAUDE_LINK" "$SKILL_BACKUP_DIR/claude-link.original" \
            || die "Could not move the previous Claude skill target into rollback storage."
        moved_state="$(_transaction_node_state "$SKILL_BACKUP_DIR/claude-link.original")"
        if [ "$moved_state" != "$SKILL_CLAUDE_BEFORE_STATE" ]; then
            mv "$SKILL_BACKUP_DIR/claude-link.original" "$SKILL_CLAUDE_LINK" \
                || die "Claude target changed during replacement and could not be restored from rollback storage."
            die "Claude skill target changed during replacement; refusing to overwrite it."
        fi
    fi
    ln -s "../../.agents/skills/iphone-use" "$SKILL_CLAUDE_LINK" \
        || die "Could not link Claude Code to the release-matched skill."
    [ -L "$SKILL_CLAUDE_LINK" ] && [ -d "$SKILL_CLAUDE_LINK" ] \
        || die "Claude Code skill discovery link is not usable."
    link_real="$(cd -P "$SKILL_CLAUDE_LINK" 2>/dev/null && pwd -P || true)"
    [ "$link_real" = "$SKILL_CANONICAL_DIR" ] \
        || die "Claude Code skill discovery link resolves to the wrong target."
    /usr/bin/cmp -s \
        "$SKILL_CANONICAL_DIR/SKILL.md" \
        "$SKILL_CLAUDE_LINK/SKILL.md" \
        || die "Claude Code does not see the verified skill bytes."
    SKILL_CLAUDE_AFTER_STATE="$(_claude_skill_link_state)"
    [ "$SKILL_CLAUDE_AFTER_STATE" != "unsafe" ] \
        || die "Could not fingerprint the installed Claude skill target."
    SKILL_CLAUDE_DISCOVERY=1
}

_validate_expected_skill() {
    local expected="$1"
    local size
    [ -f "$expected" ] && [ ! -L "$expected" ] \
        || die "Expected agent skill is not a regular file: $expected"
    size="$(stat -f '%z' "$expected" 2>/dev/null || true)"
    printf '%s' "$size" | grep -Eq '^[0-9]+$' \
        || die "Could not read the expected agent-skill size."
    [ "$size" -ge 256 ] && [ "$size" -le 1048576 ] \
        || die "Expected agent skill has an implausible size: $size bytes"
    grep -Eq '^name:[[:space:]]*iphone-use[[:space:]]*$' "$expected" \
        || die "Expected agent skill has no iphone-use frontmatter name."
    grep -Eq '^description:[[:space:]]*[^[:space:]].*$' "$expected" \
        || die "Expected agent skill has no frontmatter description."
}

_validate_installed_skill() {
    local expected="$1"
    local ref="$2"
    local commit="$3"
    local sha256="$4"
    local entry_count
    local owner

    [ -d "$SKILL_CANONICAL_DIR" ] && [ ! -L "$SKILL_CANONICAL_DIR" ] \
        || die "Canonical agent skill is not a real directory: $SKILL_CANONICAL_DIR"
    owner="$(stat -f '%u' "$SKILL_CANONICAL_DIR" 2>/dev/null || true)"
    [ "$owner" = "$UID_NUM" ] \
        || die "Canonical agent skill is not owned by uid $UID_NUM."
    [ -f "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        && [ ! -L "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        || die "Installed agent skill is missing SKILL.md."
    /usr/bin/cmp -s "$expected" "$SKILL_CANONICAL_DIR/SKILL.md" \
        || die "Installed SKILL.md differs from the release-commit source."
    entry_count="$(find "$SKILL_CANONICAL_DIR" -mindepth 1 -maxdepth 1 -print \
        | wc -l | tr -d '[:space:]')"
    [ "$entry_count" = "2" ] \
        || die "Installed agent-skill directory contains unexpected entries."
    grep -Fx "release_ref=$ref" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null \
        || die "Installed agent-skill marker has the wrong release ref."
    grep -Fx "release_commit=$commit" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null \
        || die "Installed agent-skill marker has the wrong release commit."
    grep -Fx "skill_sha256=$sha256" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null \
        || die "Installed agent-skill marker has the wrong content hash."
    if [ "$SKILL_CLAUDE_DISCOVERY" != "1" ] \
        || [ ! -f "$SKILL_CLAUDE_LINK/SKILL.md" ] \
        || ! /usr/bin/cmp -s \
            "$SKILL_CANONICAL_DIR/SKILL.md" \
            "$SKILL_CLAUDE_LINK/SKILL.md"; then
        die "Claude Code discovery does not expose the verified skill bytes."
    fi
    if /usr/bin/plutil -extract 'skills.iphone-use' json -o - \
        "$SKILL_DEFAULT_LOCK_PATH" >/dev/null 2>&1; then
        die "The default skills lock still contains a floating iphone-use source."
    fi
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && /usr/bin/plutil -extract 'skills.iphone-use' json -o - \
            "$SKILL_XDG_LOCK_PATH" >/dev/null 2>&1; then
        die "The XDG skills lock still contains a floating iphone-use source."
    fi
}

_install_verified_skill() {
    local expected="$1"
    local ref="$2"
    local commit="$3"
    local kind="$4"
    local sha256

    _validate_expected_skill "$expected"
    sha256="$(/usr/bin/shasum -a 256 "$expected" | awk '{print $1}')"
    printf '%s' "$sha256" | grep -Eq '^[0-9a-f]{64}$' \
        || die "Could not calculate the expected agent-skill SHA-256."
    _snapshot_skill_state
    SKILL_SYNC_TOUCHED=1
    SKILL_INSTALL_SIGNAL_PENDING=0
    trap 'SKILL_INSTALL_SIGNAL_PENDING=1' HUP INT TERM

    SKILL_STAGE_DIR="$(mktemp -d "$HOME/.agents/skills/.iphone-use.new.XXXXXX")" \
        || die "Could not stage the agent skill."
    chmod 700 "$SKILL_STAGE_DIR" \
        || die "Could not secure the staged agent skill."
    /usr/bin/install -m 600 "$expected" "$SKILL_STAGE_DIR/SKILL.md" \
        || die "Could not stage the release-matched SKILL.md."
    {
        printf 'format=1\n'
        printf 'release_ref=%s\n' "$ref"
        printf 'release_commit=%s\n' "$commit"
        printf 'skill_sha256=%s\n' "$sha256"
    } > "$SKILL_STAGE_DIR/.iphone-use-release" \
        || die "Could not stage the agent-skill release marker."
    chmod 600 "$SKILL_STAGE_DIR/.iphone-use-release" \
        || die "Could not secure the agent-skill release marker."

    [ "$(_canonical_skill_state)" = "$SKILL_CANONICAL_BEFORE_STATE" ] \
        || die "Canonical iphone-use skill changed before replacement; refusing to overwrite it."
    if [ -f "$SKILL_BACKUP_DIR/canonical.present" ]; then
        mv "$SKILL_CANONICAL_DIR" "$SKILL_BACKUP_DIR/canonical.original" \
            || die "Could not move the previous agent skill into rollback storage."
        if [ "$(_canonical_skill_state "$SKILL_BACKUP_DIR/canonical.original")" \
            != "$SKILL_CANONICAL_BEFORE_STATE" ]; then
            mv "$SKILL_BACKUP_DIR/canonical.original" "$SKILL_CANONICAL_DIR" \
                || die "Canonical skill changed during replacement and could not be restored from rollback storage."
            die "Canonical iphone-use skill changed during replacement; refusing to overwrite it."
        fi
    fi
    SKILL_CANONICAL_AFTER_STATE="$(_canonical_skill_state)"
    [ "$SKILL_CANONICAL_AFTER_STATE" = "absent" ] \
        || die "Canonical skill target was not empty before atomic replacement."
    /bin/mkdir -m 700 "$SKILL_CANONICAL_DIR" \
        || die "Could not claim the canonical skill path without overwriting concurrent data."
    SKILL_CANONICAL_AFTER_STATE="$(_canonical_skill_state)"
    /bin/mv -n "$SKILL_STAGE_DIR/SKILL.md" "$SKILL_CANONICAL_DIR/SKILL.md" \
        || die "Could not place the release-matched SKILL.md."
    [ ! -e "$SKILL_STAGE_DIR/SKILL.md" ] \
        || die "Concurrent data blocked the release-matched SKILL.md."
    SKILL_CANONICAL_AFTER_STATE="$(_canonical_skill_state)"
    /bin/mv -n \
        "$SKILL_STAGE_DIR/.iphone-use-release" \
        "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        || die "Could not place the agent-skill release marker."
    [ ! -e "$SKILL_STAGE_DIR/.iphone-use-release" ] \
        || die "Concurrent data blocked the agent-skill release marker."
    SKILL_CANONICAL_AFTER_STATE="$(_canonical_skill_state)"
    rmdir "$SKILL_STAGE_DIR" \
        || die "Staged agent-skill directory was not empty after installation."
    SKILL_STAGE_DIR=""
    SKILL_CANONICAL_AFTER_STATE="$(_canonical_skill_state)"
    [ "$SKILL_CANONICAL_AFTER_STATE" != "unsafe" ] \
        || die "Could not fingerprint the installed canonical iphone-use skill."

    _install_claude_skill_discovery

    if [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_BEFORE_REMOVE:-0}" = "1" ] \
        && [ "${IPHONE_USE_INTERNAL_TEST_SKILL_ONLY:-0}" = "1" ]; then
        /usr/bin/plutil -insert 'skills.concurrent-before-remove' \
            -json '{"source":"concurrent/before-remove"}' \
            "$SKILL_DEFAULT_LOCK_PATH" \
            || die "Could not create the internal pre-removal lock fixture."
    fi
    _remove_floating_skill_lock_entry \
        "$SKILL_DEFAULT_LOCK_PATH" \
        "$SKILL_DEFAULT_LOCK_BEFORE_STATE" \
        "default"
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ]; then
        _remove_floating_skill_lock_entry \
            "$SKILL_XDG_LOCK_PATH" \
            "$SKILL_XDG_LOCK_BEFORE_STATE" \
            "xdg"
    fi
    _validate_installed_skill "$expected" "$ref" "$commit" "$sha256"

    SKILL_SYNCED=1
    SKILL_SYNC_KIND="$kind"
    SKILL_SYNC_REF="$ref"
    SKILL_SYNC_COMMIT="$commit"
    SKILL_SYNC_SHA256="$sha256"
    trap 'exit 130' HUP INT TERM
    if [ "$SKILL_INSTALL_SIGNAL_PENDING" = "1" ]; then
        exit 130
    fi
    ok "Agent skill content verified: $SKILL_CANONICAL_DIR ($sha256)"
}

_verify_current_skill_transaction() {
    local actual_sha
    local entry_count
    local owner
    local link_real

    [ "$SKILL_SYNCED" = "1" ] || return 1
    printf '%s' "$SKILL_SYNC_SHA256" | grep -Eq '^[0-9a-f]{64}$' || return 1
    [ -d "$SKILL_CANONICAL_DIR" ] && [ ! -L "$SKILL_CANONICAL_DIR" ] \
        || return 1
    owner="$(stat -f '%u' "$SKILL_CANONICAL_DIR" 2>/dev/null || true)"
    [ "$owner" = "$UID_NUM" ] || return 1
    [ -f "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        && [ ! -L "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        && [ -f "$SKILL_CANONICAL_DIR/.iphone-use-release" ] \
        && [ ! -L "$SKILL_CANONICAL_DIR/.iphone-use-release" ] \
        || return 1
    actual_sha="$(/usr/bin/shasum -a 256 "$SKILL_CANONICAL_DIR/SKILL.md" \
        | awk '{print $1}')" || return 1
    [ "$actual_sha" = "$SKILL_SYNC_SHA256" ] || return 1
    grep -Fx "release_ref=$SKILL_SYNC_REF" \
        "$SKILL_CANONICAL_DIR/.iphone-use-release" >/dev/null || return 1
    grep -Fx "release_commit=$SKILL_SYNC_COMMIT" \
        "$SKILL_CANONICAL_DIR/.iphone-use-release" >/dev/null || return 1
    grep -Fx "skill_sha256=$SKILL_SYNC_SHA256" \
        "$SKILL_CANONICAL_DIR/.iphone-use-release" >/dev/null || return 1
    entry_count="$(find "$SKILL_CANONICAL_DIR" -mindepth 1 -maxdepth 1 -print \
        | wc -l | tr -d '[:space:]')"
    [ "$entry_count" = "2" ] || return 1
    [ "$(_canonical_skill_state)" = "$SKILL_CANONICAL_AFTER_STATE" ] \
        || return 1

    [ -f "$SKILL_CLAUDE_LINK/SKILL.md" ] \
        && /usr/bin/cmp -s \
            "$SKILL_CANONICAL_DIR/SKILL.md" \
            "$SKILL_CLAUDE_LINK/SKILL.md" \
        || return 1
    link_real="$(cd -P "$SKILL_CLAUDE_LINK" 2>/dev/null && pwd -P || true)"
    [ "$link_real" = "$SKILL_CANONICAL_DIR" ] || return 1
    if [ "$SKILL_CLAUDE_LINK_TOUCHED" = "1" ]; then
        [ "$(_claude_skill_link_state)" = "$SKILL_CLAUDE_AFTER_STATE" ] \
            || return 1
    fi

    _verify_skill_lock_without_floating_entry "$SKILL_DEFAULT_LOCK_PATH" \
        || return 1
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ]; then
        _verify_skill_lock_without_floating_entry "$SKILL_XDG_LOCK_PATH" \
            || return 1
    fi
    return 0
}

_mark_skill_transaction_committed() {
    [ "$SKILL_SYNC_TOUCHED" = "1" ] || return 0
    SKILL_SYNC_COMMITTED=1
    if [ -n "$SKILL_BACKUP_DIR" ]; then
        rm -rf "$SKILL_BACKUP_DIR" 2>/dev/null \
            || warn "Could not remove committed agent-skill rollback data: $SKILL_BACKUP_DIR"
        SKILL_BACKUP_DIR=""
    fi
}

_commit_skill_transaction() {
    [ "$SKILL_SYNC_TOUCHED" = "1" ] || return 0
    _verify_current_skill_transaction \
        || die "Agent skill changed before transaction commit; refusing a false synchronization claim."
    _mark_skill_transaction_committed
}

install_pinned_skill() {
    local ref="$1"
    local commit="$2"
    local raw_url
    validate_release_ref "$ref" \
        || die "Cannot install an agent skill for unsafe release ref '$ref'."
    validate_release_commit "$commit" \
        || die "Cannot install an agent skill without an exact release commit."
    raw_url="https://raw.githubusercontent.com/$REPO/$commit/skills/iphone-use/SKILL.md"
    _ensure_skill_namespace
    SKILL_EXPECTED_DL="$(mktemp "$HOME/.iphone-use/skill-source.XXXXXX")" \
        || die "Could not stage the release-matched agent skill."
    info "Fetching the agent skill from release $ref commit $commit ..."
    curl -fsSL "$raw_url" -o "$SKILL_EXPECTED_DL" \
        || die "Could not fetch the agent skill from release $ref commit $commit."
    _install_verified_skill "$SKILL_EXPECTED_DL" "$ref" "$commit" "release"
    rm -f "$SKILL_EXPECTED_DL"
    SKILL_EXPECTED_DL=""
}

install_local_skill() {
    local source="$1"
    local expected="$source/skills/iphone-use/SKILL.md"
    info "Installing the iphone-use agent skill from this local working tree ..."
    _install_verified_skill "$expected" "local" "working-tree" "local"
}

_verify_bootstrap_skill() {
    local ref="${IPHONE_USE_RELEASE_REF:-}"
    local commit="${IPHONE_USE_RELEASE_COMMIT:-}"
    local expected_sha="${IPHONE_USE_SKILL_SHA256:-}"
    local actual_sha
    local owner
    local entry_count
    local link_real

    [ "${IPHONE_USE_INSTALLER_PINNED:-0}" = "1" ] \
        && [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || return 1
    validate_release_ref "$ref" && validate_release_commit "$commit" \
        || return 1
    printf '%s' "$expected_sha" | grep -Eq '^[0-9a-f]{64}$' \
        || return 1
    [ -d "$SKILL_CANONICAL_DIR" ] && [ ! -L "$SKILL_CANONICAL_DIR" ] \
        || return 1
    owner="$(stat -f '%u' "$SKILL_CANONICAL_DIR" 2>/dev/null || true)"
    [ "$owner" = "$UID_NUM" ] || return 1
    [ -f "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        && [ ! -L "$SKILL_CANONICAL_DIR/SKILL.md" ] \
        && [ -f "$SKILL_CANONICAL_DIR/.iphone-use-release" ] \
        && [ ! -L "$SKILL_CANONICAL_DIR/.iphone-use-release" ] \
        || return 1
    actual_sha="$(/usr/bin/shasum -a 256 "$SKILL_CANONICAL_DIR/SKILL.md" \
        | awk '{print $1}')" || return 1
    [ "$actual_sha" = "$expected_sha" ] || return 1
    grep -Fx "release_ref=$ref" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null || return 1
    grep -Fx "release_commit=$commit" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null || return 1
    grep -Fx "skill_sha256=$expected_sha" "$SKILL_CANONICAL_DIR/.iphone-use-release" \
        >/dev/null || return 1
    entry_count="$(find "$SKILL_CANONICAL_DIR" -mindepth 1 -maxdepth 1 -print \
        | wc -l | tr -d '[:space:]')"
    [ "$entry_count" = "2" ] || return 1
    [ -f "$SKILL_CLAUDE_LINK/SKILL.md" ] \
        && /usr/bin/cmp -s \
            "$SKILL_CANONICAL_DIR/SKILL.md" \
            "$SKILL_CLAUDE_LINK/SKILL.md" \
        || return 1
    link_real="$(cd -P "$SKILL_CLAUDE_LINK" 2>/dev/null && pwd -P || true)"
    [ "$link_real" = "$SKILL_CANONICAL_DIR" ] || return 1

    _configure_skill_lock_paths
    _verify_skill_lock_without_floating_entry "$SKILL_DEFAULT_LOCK_PATH" \
        || return 1
    if [ -n "$SKILL_XDG_LOCK_PATH" ] \
        && [ "$SKILL_XDG_LOCK_PATH" != "$SKILL_DEFAULT_LOCK_PATH" ]; then
        _verify_skill_lock_without_floating_entry "$SKILL_XDG_LOCK_PATH" \
            || return 1
    fi
    return 0
}

_verify_skill_before_daemon_commit() {
    if [ "$SKILL_SYNC_KIND" = "bootstrap" ]; then
        _verify_bootstrap_skill \
            || die "Pinned bootstrap skill changed before daemon commit; the previous daemon state was restored."
    elif [ "$SKILL_SYNC_TOUCHED" = "1" ]; then
        _verify_current_skill_transaction \
            || die "Agent skill changed before daemon commit; the previous transaction state was restored."
    fi
}

_commit_daemon_transaction() {
    [ "$INSTALL_COMMIT_SIGNAL_PENDING" = "0" ] || return 130

    # No fallible external operation may be added between this check and the
    # committed marker. Signals are deferred into the pending flag throughout
    # this assignment-only boundary, so an interrupt either precedes every
    # commit flag or follows a fully committed daemon transaction.
    APP_COMMITTED=1
    PLIST_COMMITTED=1
    SETUP_WDA_COMMITTED=1
    UNINSTALL_COMMITTED=1
    RUNNER_SRC_COMMITTED=1
    WDA_TRANSITION_COMMITTED=1
    DAEMON_RUNTIME_COMMITTED=1
    OLD_PLIST_COMMITTED=1
    INSTALL_DAEMON_COMMITTED=1
    _mark_skill_transaction_committed
    return 0
}

prepare_companion_skill() {
    if [ "${IPHONE_USE_SKIP_SKILL:-0}" = "1" ]; then
        if [ "${IPHONE_USE_SKILL_VERIFIED_BY_BOOTSTRAP:-0}" = "1" ]; then
            _verify_bootstrap_skill \
                || die "Pinned bootstrap skill verification failed; refusing to install the daemon."
            SKILL_SYNCED=1
            SKILL_SYNC_KIND="bootstrap"
            SKILL_SYNC_REF="$IPHONE_USE_RELEASE_REF"
            SKILL_SYNC_COMMIT="$IPHONE_USE_RELEASE_COMMIT"
            SKILL_SYNC_SHA256="$IPHONE_USE_SKILL_SHA256"
            info "Agent skill content was already verified by the pinned bootstrap."
        else
            warn "Agent skill synchronization explicitly disabled (IPHONE_USE_SKIP_SKILL=1)."
            warn "The daemon may not match an existing skill; no compatibility claim will be made."
        fi
        return 0
    fi

    if [ -n "${1:-}" ] \
        && [ "$SCRIPT_IS_LOCAL" = "1" ] \
        && [ -f "$SCRIPT_DIR/skills/iphone-use/SKILL.md" ]; then
        install_local_skill "$SCRIPT_DIR"
    else
        ensure_release_commit
        install_pinned_skill "$RELEASE_REF" "$RELEASE_COMMIT"
    fi
}

bootstrap_pinned_installer() {
    local ref
    local commit
    local raw_base
    local helper_dir
    local asset_dir
    local inner_status

    [ "$#" -eq 0 ] \
        || die "A local app path is accepted only when install.sh itself is run from a local file."
    ref="$(resolve_release_ref)"
    commit="$(resolve_release_commit "$ref")"
    raw_base="https://raw.githubusercontent.com/$REPO/$commit"
    BOOTSTRAP_TMP="$(mktemp -d)"
    helper_dir="$BOOTSTRAP_TMP/helpers"
    asset_dir="$BOOTSTRAP_TMP/asset"
    mkdir -m 700 "$helper_dir" "$asset_dir"
    mkdir -m 700 "$helper_dir/scripts"

    info "Pinning release $ref raw helpers to commit $commit ..."
    curl -fsSL "$raw_base/install.sh" -o "$helper_dir/install.sh"
    curl -fsSL "$raw_base/uninstall.sh" -o "$helper_dir/uninstall.sh"
    curl -fsSL "$raw_base/scripts/sign.sh" -o "$helper_dir/scripts/sign.sh"
    curl -fsSL "$raw_base/scripts/setup-wda.sh" -o "$helper_dir/scripts/setup-wda.sh"
    [ -s "$helper_dir/install.sh" ] \
        && [ -s "$helper_dir/uninstall.sh" ] \
        && [ -s "$helper_dir/scripts/sign.sh" ] \
        && [ -s "$helper_dir/scripts/setup-wda.sh" ] \
        || die "Release $ref is incomplete; refusing a mixed-version install."
    if ! /bin/bash -n "$helper_dir/install.sh" \
        || ! /bin/bash -n "$helper_dir/uninstall.sh" \
        || ! /bin/bash -n "$helper_dir/scripts/sign.sh" \
        || ! /bin/bash -n "$helper_dir/scripts/setup-wda.sh"; then
        die "Release $ref contains a shell script that failed syntax validation."
    fi
    download_verified_release_app "$ref" "$asset_dir"
    # A release whose setup-wda.sh builds the device runner ships its sources
    # as a verified asset; the inner installer then finds them next to itself.
    if grep -q 'IPhoneUseRunner' "$helper_dir/scripts/setup-wda.sh"; then
        download_verified_release_runner "$ref" "$helper_dir" \
            || die "Release $ref is missing $RUNNER_ASSET_NAME; refusing a mixed-version install."
    fi
    ok "Raw helper scripts pinned to exact commit $commit"

    if [ "${IPHONE_USE_SKIP_SKILL:-0}" = "1" ]; then
        warn "Agent skill synchronization explicitly disabled (IPHONE_USE_SKIP_SKILL=1)."
        warn "The daemon may not match an existing skill; no compatibility claim will be made."
    else
        install_pinned_skill "$ref" "$commit"
    fi

    # Defer outer-shell signals until the inner installer has either rolled
    # itself back or returned success and the matching skill is committed.
    # This closes the success-boundary window where a signal could otherwise
    # restore the old skill after the new daemon had already committed.
    BOOTSTRAP_SIGNAL_PENDING=0
    trap 'BOOTSTRAP_SIGNAL_PENDING=1' HUP INT TERM
    if IPHONE_USE_INSTALLER_PINNED=1 \
        IPHONE_USE_RELEASE_REF="$ref" \
        IPHONE_USE_RELEASE_COMMIT="$commit" \
        IPHONE_USE_SKIP_SKILL=1 \
        IPHONE_USE_SKILL_VERIFIED_BY_BOOTSTRAP="$SKILL_SYNCED" \
        IPHONE_USE_SKILL_SHA256="$SKILL_SYNC_SHA256" \
        /bin/bash "$helper_dir/install.sh" "$asset_dir/$APP_NAME" \
        ${INSTANCE_FORWARD_ARGS[@]+"${INSTANCE_FORWARD_ARGS[@]}"}; then
        inner_status=0
    else
        inner_status=$?
    fi
    if [ "$inner_status" -eq 0 ]; then
        _commit_skill_transaction
        if [ "$SKILL_SYNCED" = "1" ]; then
            ok "Daemon install and release-matched agent skill committed together: $ref ($commit)"
            info "Verified skill SHA-256: $SKILL_SYNC_SHA256"
        else
            warn "Daemon install committed with agent-skill synchronization explicitly disabled."
        fi
    fi
    trap 'exit 130' HUP INT TERM
    if [ "$BOOTSTRAP_SIGNAL_PENDING" = "1" ]; then
        exit 130
    fi
    [ "$inner_status" -eq 0 ] || exit "$inner_status"
    exit 0
}

echo ""
printf '%b=== iphone-use — install.sh ===%b\n' "$BOLD" "$RESET"
echo ""

# ── Guard: must be a GUI session ─────────────────────────────────────────────
UID_NUM="$(id -u)"
if [ "$UID_NUM" = "0" ]; then
    die "Do not run as root.  Run as the logged-in desktop user."
fi

# All destructive replacements are rooted below HOME. Require the caller's
# spelling to already be its physical absolute path, reject broad roots, verify
# ownership, and refuse namespace-parent symlinks before curl bootstrap or any
# mutation. This keeps fixed rm/mv targets inside one real user home.
case "$HOME" in
    /*) ;;
    *) die "HOME must be an absolute path (got '$HOME')." ;;
esac
HOME_CANONICAL="$(cd -P "$HOME" 2>/dev/null && pwd -P)" \
    || die "HOME does not exist or cannot be resolved: $HOME"
[ "$HOME_CANONICAL" = "$HOME" ] \
    || die "HOME must already be canonical (got '$HOME', resolves to '$HOME_CANONICAL')."
case "$HOME_CANONICAL" in
    /|/Users|/System/Volumes/Data/Users)
        die "Refusing unsafe broad HOME path: $HOME_CANONICAL"
        ;;
esac
HOME_OWNER_UID="$(stat -f '%u' "$HOME_CANONICAL" 2>/dev/null || true)"
[ "$HOME_OWNER_UID" = "$UID_NUM" ] \
    || die "HOME is not owned by uid $UID_NUM: $HOME_CANONICAL"
for NAMESPACE_PARENT in \
    "$HOME/Applications" \
    "$HOME/Library" \
    "$HOME/Library/LaunchAgents" \
    "$HOME/Library/Logs" \
    "$HOME/.iphone-use"
do
    [ ! -L "$NAMESPACE_PARENT" ] \
        || die "Refusing symlinked product namespace parent: $NAMESPACE_PARENT"
done

if ! launchctl print "gui/$UID_NUM" >/dev/null 2>&1; then
    warn "Could not enumerate gui/$UID_NUM session."
    warn "Make sure you are logged in to a desktop (Aqua) session."
    warn "Running over SSH is fine ONLY if the desktop user is also logged in."
fi

# `curl | sh` runs only this small bootstrap. It resolves one release tag to an
# exact commit for every raw executable helper, downloads the app from that
# release tag, verifies the app asset's published SHA-256, then invokes the
# downloaded installer as a real local file. It never inspects sibling files in
# the caller's current working directory.
INSTANCE_FORWARD_ARGS=()
if [ "$INSTANCE_NAME" != default ]; then
    INSTANCE_FORWARD_ARGS=(--instance "$INSTANCE_NAME")
    [ -z "$INSTANCE_UDID" ] || INSTANCE_FORWARD_ARGS+=(--udid "$INSTANCE_UDID")
    [ -z "$INSTANCE_PORT" ] || INSTANCE_FORWARD_ARGS+=(--port "$INSTANCE_PORT")
fi
if [ "$SCRIPT_IS_LOCAL" = "0" ]; then
    [ "${IPHONE_USE_INSTALLER_PINNED:-0}" != "1" ] \
        || die "Pinned inner installer must be executed from the downloaded local file."
    bootstrap_pinned_installer "$@"
fi

# ── Named instance (#67) ──────────────────────────────────────────────────────
# A second phone gets a second daemon. Everything below writes only that
# instance's plist, its private app copy and its state directory; the default
# instance's app, plists and runtime are read (signing policy) but never
# changed. Paths, labels and ports come from setup-wda.sh `instance-context`,
# the one shell implementation of the derivation.
named_plist_env() {
    [ -f "$1" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$2" "$1" 2>/dev/null || printf ''
}

install_named_instance() {
    local app_src="${1:-$INSTALL_DIR/$APP_NAME}"
    local setup_src="$SCRIPT_DIR/scripts/setup-wda.sh"
    local uninstall_src="$SCRIPT_DIR/uninstall.sh"
    local default_wda_plist="$WDA_PLIST_DST"
    local ctx state_dir label plist wda_label daemon_port wda_port mjpeg_port udid
    local runtime app_dst app_stage app_backup="" plist_stage plist_backup=""
    local binary token key value probe code i was_loaded=0
    local -a env_keys env_values

    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "Named instances install from a local or release-pinned installer."
    [ -f "$setup_src" ] && [ -f "$uninstall_src" ] \
        || die "setup-wda.sh/uninstall.sh are missing next to this installer."
    [ "$#" -le 1 ] || die "Only one app path may be given."

    # Resolve and check the instance before touching anything: an invalid
    # name, a port another instance owns, or a phone another daemon drives
    # all fail here.
    if ! ctx="$(env PHONE_REMOTE_INSTANCE="$INSTANCE_NAME" \
        ${INSTANCE_UDID:+WDA_UDID="$INSTANCE_UDID"} \
        ${INSTANCE_PORT:+PHONE_REMOTE_PORT="$INSTANCE_PORT"} \
        /bin/bash "$setup_src" instance-context 2>&1)"; then
        die "Instance $INSTANCE_NAME cannot be installed: $(printf '%s' "$ctx" | tail -1)"
    fi
    ctx_get() { printf '%s\n' "$ctx" | sed -n "s/^$1=//p" | head -1; }
    state_dir="$(ctx_get state_dir)"
    label="$(ctx_get daemon_label)"
    wda_label="$(ctx_get wda_label)"
    plist="$(ctx_get daemon_plist)"
    daemon_port="$(ctx_get daemon_port)"
    wda_port="$(ctx_get wda_port)"
    mjpeg_port="$(ctx_get mjpeg_port)"
    udid="$(ctx_get udid)"
    [ "$(ctx_get name)" = "$INSTANCE_NAME" ] && [ -n "$state_dir" ] && [ -n "$label" ] \
        && [ "$label" != "$PLIST_LABEL" ] \
        || die "setup-wda.sh resolved an unexpected instance context."
    [ -n "$udid" ] \
        || die "A new instance needs its iPhone: --udid <UDID> (see: xcrun devicectl list devices)."
    printf '%s' "$udid" | LC_ALL=C grep -Eq '^[0-9A-Fa-f-]+$' \
        || die "--udid must be hex digits and dashes."
    for value in "$daemon_port" "$wda_port" "$mjpeg_port"; do
        printf '%s' "$value" | grep -Eq '^[0-9]{1,5}$' && [ "$value" -ge 1 ] && [ "$value" -le 65535 ] \
            || die "Instance ports must be TCP ports (got '$value')."
    done
    [ "$daemon_port" != "$wda_port" ] && [ "$daemon_port" != "$mjpeg_port" ] \
        && [ "$wda_port" != "$mjpeg_port" ] \
        || die "Daemon, WDA and video ports must differ."

    runtime="$state_dir/runtime"
    app_dst="$runtime/$APP_NAME"
    binary="$app_dst/$BINARY_INSIDE_APP"
    if [ -f "$plist" ]; then
        [ ! -L "$plist" ] || die "Refusing symlinked plist: $plist"
        [ "$(/usr/libexec/PlistBuddy -c 'Print :Label' "$plist" 2>/dev/null)" = "$label" ] \
            && [ "$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$plist" 2>/dev/null)" = "$binary" ] \
            || die "$plist exists but is not this installer's instance $INSTANCE_NAME; refusing to overwrite it."
    fi

    # The app: a verified copy, private to the instance, so the default
    # instance's upgrades never swap the binary under this daemon.
    [ -d "$app_src" ] && [ ! -L "$app_src" ] || die "App not found: $app_src (install the default instance first, or pass an app path)."
    [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app_src/Contents/Info.plist" 2>/dev/null)" = "$BUNDLE_ID" ] \
        || die "$app_src is not $BUNDLE_ID."
    codesign --verify "$app_src" 2>/dev/null || die "$app_src fails signature verification."
    probe="$(env PHONE_REMOTE_INSTANCE="$INSTANCE_NAME" "$app_src/$BINARY_INSIDE_APP" instance-context 2>/dev/null || true)"
    printf '%s' "$probe" | grep -q "\"name\": *\"$INSTANCE_NAME\"" \
        || die "$app_src predates named instances (no instance-context); install a newer release."

    # Settings: explicit environment > this instance's plist > the default
    # instance's WDA supervisor, which holds the signing policy that works on
    # this Mac. The three ASC keys travel as a set.
    env_keys=(); env_values=()
    add_env() { [ -z "$2" ] || { env_keys+=("$1"); env_values+=("$2"); }; }
    pick() {
        local v
        v="$(printenv "$1" 2>/dev/null || true)"
        [ -n "$v" ] || v="$(named_plist_env "$plist" "$1")"
        [ -n "$v" ] || [ "${2:-}" != inherit ] || v="$(named_plist_env "$default_wda_plist" "$1")"
        printf '%s' "$v"
    }
    token="$(pick PHONE_REMOTE_AGENT_TOKEN)"
    [ -n "$token" ] || token="$(openssl rand -hex 32)" || die "Could not generate an agent token."
    add_env PHONE_REMOTE_INSTANCE "$INSTANCE_NAME"
    # The daemon keeps its pid record and session secret under $TMPDIR; daemons
    # up to v0.10.12 used one directory per user, so a second daemon saw the
    # first one's pid and refused to start. A private TMPDIR keeps them apart
    # on any release.
    add_env TMPDIR "$state_dir/tmp"
    add_env PHONE_REMOTE_HOST "$(pick PHONE_REMOTE_HOST)"
    [ -n "$(pick PHONE_REMOTE_HOST)" ] || add_env PHONE_REMOTE_HOST 127.0.0.1
    add_env PHONE_REMOTE_PORT "$daemon_port"
    add_env PHONE_REMOTE_AGENT_TOKEN "$token"
    add_env PHONE_REMOTE_BACKEND direct
    add_env PHONE_REMOTE_UDID "$udid"
    add_env PHONE_REMOTE_WDA_MANAGED true
    add_env PHONE_REMOTE_WDA_URL "http://127.0.0.1:$wda_port"
    add_env PHONE_REMOTE_WDA_MJPEG_URL "http://127.0.0.1:$mjpeg_port"
    add_env WDA_PORT "$wda_port"
    add_env MJPEG_PORT "$mjpeg_port"
    add_env RUST_LOG "$(pick RUST_LOG)"
    [ -n "$(pick RUST_LOG)" ] || add_env RUST_LOG warn
    for key in PHONE_REMOTE_IDLE_RELEASE_SECS PHONE_REMOTE_PASSWORD PHONE_REMOTE_NO_UPDATE_CHECK; do
        add_env "$key" "$(pick "$key")"
    done
    for key in WDA_TEAM_ID WDA_BUNDLE_ID WDA_ALLOW_LAN IPU_RUNNER_SRC; do
        add_env "$key" "$(pick "$key" inherit)"
    done
    if [ -n "${WDA_ASC_KEY_PATH:-}${WDA_ASC_KEY_ID:-}${WDA_ASC_ISSUER_ID:-}" ]; then
        [ -n "${WDA_ASC_KEY_PATH:-}" ] && [ -n "${WDA_ASC_KEY_ID:-}" ] && [ -n "${WDA_ASC_ISSUER_ID:-}" ] \
            || die "Set all three of WDA_ASC_KEY_PATH, WDA_ASC_KEY_ID and WDA_ASC_ISSUER_ID, or none."
        add_env WDA_ASC_KEY_PATH "$WDA_ASC_KEY_PATH"
        add_env WDA_ASC_KEY_ID "$WDA_ASC_KEY_ID"
        add_env WDA_ASC_ISSUER_ID "$WDA_ASC_ISSUER_ID"
    else
        for value in "$plist" "$default_wda_plist"; do
            if [ -n "$(named_plist_env "$value" WDA_ASC_KEY_PATH)" ] \
                && [ -n "$(named_plist_env "$value" WDA_ASC_KEY_ID)" ] \
                && [ -n "$(named_plist_env "$value" WDA_ASC_ISSUER_ID)" ]; then
                for key in WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID; do
                    add_env "$key" "$(named_plist_env "$value" "$key")"
                done
                break
            fi
        done
    fi

    info "Installing instance $INSTANCE_NAME for iPhone $udid"
    for value in "$HOME/.iphone-use" "$HOME/.iphone-use/instances" "$state_dir" "$runtime" "$state_dir/logs" "$state_dir/tmp"; do
        [ ! -L "$value" ] || die "Refusing symlinked instance namespace: $value"
        if [ ! -d "$value" ]; then
            mkdir -m 700 "$value" || die "Could not create $value"
        fi
    done

    # Stage everything, then stop this instance's daemon (only it) and swap.
    app_stage="$runtime/.$APP_NAME.new.$$"
    rm -rf "$app_stage"
    ditto "$app_src" "$app_stage" || die "Could not copy $app_src"
    codesign --verify "$app_stage" 2>/dev/null || { rm -rf "$app_stage"; die "The copied app fails signature verification."; }
    plist_stage="$(mktemp "$HOME/Library/LaunchAgents/.${label}.new.XXXXXX")"
    /usr/bin/plutil -create xml1 "$plist_stage" \
        && /usr/bin/plutil -insert Label -string "$label" "$plist_stage" \
        && /usr/bin/plutil -insert ProgramArguments -array "$plist_stage" \
        && /usr/bin/plutil -insert ProgramArguments.0 -string "$binary" "$plist_stage" \
        && /usr/bin/plutil -insert ProgramArguments.1 -string serve "$plist_stage" \
        && /usr/bin/plutil -insert RunAtLoad -bool true "$plist_stage" \
        && /usr/bin/plutil -insert KeepAlive -bool true "$plist_stage" \
        && /usr/bin/plutil -insert ThrottleInterval -integer 10 "$plist_stage" \
        && /usr/bin/plutil -insert StandardOutPath -string "$state_dir/logs/iphone-use.log" "$plist_stage" \
        && /usr/bin/plutil -insert StandardErrorPath -string "$state_dir/logs/iphone-use.err" "$plist_stage" \
        && /usr/bin/plutil -insert ProcessType -string Interactive "$plist_stage" \
        && /usr/bin/plutil -insert LimitLoadToSessionType -string Aqua "$plist_stage" \
        && /usr/bin/plutil -insert EnvironmentVariables -dictionary "$plist_stage" \
        || { rm -rf "$app_stage" "$plist_stage"; die "Could not build the instance plist."; }
    i=0
    while [ "$i" -lt "${#env_keys[@]}" ]; do
        /usr/bin/plutil -insert "EnvironmentVariables.${env_keys[$i]}" -string "${env_values[$i]}" "$plist_stage" \
            || { rm -rf "$app_stage" "$plist_stage"; die "Could not write ${env_keys[$i]} into the instance plist."; }
        i=$((i + 1))
    done
    chmod 600 "$plist_stage"
    /usr/bin/plutil -lint "$plist_stage" >/dev/null \
        || { rm -rf "$app_stage" "$plist_stage"; die "Generated instance plist is invalid."; }

    if launchctl print "gui/$UID_NUM/$label" >/dev/null 2>&1; then
        was_loaded=1
        launchctl bootout "gui/$UID_NUM/$label" 2>/dev/null || true
        i=0
        while launchctl print "gui/$UID_NUM/$label" >/dev/null 2>&1 && [ "$i" -lt 50 ]; do
            sleep 0.2; i=$((i + 1))
        done
    fi
    if [ -d "$app_dst" ]; then
        app_backup="$runtime/.$APP_NAME.backup.$$"
        mv "$app_dst" "$app_backup" || die "Could not move the previous instance app aside."
    fi
    mv "$app_stage" "$app_dst" || die "Could not install the instance app."
    if [ -f "$plist" ]; then
        plist_backup="$plist_stage.backup"
        cp -p "$plist" "$plist_backup"
    fi
    mv -f "$plist_stage" "$plist"
    for value in setup-wda.sh uninstall.sh; do
        if [ "$value" = setup-wda.sh ]; then key="$setup_src"; else key="$uninstall_src"; fi
        cp -f "$key" "$state_dir/$value.new.$$" && chmod 700 "$state_dir/$value.new.$$" \
            && mv -f "$state_dir/$value.new.$$" "$state_dir/$value" \
            || die "Could not install $state_dir/$value"
    done
    # Every instance builds the device runner from the shared sources.
    if runner_tree_valid "$SCRIPT_DIR/runner"; then
        install_runner_sources "$SCRIPT_DIR/runner"
    elif ! runner_tree_valid "$RUNNER_SRC_DST"; then
        warn "No device runner sources next to this installer or at $RUNNER_SRC_DST; install the default instance first."
    fi

    named_rollback() {
        warn "Rolling back instance $INSTANCE_NAME: $1"
        _restore_runner_sources
        launchctl bootout "gui/$UID_NUM/$label" 2>/dev/null || true
        if [ -n "$plist_backup" ]; then mv -f "$plist_backup" "$plist"; else rm -f "$plist"; fi
        if [ -n "$app_backup" ]; then rm -rf "$app_dst"; mv "$app_backup" "$app_dst"; fi
        if [ "$was_loaded" = 1 ] && [ -f "$plist" ]; then
            launchctl bootstrap "gui/$UID_NUM" "$plist" 2>/dev/null || true
        fi
        die "$1"
    }
    launchctl enable "gui/$UID_NUM/$label" 2>/dev/null || true
    launchctl bootstrap "gui/$UID_NUM" "$plist" 2>/dev/null \
        || named_rollback "launchctl refused $plist"
    code=000
    i=0
    while [ "$i" -lt 40 ]; do
        code="$(curl -s -m 2 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$daemon_port/agent/status" 2>/dev/null || true)"
        case "$code" in 000|'') ;; *) break ;; esac
        sleep 0.5; i=$((i + 1))
    done
    case "$code" in
        000|'') named_rollback "the instance daemon did not answer on 127.0.0.1:$daemon_port (see $state_dir/logs/iphone-use.err)" ;;
    esac
    [ -z "$app_backup" ] || rm -rf "$app_backup"
    [ -z "$plist_backup" ] || rm -f "$plist_backup"
    RUNNER_SRC_COMMITTED=1
    _finish_runner_sources

    ok "Instance $INSTANCE_NAME daemon is up on http://127.0.0.1:$daemon_port"
    echo ""
    info "iPhone      : $udid"
    info "Daemon      : http://127.0.0.1:$daemon_port   (launchd $label)"
    info "Relays      : 127.0.0.1:$wda_port control, 127.0.0.1:$mjpeg_port video (launchd $wda_label)"
    info "Token       : EnvironmentVariables:PHONE_REMOTE_AGENT_TOKEN in $plist"
    info "State       : $state_dir"
    echo ""
    printf '  Agents target this phone with:\n'
    printf "    ${BOLD}PHONE_REMOTE_URL=http://127.0.0.1:%s PHONE_REMOTE_AGENT_TOKEN=<token>${RESET}\n" "$daemon_port"
    printf '  Next, build and start the device runner for this phone (keep it unlocked):\n'
    printf "    ${BOLD}PHONE_REMOTE_INSTANCE=%s %s/setup-wda.sh${RESET}\n" "$INSTANCE_NAME" "$state_dir"
    printf '  Remove only this instance with:\n'
    printf "    ${BOLD}%s/uninstall.sh --instance %s${RESET}\n" "$state_dir" "$INSTANCE_NAME"
    exit 0
}

if [ "$INSTANCE_NAME" != default ]; then
    install_named_instance "$@"
fi

if [ "${IPHONE_USE_INTERNAL_TEST_RESOLVE_COMMIT_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal release-resolution hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal release-resolution sentinel is missing."
    resolve_release_commit "${IPHONE_USE_INTERNAL_TEST_RELEASE_REF:-v-test}"
    printf '\n'
    exit 0
fi

# Exercises the device-runner source transaction (and the release archive
# checks) in an isolated fake HOME. Same gating as the hooks around it.
if [ "${IPHONE_USE_INTERNAL_TEST_RUNNER_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal runner-source test hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal runner-source test sentinel is missing."
    case "${IPHONE_USE_INTERNAL_TEST_RUNNER_SRC:-}${IPHONE_USE_INTERNAL_TEST_RUNNER_ARCHIVE:-}" in
        "$HOME"/*) ;;
        *) die "The internal runner fixture must be inside the isolated HOME." ;;
    esac
    if [ -n "${IPHONE_USE_INTERNAL_TEST_RUNNER_ARCHIVE:-}" ]; then
        RUNNER_SRC_DL="$(mktemp -d "$HOME/.runner-archive.XXXXXX")"
        extract_verified_runner_archive v-test "$IPHONE_USE_INTERNAL_TEST_RUNNER_ARCHIVE" "$RUNNER_SRC_DL"
        install_runner_sources "$RUNNER_SRC_DL/runner"
    else
        install_runner_sources "$IPHONE_USE_INTERNAL_TEST_RUNNER_SRC"
    fi
    if [ "${IPHONE_USE_INTERNAL_TEST_RUNNER_FAIL:-0}" = "1" ]; then
        die "simulated failure after the runner sources were swapped in"
    fi
    RUNNER_SRC_COMMITTED=1
    _finish_runner_sources
    exit 0
fi

if [ "${IPHONE_USE_INTERNAL_TEST_ARCHIVE_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal archive test hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal archive test sentinel is missing."
    case "${IPHONE_USE_INTERNAL_TEST_ARCHIVE:-}" in
        "$HOME"/*) ;;
        *) die "The internal archive fixture must be inside the isolated HOME." ;;
    esac
    case "${IPHONE_USE_INTERNAL_TEST_ARCHIVE_DEST:-}" in
        "$HOME"/*) ;;
        *) die "The internal archive destination must be inside the isolated HOME." ;;
    esac
    extract_verified_release_app_archive \
        "v-test" \
        "$IPHONE_USE_INTERNAL_TEST_ARCHIVE" \
        "$IPHONE_USE_INTERNAL_TEST_ARCHIVE_DEST"
    exit 0
fi

# This hook exercises the real skill transaction in an isolated fake HOME. It
# is intentionally unavailable unless both a local installer and a sentinel
# owned by that fake HOME are present.
if [ "${IPHONE_USE_INTERNAL_TEST_SKILL_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal skill-transaction test hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal skill-transaction test sentinel is missing."
    case "${IPHONE_USE_INTERNAL_TEST_EXPECTED:-}" in
        "$HOME"/*) ;;
        *) die "The internal skill test source must be inside the isolated HOME." ;;
    esac
    _install_verified_skill "$IPHONE_USE_INTERNAL_TEST_EXPECTED" \
        "v-test" "0123456789abcdef0123456789abcdef01234567" "test"
    if [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_LOCK_AFTER_INSTALL:-0}" = "1" ]; then
        /usr/bin/plutil -insert 'skills.concurrent-test' \
            -json '{"source":"concurrent/test"}' \
            "$SKILL_DEFAULT_LOCK_PATH" \
            || die "Could not create the internal concurrent-lock fixture."
    fi
    if [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_CLAUDE_AFTER_INSTALL:-0}" = "1" ]; then
        rm -f "$SKILL_CLAUDE_LINK" \
            || die "Could not replace the internal Claude-link fixture."
        mkdir "$SKILL_CLAUDE_LINK" \
            || die "Could not create the internal concurrent Claude target."
        printf '%s\n' 'concurrent Claude target' \
            > "$SKILL_CLAUDE_LINK/SKILL.md" \
            || die "Could not populate the internal concurrent Claude target."
    fi
    if [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_CANONICAL_AFTER_INSTALL:-0}" = "1" ]; then
        printf '%s\n' 'concurrent canonical mutation' \
            >> "$SKILL_CANONICAL_DIR/SKILL.md" \
            || die "Could not create the internal concurrent canonical fixture."
    fi
    if [ "${IPHONE_USE_INTERNAL_TEST_FORCE_FAILURE:-0}" = "1" ]; then
        die "Forced failure after skill replacement for rollback verification."
    fi
    _commit_skill_transaction
    exit 0
fi

if [ "${IPHONE_USE_INTERNAL_TEST_BOOTSTRAP_VERIFY_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal bootstrap-verification hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal bootstrap-verification sentinel is missing."
    prepare_companion_skill
    exit 0
fi

if [ "${IPHONE_USE_INTERNAL_TEST_DAEMON_COMMIT_VERIFY_ONLY:-0}" = "1" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal daemon-commit verification hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal daemon-commit verification sentinel is missing."
    prepare_companion_skill
    if [ "${IPHONE_USE_INTERNAL_TEST_MUTATE_BOOTSTRAP_LOCK_SYMLINK:-0}" = "1" ]; then
        [ -f "$SKILL_DEFAULT_LOCK_PATH" ] \
            || die "The internal bootstrap-lock fixture requires an existing lock."
        /bin/mv "$SKILL_DEFAULT_LOCK_PATH" \
            "$SKILL_DEFAULT_LOCK_PATH.bootstrap-target" \
            || die "Could not stage the internal bootstrap-lock target."
        /bin/ln -s "$(basename "$SKILL_DEFAULT_LOCK_PATH.bootstrap-target")" \
            "$SKILL_DEFAULT_LOCK_PATH" \
            || die "Could not create the internal bootstrap-lock symlink fixture."
    fi
    _verify_skill_before_daemon_commit
    exit 0
fi

if [ -n "${IPHONE_USE_INTERNAL_TEST_COMMIT_STATE_ONLY:-}" ]; then
    [ "$SCRIPT_IS_LOCAL" = "1" ] \
        || die "The internal commit-state hook requires a local installer."
    [ -f "$HOME/.iphone-use-installer-test-root" ] \
        || die "The internal commit-state sentinel is missing."
    SKILL_SYNC_KIND="bootstrap"
    case "$IPHONE_USE_INTERNAL_TEST_COMMIT_STATE_ONLY" in
        pending-before)
            INSTALL_COMMIT_SIGNAL_PENDING=1
            if _commit_daemon_transaction; then
                die "The internal pre-commit pending fixture unexpectedly committed."
            else
                internal_commit_status=$?
            fi
            [ "$internal_commit_status" -eq 130 ] \
                || die "The internal pre-commit fixture returned an unexpected status."
            [ "$INSTALL_DAEMON_COMMITTED" = "0" ] \
                && [ "$APP_COMMITTED" = "0" ] \
                && [ "$PLIST_COMMITTED" = "0" ] \
                && [ "$DAEMON_RUNTIME_COMMITTED" = "0" ] \
                || die "The internal pre-commit fixture set a commit flag."
            exit "$internal_commit_status"
            ;;
        failure-after)
            INSTALL_COMMIT_SIGNAL_PENDING=0
            _commit_daemon_transaction \
                || die "The internal post-commit fixture could not commit."
            INSTALL_COMMIT_SIGNAL_PENDING=1
            [ "$INSTALL_DAEMON_COMMITTED" = "1" ] \
                && [ "$APP_COMMITTED" = "1" ] \
                && [ "$PLIST_COMMITTED" = "1" ] \
                && [ "$DAEMON_RUNTIME_COMMITTED" = "1" ] \
                || die "The internal post-commit fixture missed a commit flag."
            exit 77
            ;;
        *)
            die "Unknown internal commit-state fixture: $IPHONE_USE_INTERNAL_TEST_COMMIT_STATE_ONLY"
            ;;
    esac
fi

# Skill and daemon are one transaction. Install and byte-verify the exact
# companion skill before any app/plist/launchd mutation; cleanup restores it if
# a later daemon step fails.
prepare_companion_skill "$@"

# Snapshot both product daemon labels before replacing files or touching
# launchd. If a later enable/bootstrap/print step fails, cleanup restores these
# exact persistent and loaded states after restoring the old app/plists.
if ! LAUNCHD_DISABLED_SNAPSHOT="$(launchctl print-disabled "gui/$UID_NUM" 2>/dev/null)"; then
    die "Could not snapshot launchd disabled states for gui/$UID_NUM; refusing a non-atomic install."
fi
if launchctl print "gui/$UID_NUM/$PLIST_LABEL" >/dev/null 2>&1; then
    PREINSTALL_DAEMON_LOADED=1
fi
if launchd_label_is_disabled "$PLIST_LABEL"; then
    PREINSTALL_DAEMON_DISABLED=1
fi
if launchctl print "gui/$UID_NUM/$OLD_PLIST_LABEL" >/dev/null 2>&1; then
    PREINSTALL_OLD_DAEMON_LOADED=1
fi
if launchd_label_is_disabled "$OLD_PLIST_LABEL"; then
    PREINSTALL_OLD_DAEMON_DISABLED=1
fi
if launchctl print "gui/$UID_NUM/$WDA_PLIST_LABEL" >/dev/null 2>&1; then
    PREINSTALL_WDA_LOADED=1
fi
if launchd_label_is_disabled "$WDA_PLIST_LABEL"; then
    PREINSTALL_WDA_DISABLED=1
fi
[ "$PREINSTALL_DAEMON_LOADED" = "0" ] || [ -f "$PLIST_DST" ] \
    || die "The current product daemon is loaded but its expected plist is missing: $PLIST_DST"
[ "$PREINSTALL_OLD_DAEMON_LOADED" = "0" ] || [ -f "$OLD_PLIST" ] \
    || die "The legacy product daemon is loaded but its expected plist is missing: $OLD_PLIST"

# Read installer-owned values before signing: the selected backend determines
# whether a stable local signing identity is useful at all. Keep this as the
# single backend-resolution path for both signing and plist generation.
plist_env_get_from() {
    local plist="$1"
    local key="$2"
    [ -f "$plist" ] || { printf ''; return; }
    /usr/libexec/PlistBuddy -c "Print :EnvironmentVariables:$key" "$plist" 2>/dev/null || printf ''
}

plist_env_get() {
    local key="$1"
    local value
    value="$(plist_env_get_from "$PLIST_DST" "$key")"
    [ -n "$value" ] || value="$(plist_env_get_from "$OLD_PLIST" "$key")"
    printf '%s' "$value"
}

env_or_existing() {
    local key="$1"
    local fallback="${2:-}"
    local value
    value="$(printenv "$key" 2>/dev/null || true)"
    [ -n "$value" ] || value="$(plist_env_get "$key")"
    [ -n "$value" ] || value="$fallback"
    printf '%s' "$value"
}

# The iPhone Mirroring backend was removed in v0.9; every install serves the
# phone over WDA. An older plist may still say mirror: say so and move on.
if [ "${PHONE_REMOTE_BACKEND:-$(plist_env_get PHONE_REMOTE_BACKEND)}" = "mirror" ] \
    || [ "${PHONE_REMOTE_BACKEND:-$(plist_env_get PHONE_REMOTE_BACKEND)}" = "legacy-mirror" ]; then
    warn "The iPhone Mirroring backend was removed in v0.9; this install serves the phone over WDA."
    warn "  Run setup-wda.sh once if this Mac has never set the device runner up."
fi

# ── Step 1 — Obtain the .app ──────────────────────────────────────────────────
APP_SRC=""
if [ -n "${1:-}" ]; then
    # Local path supplied (dev / CI / re-install)
    APP_SRC="$1"
    if [ ! -d "$APP_SRC" ]; then
        die "Supplied path is not a directory: $APP_SRC"
    fi
    ok "Using local app: $APP_SRC"
else
    # Freeze both the release name and its exact source commit before the first
    # network artifact is fetched; every later fallback helper reuses them.
    ensure_release_commit
    info "Fetching release $RELEASE_REF from github.com/$REPO ..."
    TMPDIR_INSTALL="$(mktemp -d)"
    download_verified_release_app "$RELEASE_REF" "$TMPDIR_INSTALL"
    APP_SRC="$TMPDIR_INSTALL/$APP_NAME"
    ok "Downloaded and extracted: $APP_SRC"
fi

# Work on an installer-owned copy so local/dev installs never mutate the source
# app (including the currently installed app) while removing quarantine or
# applying the local signature.
mkdir -p "$INSTALL_DIR"
DEST="$INSTALL_DIR/$APP_NAME"
APP_STAGE="$(mktemp -d "$INSTALL_DIR/.${APP_NAME}.new.XXXXXX")"
rmdir "$APP_STAGE"
cp -R "$APP_SRC" "$APP_STAGE"
[ -x "$APP_STAGE/$BINARY_INSIDE_APP" ] \
    || die "Staged app is missing its executable: $BINARY_INSIDE_APP"
[ -x "$APP_STAGE/$MCP_BINARY_INSIDE_APP" ] \
    || die "Staged app is missing its MCP executable: $MCP_BINARY_INSIDE_APP"

# ── Step 2 — Remove quarantine ────────────────────────────────────────────────
info "Removing quarantine attribute ..."
xattr -dr com.apple.quarantine "$APP_STAGE" 2>/dev/null || true
ok "Quarantine cleared"

# ── Step 3 — Validate or repair the signature ─────────────────────────────────
# The daemon needs no TCC identity. Preserve any valid release/local signature
# verbatim (Developer ID or ad-hoc), avoiding needless certificate creation and
# keychain mutation.
if codesign --verify --deep --strict "$APP_STAGE" 2>/dev/null; then
    ok "Valid existing app signature preserved."
else
    warn "App is unsigned or has an invalid signature; applying keychain-free ad-hoc signing."
    _inline_sign "$APP_STAGE"
fi

# ── Step 4 — Verify bundle-id in signature ────────────────────────────────────
FINAL_ID="$(codesign --display --verbose=4 "$APP_STAGE" 2>&1 \
            | grep 'Identifier=' | head -1 \
            | sed 's/.*Identifier=\(.*\)/\1/' || true)"
if [ "$FINAL_ID" != "$BUNDLE_ID" ]; then
    die "Signed bundle-id '$FINAL_ID' does not match expected '$BUNDLE_ID'. Check deploy/Info.plist."
fi
ok "Bundle-id verified: $BUNDLE_ID"

# ── Step 5 — Install .app ─────────────────────────────────────────────────────
# Copy and verify beside the destination before touching a working install.
# The backup stays in the same filesystem so replacement and rollback use
# rename rather than a second fallible cross-volume copy.
[ -x "$APP_STAGE/$BINARY_INSIDE_APP" ] \
    || die "Staged app is missing its executable: $BINARY_INSIDE_APP"
[ -x "$APP_STAGE/$MCP_BINARY_INSIDE_APP" ] \
    || die "Staged app is missing its MCP executable: $MCP_BINARY_INSIDE_APP"
codesign --verify --deep --strict "$APP_STAGE" 2>/dev/null \
    || die "Staged app failed code-signature verification."

if [ -d "$DEST" ]; then
    APP_HAD_EXISTING=1
    APP_BACKUP="$(mktemp -d "$INSTALL_DIR/.${APP_NAME}.backup.XXXXXX")"
    rmdir "$APP_BACKUP"
    mv "$DEST" "$APP_BACKUP"
fi
APP_REPLACED=1
mv "$APP_STAGE" "$DEST"
APP_STAGE=""
ok "Installed: $DEST"

# ── Step 6 — Create log directory ─────────────────────────────────────────────
mkdir -p "$LOG_DIR"
ok "Log directory: $LOG_DIR"

# ── Step 6b — Resolve persisted environment ───────────────────────────────────
# Every installer-owned value below uses the same precedence:
# explicit environment > current plist > pre-rename plist > documented default.
# The old job is evicted only after these values have been copied into, and the
# generated replacement has passed plutil validation.
wda_env_or_existing() {
    local key="$1"
    local fallback="${2:-}"
    local value
    value="$(printenv "$key" 2>/dev/null || true)"
    [ -n "$value" ] || value="$(plist_env_get "$key")"
    if [ -z "$value" ] && [ -f "$WDA_PLIST_DST" ]; then
        value="$(/usr/libexec/PlistBuddy \
            -c "Print :EnvironmentVariables:$key" "$WDA_PLIST_DST" 2>/dev/null || true)"
    fi
    [ -n "$value" ] || value="$fallback"
    printf '%s' "$value"
}

# Values are inserted into XML element text, so escape the three characters that
# can make an otherwise-valid LaunchAgent plist unparsable.
xml_escape() {
    printf '%s' "$1" \
        | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

PLIST_ENV_BLOCK=""
append_plist_env() {
    local key="$1"
    local value="${2:-}"
    local escaped
    [ -n "$value" ] || return 0
    escaped="$(xml_escape "$value")"
    PLIST_ENV_BLOCK="${PLIST_ENV_BLOCK}        <key>${key}</key>
        <string>${escaped}</string>
"
}

# The iPhone reaches the web UI over the LAN, so bind 0.0.0.0 by default.
# A password remains mandatory here — generated only when neither the current
# environment nor the previous install supplies one.
HOST="$(env_or_existing PHONE_REMOTE_HOST 0.0.0.0)"
PORT="$(env_or_existing PHONE_REMOTE_PORT 44321)"
HOST="$(printf '%s' "$HOST" \
    | LC_ALL=C sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
case "$HOST" in
    ""|*[!A-Za-z0-9._:-]*)
        die "PHONE_REMOTE_HOST must be a non-empty IP address or hostname without spaces/control characters."
        ;;
esac
case "$PORT" in
    ""|*[!0-9]*)
        die "PHONE_REMOTE_PORT must be a decimal integer from 1 to 65535 (got '$PORT')."
        ;;
esac
PORT_NORMALIZED="$(printf '%s' "$PORT" | sed 's/^0*//')"
[ -n "$PORT_NORMALIZED" ] || PORT_NORMALIZED="0"
if [ "${#PORT_NORMALIZED}" -gt 5 ] \
    || { [ "${#PORT_NORMALIZED}" -eq 5 ] && [ "$PORT_NORMALIZED" -gt 65535 ]; } \
    || [ "$PORT_NORMALIZED" = "0" ]; then
    die "PHONE_REMOTE_PORT must be a decimal integer from 1 to 65535 (got '$PORT')."
fi
PORT="$PORT_NORMALIZED"

# Password precedence: explicit env > existing plist > freshly generated.
# Re-running install must NOT rotate a working password (it would break every
# saved client/bookmark) — only mint one when there genuinely isn't one yet.
PASSWORD="${PHONE_REMOTE_PASSWORD:-}"
PW_SOURCE="env"
if [ -z "$PASSWORD" ]; then
    PASSWORD="$(plist_env_get PHONE_REMOTE_PASSWORD)"
    PW_SOURCE="existing"
fi
if [ -z "$PASSWORD" ]; then
    PASSWORD="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 16 || true)"
    [ -n "$PASSWORD" ] || PASSWORD="$(date +%s | shasum | head -c 16)"
    PW_SOURCE="generated"
fi
case "$PW_SOURCE" in
    env)      ok "Using password from \$PHONE_REMOTE_PASSWORD" ;;
    existing) ok "Reusing the password from the existing install" ;;
    generated) ok "Generated a random access password (shown at the end)" ;;
esac

# Best-effort LAN IP for the final connect instructions.
LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
[ -n "$LAN_IP" ] || LAN_IP="<this-mac-LAN-ip>"

# Direct mode defaults to deterministic localhost endpoints. setup-wda.sh owns
# the USB iproxy relays behind these URLs; a LAN/socat relay is available only
# through the explicit WDA_ALLOW_LAN=1 escape hatch. WDA itself has no auth, so
# the Mac and phone still belong on a trusted or isolated network.
WDA_URL="$(env_or_existing PHONE_REMOTE_WDA_URL)"
WDA_MJPEG_URL="$(env_or_existing PHONE_REMOTE_WDA_MJPEG_URL)"
if [ -z "$WDA_URL" ] && [ -z "$WDA_MJPEG_URL" ]; then
    WDA_URL="http://127.0.0.1:8100"
    WDA_MJPEG_URL="http://127.0.0.1:9100"
elif [ -z "$WDA_URL" ]; then
    if is_loopback_wda_url "$WDA_MJPEG_URL"; then
        WDA_URL="http://127.0.0.1:8100"
    else
        die "External Direct WDA requires both PHONE_REMOTE_WDA_URL and PHONE_REMOTE_WDA_MJPEG_URL; refusing a remote/local split."
    fi
elif [ -z "$WDA_MJPEG_URL" ]; then
    if is_loopback_wda_url "$WDA_URL"; then
        WDA_MJPEG_URL="http://127.0.0.1:9100"
    else
        die "External Direct WDA requires both PHONE_REMOTE_WDA_URL and PHONE_REMOTE_WDA_MJPEG_URL; refusing a remote/local split."
    fi
fi
is_network_wda_url "$WDA_URL" \
    || die "PHONE_REMOTE_WDA_URL must be an explicit HTTP(S) host and port without credentials, query, or fragment."
is_network_wda_url "$WDA_MJPEG_URL" \
    || die "PHONE_REMOTE_WDA_MJPEG_URL must be an explicit HTTP(S) host and port without credentials, query, or fragment."
if is_loopback_wda_url "$WDA_URL"; then
    WDA_CONTROL_LOOPBACK=1
else
    WDA_CONTROL_LOOPBACK=0
fi
if is_loopback_wda_url "$WDA_MJPEG_URL"; then
    WDA_MJPEG_LOOPBACK=1
else
    WDA_MJPEG_LOOPBACK=0
fi
[ "$WDA_CONTROL_LOOPBACK" = "$WDA_MJPEG_LOOPBACK" ] \
    || die "Direct control and MJPEG endpoints must both be loopback or both be external; mixed routing is unsafe."
ok "WDA control endpoint: $WDA_URL"
ok "WDA video endpoint: $WDA_MJPEG_URL"

# Recognize the product-owned supervisor shipped before
# PHONE_REMOTE_WDA_MANAGED existed. Exact label + exact fixed setup-script path
# avoids claiming an unrelated/custom WDA service merely because a plist exists.
legacy_product_wda_supervisor_owned() {
    local label
    local program
    [ -f "$WDA_PLIST_DST" ] || return 1
    label="$(/usr/libexec/PlistBuddy -c "Print :Label" "$WDA_PLIST_DST" 2>/dev/null || true)"
    program="$(/usr/libexec/PlistBuddy -c "Print :ProgramArguments:1" "$WDA_PLIST_DST" 2>/dev/null || true)"
    [ "$label" = "$WDA_PLIST_LABEL" ] \
        && [ "$program" = "$HOME/.iphone-use/setup-wda.sh" ]
}

# The installer owns the default loopback supervisor/relay pair. A custom or
# remote WDA URL is external by default and must never be stopped or rewritten
# as though this installer owned it. An explicit value always wins.
PRODUCT_WDA_SUPERVISOR_OWNED=0
if legacy_product_wda_supervisor_owned; then
    PRODUCT_WDA_SUPERVISOR_OWNED=1
fi
WDA_MANAGED="$(env_or_existing PHONE_REMOTE_WDA_MANAGED)"
if [ -z "$WDA_MANAGED" ]; then
    if [ "$PRODUCT_WDA_SUPERVISOR_OWNED" = "1" ]; then
        WDA_MANAGED="true"
        [ -n "$WDA_URL" ] || WDA_URL="http://127.0.0.1:8100"
        [ -n "$WDA_MJPEG_URL" ] || WDA_MJPEG_URL="http://127.0.0.1:9100"
        info "Legacy product-owned WDA supervisor detected; migrating lifecycle ownership."
    elif [ "$WDA_URL" = "http://127.0.0.1:8100" ] \
        && [ "$WDA_MJPEG_URL" = "http://127.0.0.1:9100" ]; then
        WDA_MANAGED="true"
    else
        WDA_MANAGED="false"
    fi
fi
case "$WDA_MANAGED" in
    1|true|TRUE|yes|YES) WDA_MANAGED="true" ;;
    0|false|FALSE|no|NO) WDA_MANAGED="false" ;;
    *) die "PHONE_REMOTE_WDA_MANAGED must be true or false (got '$WDA_MANAGED')." ;;
esac
if [ "$WDA_MANAGED" = "true" ] \
    && { ! is_loopback_wda_url "$WDA_URL" \
        || ! is_loopback_wda_url "$WDA_MJPEG_URL"; }; then
    die "PHONE_REMOTE_WDA_MANAGED=true requires loopback HTTP control and MJPEG URLs (127.0.0.1 or localhost). Set it to false for external WDA."
fi
ok "WDA lifecycle managed by iphone-use: $WDA_MANAGED"

# Persist one explicit device target across browser reconnects, idle release,
# daemon upgrades, and multi-device Macs. Accept WDA_UDID as a first-install
# convenience, but store the daemon's canonical PHONE_REMOTE_UDID key.
DEVICE_UDID="$(env_or_existing PHONE_REMOTE_UDID "${WDA_UDID:-}")"
if [ -n "$DEVICE_UDID" ] && ! printf '%s' "$DEVICE_UDID" | grep -Eq '^[0-9A-Fa-f-]+$'; then
    die "PHONE_REMOTE_UDID contains invalid characters (expected hex and dashes)."
fi
[ -z "$DEVICE_UDID" ] || ok "Target iPhone fixed to UDID: $DEVICE_UDID"

# Keep ~/.iphone-use/setup-wda.sh fresh on EVERY install. The daemon's
# POST /agent/mode runs this copy to build/relaunch WDA (and relay its MJPEG
# video port). Without this step the script only ever self-installs the OLD copy
# the daemon re-spawns, so setup-wda.sh fixes never reach upgraders. Prefer a
# local checkout, else fetch from the exact release commit selected above.
SETUP_WDA_DST="$HOME/.iphone-use/setup-wda.sh"
mkdir -p "$HOME/.iphone-use" 2>/dev/null || true
SETUP_WDA_SRC=""
if [ "$SCRIPT_IS_LOCAL" = "1" ] && [ -f "$SCRIPT_DIR/scripts/setup-wda.sh" ]; then
    SETUP_WDA_SRC="$SCRIPT_DIR/scripts/setup-wda.sh"
else
    ensure_release_commit
    SETUP_WDA_DL="$(mktemp)"
    curl -fsSL "https://raw.githubusercontent.com/$REPO/$RELEASE_COMMIT/scripts/setup-wda.sh" \
        -o "$SETUP_WDA_DL" \
        || die "Could not fetch setup-wda.sh from release $RELEASE_REF commit $RELEASE_COMMIT."
    [ -s "$SETUP_WDA_DL" ] \
        || die "Release $RELEASE_REF contains an empty setup-wda.sh."
    SETUP_WDA_SRC="$SETUP_WDA_DL"
fi

# Stage and syntax-check before touching the fixed hand-off path. Keep a backup
# until the app and generated daemon plist have both passed validation.
SETUP_WDA_STAGE="$(mktemp "$HOME/.iphone-use/setup-wda.sh.new.XXXXXX")"
cp -f "$SETUP_WDA_SRC" "$SETUP_WDA_STAGE"
chmod 700 "$SETUP_WDA_STAGE"
/bin/bash -n "$SETUP_WDA_STAGE" \
    || die "The release-matched setup-wda.sh failed Bash syntax validation."
if [ -f "$SETUP_WDA_DST" ]; then
    SETUP_WDA_HAD_EXISTING=1
    SETUP_WDA_BACKUP="$(mktemp "$HOME/.iphone-use/setup-wda.sh.backup.XXXXXX")"
    cp -p "$SETUP_WDA_DST" "$SETUP_WDA_BACKUP"
fi
mv -f "$SETUP_WDA_STAGE" "$SETUP_WDA_DST"
SETUP_WDA_STAGE=""
SETUP_WDA_REPLACED=1
ok "WDA setup script atomically updated: $SETUP_WDA_DST"

# Install the standalone uninstaller from the same local checkout or exact
# release commit as this installer. It participates in the same rollback transaction
# as the app, setup helper, and LaunchAgent plist.
UNINSTALL_DST="$HOME/.iphone-use/uninstall.sh"
UNINSTALL_SRC=""
if [ "$SCRIPT_IS_LOCAL" = "1" ] && [ -f "$SCRIPT_DIR/uninstall.sh" ]; then
    UNINSTALL_SRC="$SCRIPT_DIR/uninstall.sh"
else
    ensure_release_commit
    UNINSTALL_DL="$(mktemp)"
    curl -fsSL "https://raw.githubusercontent.com/$REPO/$RELEASE_COMMIT/uninstall.sh" \
        -o "$UNINSTALL_DL" \
        || die "Could not fetch uninstall.sh from release $RELEASE_REF commit $RELEASE_COMMIT."
    [ -s "$UNINSTALL_DL" ] \
        || die "Release $RELEASE_REF contains an empty uninstall.sh."
    UNINSTALL_SRC="$UNINSTALL_DL"
fi
UNINSTALL_STAGE="$(mktemp "$HOME/.iphone-use/uninstall.sh.new.XXXXXX")"
cp -f "$UNINSTALL_SRC" "$UNINSTALL_STAGE"
chmod 700 "$UNINSTALL_STAGE"
/bin/bash -n "$UNINSTALL_STAGE" \
    || die "The release-matched uninstall.sh failed Bash syntax validation."
if [ -f "$UNINSTALL_DST" ]; then
    UNINSTALL_HAD_EXISTING=1
    UNINSTALL_BACKUP="$(mktemp "$HOME/.iphone-use/uninstall.sh.backup.XXXXXX")"
    cp -p "$UNINSTALL_DST" "$UNINSTALL_BACKUP"
fi
mv -f "$UNINSTALL_STAGE" "$UNINSTALL_DST"
UNINSTALL_STAGE=""
UNINSTALL_REPLACED=1
ok "Uninstaller atomically updated: $UNINSTALL_DST"

# Device runner sources for setup-wda.sh, from the same checkout or release.
if resolve_runner_source; then
    install_runner_sources "$RUNNER_SOURCE_DIR"
elif runner_tree_valid "$RUNNER_SRC_DST"; then
    warn "Release ${RELEASE_REF:-?} ships no device runner sources; keeping the existing $RUNNER_SRC_DST."
else
    warn "Release ${RELEASE_REF:-?} ships no device runner sources; setup-wda.sh will report them missing until a release that has them is installed."
fi

info "For cross-network access, put an authenticated HTTPS reverse proxy or a trusted VPN/tunnel in front of the daemon; never expose WDA ports."

# Persist every daemon setting the installer knows about, not just the four
# headline values. This is intentionally an allow-list: it preserves supported
# configuration without copying arbitrary/untrusted LaunchAgent environment.
append_plist_env RUST_LOG "$(env_or_existing RUST_LOG info)"
append_plist_env PHONE_REMOTE_HOST "$HOST"
append_plist_env PHONE_REMOTE_PORT "$PORT"
append_plist_env PHONE_REMOTE_PASSWORD "$PASSWORD"
append_plist_env PHONE_REMOTE_UDID "$DEVICE_UDID"
append_plist_env PHONE_REMOTE_WDA_URL "$WDA_URL"
append_plist_env PHONE_REMOTE_WDA_MJPEG_URL "$WDA_MJPEG_URL"
append_plist_env PHONE_REMOTE_WDA_MANAGED "$WDA_MANAGED"
for ENV_KEY in \
    PHONE_REMOTE_AGENT_TOKEN \
    PHONE_REMOTE_NO_UPDATE_CHECK \
    PHONE_REMOTE_SECRET \
    PHONE_REMOTE_SESSION_TTL \
    PHONE_REMOTE_STATE_DIR \
    PHONE_REMOTE_TOKEN \
    PHONE_REMOTE_URL
do
    append_plist_env "$ENV_KEY" "$(env_or_existing "$ENV_KEY")"
done
# Unset means the daemon's default: release the phone after 600 idle seconds
# and bring WDA up again on the next request. A stored value (including 0,
# "keep the runner up") is the operator's choice and is carried forward.
IDLE_RELEASE_SECS="$(env_or_existing PHONE_REMOTE_IDLE_RELEASE_SECS)"
append_plist_env PHONE_REMOTE_IDLE_RELEASE_SECS "$IDLE_RELEASE_SECS"
for ENV_KEY in \
    WDA_TEAM_ID \
    WDA_BUNDLE_ID \
    IPU_RUNNER_SRC \
    WDA_PORT \
    MJPEG_PORT \
    WDA_ALLOW_LAN
do
    append_plist_env "$ENV_KEY" "$(wda_env_or_existing "$ENV_KEY")"
done
# App Store Connect API-key signing only works with all three keys, and a
# partial set must never mix with a different saved key: carry them as one
# set from the first source that has all three (#67: an upgrade dropped them).
ASC_SET_SOURCE=""
if [ -n "${WDA_ASC_KEY_PATH:-}${WDA_ASC_KEY_ID:-}${WDA_ASC_ISSUER_ID:-}" ]; then
    [ -n "${WDA_ASC_KEY_PATH:-}" ] && [ -n "${WDA_ASC_KEY_ID:-}" ] && [ -n "${WDA_ASC_ISSUER_ID:-}" ] \
        || die "Set all three of WDA_ASC_KEY_PATH, WDA_ASC_KEY_ID and WDA_ASC_ISSUER_ID, or none."
    for ENV_KEY in WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID; do
        append_plist_env "$ENV_KEY" "$(printenv "$ENV_KEY")"
    done
else
    for ASC_SET_SOURCE in "$PLIST_DST" "$WDA_PLIST_DST"; do
        if [ -n "$(plist_env_get_from "$ASC_SET_SOURCE" WDA_ASC_KEY_PATH)" ] \
            && [ -n "$(plist_env_get_from "$ASC_SET_SOURCE" WDA_ASC_KEY_ID)" ] \
            && [ -n "$(plist_env_get_from "$ASC_SET_SOURCE" WDA_ASC_ISSUER_ID)" ]; then
            for ENV_KEY in WDA_ASC_KEY_PATH WDA_ASC_KEY_ID WDA_ASC_ISSUER_ID; do
                append_plist_env "$ENV_KEY" "$(plist_env_get_from "$ASC_SET_SOURCE" "$ENV_KEY")"
            done
            break
        fi
    done
fi

# ── Step 7 — Write the LaunchAgent plist ─────────────────────────────────────
mkdir -p "$HOME/Library/LaunchAgents"
BINARY_PATH="$DEST/$BINARY_INSIDE_APP"
BINARY_PATH_XML="$(xml_escape "$BINARY_PATH")"
LOG_DIR_XML="$(xml_escape "$LOG_DIR")"
PLIST_STAGE="$(mktemp "$HOME/Library/LaunchAgents/.${PLIST_LABEL}.new.XXXXXX")"

cat > "$PLIST_STAGE" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${BINARY_PATH_XML}</string>
        <string>serve</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <!-- Cap the relaunch rate: if startup fails (device not ready, port in use)
         launchd would otherwise relaunch instantly, pegging launchservicesd at
         100% CPU and ballooning the err log (issue #28). The daemon also backs
         off ~30s before exiting unattended; this bounds it at the launchd layer. -->
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>${LOG_DIR_XML}/iphone-use.log</string>
    <key>StandardErrorPath</key>
    <string>${LOG_DIR_XML}/iphone-use.err</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>EnvironmentVariables</key>
    <dict>
${PLIST_ENV_BLOCK}    </dict>
</dict>
</plist>
PLIST

if ! plutil -lint "$PLIST_STAGE" >/dev/null 2>&1; then
    die "Generated LaunchAgent plist is invalid; the previous configuration was left untouched."
fi
[ -x "$BINARY_PATH" ] \
    || die "Installed app is missing its executable: $BINARY_PATH"

# The plist embeds the password in plaintext — lock it to the user only.
chmod 600 "$PLIST_STAGE"
if [ -f "$PLIST_DST" ]; then
    PLIST_HAD_EXISTING=1
    PLIST_BACKUP="$(mktemp "$HOME/Library/LaunchAgents/.${PLIST_LABEL}.backup.XXXXXX")"
    rm -f "$PLIST_BACKUP"
    mv "$PLIST_DST" "$PLIST_BACKUP"
fi
PLIST_REPLACED=1
mv "$PLIST_STAGE" "$PLIST_DST"
PLIST_STAGE=""

ok "LaunchAgent plist written: $PLIST_DST"

# Settle the WDA supervisor's enabled state before committing the artifact
# backups, so a failure here restores the previous install.
if [ "$PRODUCT_WDA_SUPERVISOR_OWNED" = "1" ] \
    && [ "$WDA_MANAGED" = "true" ]; then
    if [ "$PREINSTALL_WDA_DISABLED" = "1" ] && [ "${IDLE_RELEASE_SECS:-}" != "0" ]; then
        # A disabled supervisor in Direct mode is a released phone, parked on
        # purpose. Enabling it here made the next login start the runner, and
        # iOS then asked for the passcode on a phone nobody had asked for. The
        # daemon enables it again on the next agent request.
        ok "Product WDA supervisor stays parked; the daemon starts it on the next agent request"
    else
        if [ "$PREINSTALL_WDA_DISABLED" = "1" ]; then
            WDA_RUNTIME_TOUCHED=1
        fi
        launchctl enable "gui/$UID_NUM/$WDA_PLIST_LABEL" 2>/dev/null \
            || die "Could not re-enable the product WDA supervisor for Direct mode"
        ok "Product WDA supervisor enabled for Direct mode"
    fi
fi

# ── Step 8 — Start or restart the LaunchAgent (no sudo; gui/$UID) ─────────────
# The daemon intentionally starts while WDA is down: its HTTP/UI control plane
# explains the missing prerequisite and offers recovery. Keep the product
# reachable instead of turning setup state into a connection-refused page.
DAEMON_SHOULD_START=1
WDA_READY=0
if command -v curl >/dev/null 2>&1 \
    && curl -fsS --noproxy '*' -m 4 "${WDA_URL%/}/status" >/dev/null 2>&1; then
    WDA_READY=1
    ok "Existing WDA endpoint verified; the direct daemon can start now"
elif launchctl print "gui/$UID_NUM/$WDA_PLIST_LABEL" >/dev/null 2>&1; then
    info "Existing WDA launchd supervisor found; it can recover the device layer."
else
    info "Direct control plane will start offline; run setup-wda.sh to connect the device layer."
fi

info "Updating LaunchAgent (gui/$UID_NUM) ..."

# Evict any PRIOR-label daemon first. Before v0.2.0 the label/app/bundle-id were
# work.pwtk.iphone-remote / iPhoneRemote.app. A label change means the OLD
# LaunchAgent is NOT superseded by ours — it keeps respawning and squats the
# port, so the new daemon can't bind and the two race (flaky, served the wrong
# build). Boot out the exact launchd-owned label and disable its plist. Do not
# use a global process-name kill: it cannot prove UID, launchd ownership, or
# process start identity and could terminate an unrelated/custom executable.
#
# Moving the old plist is itself transactional. Preserve a pre-existing
# `.disabled` copy and keep both recovery sources until the new job is proven
# loaded; cleanup then puts every file and launchd state back exactly.
if [ -e "$OLD_PLIST_DISABLED" ] && [ ! -f "$OLD_PLIST_DISABLED" ]; then
    die "Legacy disabled-plist path is not a regular file: $OLD_PLIST_DISABLED"
fi
if [ -f "$OLD_PLIST" ]; then
    OLD_PLIST_REPLACED=1
    if [ -f "$OLD_PLIST_DISABLED" ]; then
        OLD_DISABLED_HAD_EXISTING=1
        OLD_DISABLED_BACKUP="$(mktemp "$HOME/Library/LaunchAgents/.${OLD_PLIST_LABEL}.disabled.backup.XXXXXX")"
        rm -f "$OLD_DISABLED_BACKUP"
        mv "$OLD_PLIST_DISABLED" "$OLD_DISABLED_BACKUP" \
            || die "Could not stage the existing disabled legacy plist."
    fi
    OLD_PLIST_STAGED="$(mktemp "$HOME/Library/LaunchAgents/.${OLD_PLIST_LABEL}.active.XXXXXX")"
    rm -f "$OLD_PLIST_STAGED"
    mv "$OLD_PLIST" "$OLD_PLIST_STAGED" \
        || die "Could not stage the active legacy LaunchAgent plist."
    mv "$OLD_PLIST_STAGED" "$OLD_PLIST_DISABLED" \
        || die "Could not disable the legacy LaunchAgent plist."
    OLD_PLIST_STAGED=""
fi

# A first formal install can be launched while the same product is already
# running manually from a checkout. launchctl cannot see or evict that process,
# so the new KeepAlive job otherwise crash-loops on the occupied port and the
# transaction rolls back with an opaque health-check failure. The installed
# binary's `stop` command validates the private pid record, uid, process start
# time, executable, and argv before signaling; it refuses legacy/unsafe records.
#
# Run this only when neither product LaunchAgent was loaded before the install:
# launchd-owned jobs are evicted below and must not be raced by a manual signal.
if [ "$PREINSTALL_DAEMON_LOADED" = "0" ] \
    && [ "$PREINSTALL_OLD_DAEMON_LOADED" = "0" ]; then
    MANUAL_STOP_RESULT=""
    if ! MANUAL_STOP_RESULT="$("$BINARY_PATH" stop 2>&1)"; then
        die "A non-launchd daemon may be occupying the product runtime, but its identity could not be verified safely: $MANUAL_STOP_RESULT"
    fi
    case "$MANUAL_STOP_RESULT" in
        *"sent SIGTERM to verified pid"*)
            PREINSTALL_MANUAL_DAEMON_STOPPED=1
            ok "Verified manual daemon stopped for LaunchAgent takeover"
            ;;
    esac
fi

DAEMON_RUNTIME_TOUCHED=1
if launchctl print "gui/$UID_NUM/$OLD_PLIST_LABEL" >/dev/null 2>&1; then
    warn "Evicting old daemon: $OLD_PLIST_LABEL (configuration already migrated)"
    launchctl bootout "gui/$UID_NUM/$OLD_PLIST_LABEL" 2>/dev/null || true
fi

# Unload OUR label if already running (idempotent). `bootout` is ASYNCHRONOUS —
# it returns before the service is fully torn down, so a bootstrap fired right
# after races the teardown and fails "Bootstrap failed: 5: Input/output error"
# (the exact failure an upgrade hit). Wait for the label to actually disappear.
launchctl bootout "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null || true
for _ in 1 2 3 4 5 6 7 8 9 10; do
    launchctl print "gui/$UID_NUM/$PLIST_LABEL" >/dev/null 2>&1 || break
    sleep 0.5
done

# Enable the job definition before bootstrapping it.
if launchctl enable "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null; then
    ok "LaunchAgent enabled (persists across reboots)"
else
    die "Could not enable the new LaunchAgent; the previous install and daemon state were restored."
fi

DAEMON_LOADED=0
if [ "$DAEMON_SHOULD_START" = "1" ]; then
    # Bootstrap from the new plist; retry once if it still raced the teardown.
    if ! launchctl bootstrap "gui/$UID_NUM" "$PLIST_DST" 2>/dev/null; then
        sleep 1
        launchctl bootout "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null || true
        sleep 1
        if ! launchctl bootstrap "gui/$UID_NUM" "$PLIST_DST" 2>/dev/null; then
            die "Could not bootstrap the new LaunchAgent after retry; the previous install and daemon state were restored."
        else
            DAEMON_LOADED=1
            ok "LaunchAgent bootstrapped (after retry)"
        fi
    else
        DAEMON_LOADED=1
        ok "LaunchAgent bootstrapped"
    fi

    if [ "$DAEMON_LOADED" = "1" ]; then
        if launchctl kickstart -k "gui/$UID_NUM/$PLIST_LABEL" 2>/dev/null \
            && launchctl print "gui/$UID_NUM/$PLIST_LABEL" >/dev/null 2>&1; then
            ok "LaunchAgent job is loaded"
        else
            DAEMON_LOADED=0
            die "The new LaunchAgent did not remain loaded; the previous install and daemon state were restored."
        fi
    fi
fi
[ "$DAEMON_LOADED" = "1" ] \
    || die "The new LaunchAgent was not loaded; the previous install and daemon state were restored."

# A loaded KeepAlive job can still be crash-looping or unable to bind its port.
# The browser control plane is designed to work while WDA is offline, so
# require that local HTTP endpoint before discarding the previous install.
case "$HOST" in
    0.0.0.0) DAEMON_PROBE_HOST="127.0.0.1" ;;
    "::"|"0:0:0:0:0:0:0:0") DAEMON_PROBE_HOST="[::1]" ;;
    *:*) DAEMON_PROBE_HOST="[$HOST]" ;;
    *) DAEMON_PROBE_HOST="$HOST" ;;
esac
DAEMON_PROBE_URL="http://${DAEMON_PROBE_HOST}:${PORT}/"
DAEMON_HTTP_READY=0
DAEMON_PID=""
if command -v curl >/dev/null 2>&1; then
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if probe_daemon_control_plane; then
            DAEMON_HTTP_READY=1
            break
        fi
        sleep 0.5
    done
fi
if [ "$DAEMON_HTTP_READY" != "1" ]; then
    die "The new daemon did not prove a stable owned PID, listener, and HTTP response at $DAEMON_PROBE_URL; the previous install and daemon state were restored. Inspect $LOG_DIR."
fi
ok "Daemon identity, listener, and HTTP control plane verified (pid $DAEMON_PID): $DAEMON_PROBE_URL"

# A newly-started daemon begins with a conservative cached WDA health
# value and refreshes it in the background. If the installer already proved the
# WDA endpoint itself, do not report a ready device layer while the product API
# still says `drivable:false` (the same cold-cache boundary setup-wda.sh gates).
DAEMON_PRODUCT_READY=0
if [ "$WDA_READY" = "1" ] \
    && [ "$DAEMON_HTTP_READY" = "1" ]; then
    DAEMON_AGENT_SECRET="$(plist_env_get PHONE_REMOTE_AGENT_TOKEN)"
    [ -n "$DAEMON_AGENT_SECRET" ] || DAEMON_AGENT_SECRET="$PASSWORD"
    DAEMON_STATUS_TRIES=0
    while [ "$DAEMON_STATUS_TRIES" -lt 30 ]; do
        DAEMON_STATUS_TRIES=$((DAEMON_STATUS_TRIES + 1))
        if [ -n "$DAEMON_AGENT_SECRET" ]; then
            DAEMON_STATUS="$(curl -sS --noproxy '*' -m 2 \
                -H "Authorization: Bearer $DAEMON_AGENT_SECRET" \
                "${DAEMON_PROBE_URL%/}/agent/status" 2>/dev/null || true)"
        else
            DAEMON_STATUS="$(curl -sS --noproxy '*' -m 2 \
                "${DAEMON_PROBE_URL%/}/agent/status" 2>/dev/null || true)"
        fi
        if printf '%s' "$DAEMON_STATUS" \
            | grep -Eq '"drivable"[[:space:]]*:[[:space:]]*true'; then
            DAEMON_PRODUCT_READY=1
            break
        fi
        sleep 0.5
    done
    DAEMON_STATUS=""
    DAEMON_AGENT_SECRET=""
    [ "$DAEMON_PRODUCT_READY" = "1" ] \
        || die "WDA answered before install, but the restarted daemon did not report drivable=true within 15s; the previous install and daemon state were restored. Inspect $LOG_DIR."
    ok "Daemon product status verified: drivable=true"
fi

# Commit only after launchd accepts the new definition and the local HTTP
# control plane answers. WDA may still be a first-run prerequisite, but a
# broken daemon bootstrap/control plane is an install failure and must never
# be reported as success.
INSTALL_COMMIT_SIGNAL_PENDING=0
trap 'INSTALL_COMMIT_SIGNAL_PENDING=1' HUP INT TERM
_verify_skill_before_daemon_commit
if _commit_daemon_transaction; then
    :
else
    commit_status=$?
    exit "$commit_status"
fi
[ -z "$APP_BACKUP" ] || rm -rf "$APP_BACKUP" 2>/dev/null || true
APP_BACKUP=""
[ -z "$PLIST_BACKUP" ] || rm -f "$PLIST_BACKUP" 2>/dev/null || true
PLIST_BACKUP=""
[ -z "$SETUP_WDA_BACKUP" ] || rm -f "$SETUP_WDA_BACKUP" 2>/dev/null || true
SETUP_WDA_BACKUP=""
[ -z "$UNINSTALL_BACKUP" ] || rm -f "$UNINSTALL_BACKUP" 2>/dev/null || true
UNINSTALL_BACKUP=""
_finish_runner_sources
[ -z "$OLD_DISABLED_BACKUP" ] || rm -f "$OLD_DISABLED_BACKUP" 2>/dev/null || true
OLD_DISABLED_BACKUP=""

# ── Step 8b — Command-line entrypoint (`iphone-use upgrade`) ──────────────────
echo ""
install_cli_link "$DEST/$BINARY_INSIDE_APP"

# ── Step 9 — First-run work ────────────────────────────────────────────────────
echo ""
printf '%b━━━ Device layer (iphone-use device runner) ━━━%b\n' "$BOLD" "$RESET"
echo ""
if [ "$WDA_MANAGED" = "true" ]; then
    printf "  Before managed setup:\n"
    printf "    • Install full Xcode and sign in: Xcode → Settings → Accounts.\n"
    printf "    • Enable Developer Mode on the iPhone; connect it over USB.\n"
    printf "    • Keep the iPhone unlocked and awake during the first build.\n"
    printf '    • Install the default USB loopback relay: %bbrew install libimobiledevice%b (iproxy).\n' "$BOLD" "$RESET"
    printf "    • Keep the Mac and iPhone on a trusted/isolated network: WDA itself has no authentication.\n"
    printf "    • Keep Cloudflare WARP / tunnel VPN manually disconnected while Xcode mounts developer services.\n"
    printf "    • WDA_ALLOW_LAN=1 + socat is an explicit unsafe fallback, not automatic recovery.\n"
    echo ""
    if [ -x "$SETUP_WDA_DST" ]; then
        printf "  1. Check prerequisites (read-only):\n"
        printf "       ${BOLD}%s doctor${RESET}\n" "$SETUP_WDA_DST"
        printf "  2. Build, sign, install, relay, and verify WDA:\n"
        printf "       ${BOLD}%s${RESET}\n" "$SETUP_WDA_DST"
    else
        warn "WDA setup script disappeared after commit; rerun install.sh to restore $SETUP_WDA_DST."
    fi
else
    info "Using an externally managed WDA endpoint:"
    printf "    control: %s\n" "$WDA_URL"
    printf "    video  : %s\n" "$WDA_MJPEG_URL"
    printf "  The installer will not start, stop, or rewrite that service.\n"
fi
echo ""
if [ "$WDA_READY" = "1" ]; then
    ok "WDA was already reachable during this install."
elif [ "$WDA_MANAGED" = "false" ]; then
    warn "The external WDA endpoint was not reachable; verify it independently."
else
    warn "The phone is not reported ready yet; completion is intentionally deferred to setup-wda.sh."
fi
printf "\n"
printf '%bNOTE (headless):%b LaunchAgents run only after a desktop login (Aqua session).\n' "$YELLOW" "$RESET"

# ── Step 9b — Companion agent skill ──────────────────────────────────────────
echo ""
printf '%b━━━ Agent skill ━━━%b\n' "$BOLD" "$RESET"
if [ "$SKILL_SYNCED" = "1" ]; then
    if [ "$SKILL_SYNC_KIND" = "bootstrap" ]; then
        ok "Release-matched agent skill was verified by the pinned bootstrap."
    else
        ok "Daemon and agent skill were installed from the same source: $SKILL_SYNC_REF ($SKILL_SYNC_COMMIT)"
        info "Verified skill SHA-256: $SKILL_SYNC_SHA256"
    fi
else
    warn "Agent skill synchronization was explicitly skipped."
    warn "Existing skill compatibility is unknown; rerun without IPHONE_USE_SKIP_SKILL=1 to synchronize it."
fi

echo ""
printf '%b━━━ MCP server ━━━%b\n' "$BOLD" "$RESET"
ok "Installed release-matched MCP executable:"
printf '  %s\n' "$DEST/$MCP_BINARY_INSIDE_APP"
info "Use this absolute path as the MCP client command; the bridge connects to the installed daemon."

echo ""
printf '%b━━━ Upgrade ━━━%b\n' "$BOLD" "$RESET"
info "Check: iphone-use upgrade --check    Upgrade: iphone-use upgrade"

# ── Step 10 — Print current status ───────────────────────────────────────────
if command -v curl >/dev/null 2>&1 \
    && curl -fsS --noproxy '*' -m 4 "${WDA_URL%/}/status" >/dev/null 2>&1; then
    WDA_READY=1
fi

echo ""
printf '%b━━━ Current LaunchAgent status ━━━%b\n' "$BOLD" "$RESET"
if launchctl print "gui/$UID_NUM/$PLIST_LABEL" >/dev/null 2>&1; then
    ok "Daemon job loaded: gui/$UID_NUM/$PLIST_LABEL"
else
    info "Daemon job is staged but not loaded."
fi
if [ "$WDA_MANAGED" = "false" ]; then
    info "WDA endpoint is externally managed; no local supervisor status is asserted."
elif launchctl print "gui/$UID_NUM/$WDA_PLIST_LABEL" >/dev/null 2>&1; then
    ok "WDA supervisor loaded: gui/$UID_NUM/$WDA_PLIST_LABEL"
else
    info "WDA supervisor is parked or not set up yet (it starts on the next agent request)."
fi

echo ""
if [ "$DAEMON_HTTP_READY" = "1" ]; then
    printf '%b━━━ Connect from your iPhone ━━━%b\n' "$BOLD" "$RESET"
    ok "Daemon HTTP endpoint verified at $DAEMON_PROBE_URL"
else
    printf '%b━━━ Connect after first-run setup ━━━%b\n' "$BOLD" "$RESET"
    if [ "$WDA_MANAGED" = "true" ]; then
        warn "Run setup-wda.sh first; the installer has not verified a usable daemon yet."
    else
        warn "Verify the external WDA endpoints and restart the daemon before connecting."
    fi
fi
printf "  1. Keep the iPhone and Mac on the same trusted Wi-Fi.\n"
printf "  2. In iPhone Safari open:  ${BOLD}http://%s:%s/phone${RESET}\n" "$LAN_IP" "$PORT"
printf "  3. Password: ${BOLD}%s${RESET}\n" "$PASSWORD"
if [ "$PW_SOURCE" = "generated" ]; then
    printf "     ${YELLOW}(generated — save it; it's stored in %s)${RESET}\n" "$PLIST_DST"
fi
printf "     Change it later by editing PHONE_REMOTE_PASSWORD in that plist + kickstart.\n"

echo ""
printf '%b━━━ Quick reference ━━━%b\n' "$BOLD" "$RESET"
printf "  Status  : launchctl print gui/%s/%s\n"       "$UID_NUM" "$PLIST_LABEL"
printf "  Restart : launchctl kickstart -k gui/%s/%s\n" "$UID_NUM" "$PLIST_LABEL"
printf "  Stop    : launchctl bootout gui/%s/%s\n"      "$UID_NUM" "$PLIST_LABEL"
printf "  Uninstall: %s\n" "$UNINSTALL_DST"
printf "  Logs    : tail -f %s/iphone-use.log\n"    "$LOG_DIR"
printf "  Errors  : tail -f %s/iphone-use.err\n"    "$LOG_DIR"
printf "  WDA     : %s status\n" "$SETUP_WDA_DST"
printf "  WDA log : tail -f %s/.iphone-use/wda-agent.log\n" "$HOME"
echo ""
if [ "$DAEMON_HTTP_READY" = "1" ] \
    && [ "$WDA_READY" = "1" ] \
    && [ "$DAEMON_PRODUCT_READY" = "1" ]; then
    ok "Installed; daemon HTTP, WDA endpoints, and product drivable status verified."
else
    ok "Installed; daemon HTTP control plane verified."
    warn "The device layer is pending: the device runner is parked, or still needs setup-wda.sh."
fi
