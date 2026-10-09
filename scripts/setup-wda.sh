#!/usr/bin/env bash
# scripts/setup-wda.sh — the entry point of the device-layer setup (the
# iphone-use device runner, runner/IPhoneUseRunner).
#
# The setup itself is `iphone-use setup-native <command>` (Rust,
# crates/server/src/setup): it builds the runner, installs and launches it on
# the iPhone, relays it to loopback, points the daemon at it, and — run by
# launchd with WDA_KEEPALIVE=1 — supervises it. This script stays the entry
# point because launchd's runner supervisor, the daemon (reconnect, idle
# release), the installers and existing installs all address the device
# layer through it, its name, its WDA_* environment variables, its launchd
# label and its status file. It only finds the iphone-use binary and hands
# the command over with the same environment.
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
#   WDA_RUNNER_ICON=... the runner's Home Screen icon: auto, none, or a .png/.icns
#   IPU_RUNNER_SRC=...  device runner sources (default: ~/.iphone-use/runner; a
#                       repo checkout uses its own runner/)
#   IPHONE_USE_XCODE=... this phone's own Xcode (`iphone-use setup --xcode`)
#   WDA_PORT=...        control relay port (default: 8100; named instances: derived)
#   MJPEG_PORT=...      video relay port (default: 9100; named instances: derived)
#   PHONE_REMOTE_INSTANCE=... which daemon/phone pair (default: default)
#   WDA_TRANSPORT=...   auto (default): off USB, set up and relaunch through the
#                       phone's encrypted CoreDevice Wi-Fi tunnel (USB preferred);
#                       usb: require the cable
#   WDA_ALLOW_LAN=1     also permit a plain, unauthenticated LAN socat relay to the
#                       phone's address (unsafe on untrusted networks; default off)
#   WDA_RUNNER_REBUILD=1 ignore the recorded runner product and build again
#   IPHONE_USE_SETUP_BIN=... the iphone-use binary to hand over to (default: the
#                       daemon's own, then ~/Applications, then /Applications)
#
# Requirements: Xcode (an Apple ID in Settings → Accounts, or WDA_ASC_* signing),
# the iPhone paired + Developer Mode on, and the iPhoneUse app (install.sh).
set -eu
umask 077

# A LaunchAgent starts with a bare PATH; the setup engine extends it the same
# way, this keeps `/usr/libexec` and Homebrew tools reachable for this script.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/sbin:/sbin:$PATH"

COMMAND="${1:-setup}"
case "$COMMAND" in
    setup|status|stop|pause|resume|doctor|instance-context) ;;
    *)
        printf 'unknown command: %s (use: setup|status|stop|pause|resume|doctor|instance-context)\n' "$COMMAND" >&2
        exit 1
        ;;
esac

# The installed copy under ~/.iphone-use/instances/<name>/ belongs to that
# instance and refuses to run as any other (#67).
SELF="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")"
INSTALLED=""
case "$SELF" in
    "$HOME/.iphone-use/instances/"*/setup-wda.sh)
        INSTALLED="${SELF#"$HOME/.iphone-use/instances/"}"
        INSTALLED="${INSTALLED%/setup-wda.sh}"
        ;;
    "$HOME/.iphone-use/setup-wda.sh") INSTALLED=default ;;
esac
REQUESTED="${PHONE_REMOTE_INSTANCE-}"
[ -n "$REQUESTED" ] || REQUESTED="${INSTALLED:-default}"
if [ -n "$INSTALLED" ] && [ "$INSTALLED" != "$REQUESTED" ] \
    && [ -z "${PHONE_REMOTE_STATE_DIR:-}" ]; then
    if [ "$REQUESTED" = default ]; then
        OTHER="$HOME/.iphone-use/setup-wda.sh"
    else
        OTHER="$HOME/.iphone-use/instances/$REQUESTED/setup-wda.sh"
    fi
    printf 'this setup-wda.sh belongs to instance "%s" but PHONE_REMOTE_INSTANCE is "%s"; run %s instead\n' \
        "$INSTALLED" "$REQUESTED" "$OTHER" >&2
    exit 2
fi
export PHONE_REMOTE_INSTANCE="$REQUESTED"

# The binary: the daemon this instance runs (a named instance has its own
# runtime copy), then the standard app locations. It must implement the
# command (`setup-native --supports`).
if [ "$REQUESTED" = default ]; then
    DAEMON_PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.plist"
else
    DAEMON_PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.$REQUESTED.plist"
fi
PROGRAM=""
if [ -f "$DAEMON_PLIST" ]; then
    PROGRAM="$(/usr/libexec/PlistBuddy -c 'Print :ProgramArguments:0' "$DAEMON_PLIST" 2>/dev/null || true)"
fi
for CANDIDATE in "${IPHONE_USE_SETUP_BIN:-}" "$PROGRAM" \
    "$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use" \
    "/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"; do
    [ -n "$CANDIDATE" ] && [ -x "$CANDIDATE" ] || continue
    "$CANDIDATE" setup-native --supports "$COMMAND" >/dev/null 2>&1 || continue
    IPHONE_USE_SETUP_SCRIPT="$SELF" exec "$CANDIDATE" setup-native "$COMMAND"
done

printf '%s\n' "The iPhoneUse app is missing, or older than this setup script (it needs \`iphone-use setup-native $COMMAND\`)." >&2
printf '%s\n' "Reinstall it: curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh" >&2
exit 1
