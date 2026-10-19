#!/bin/bash
# Build the iphone-use native runner for a generic iOS device (build-for-testing) and print the
# produced .xctestrun path on the last line of stdout.
#
# Signing:
#   - IPU_RUNNER_ASC_KEY_PATH / IPU_RUNNER_ASC_KEY_ID / IPU_RUNNER_ASC_ISSUER_ID set (or readable from the runner
#     LaunchAgent plist) → automatic signing for team 6ZPXG4KVVS through the App Store Connect
#     API key (-allowProvisioningUpdates -authenticationKey*).
#   - otherwise, or with --unsigned / IPU_RUNNER_UNSIGNED=1 → CODE_SIGNING_ALLOWED=NO (compile check;
#     the products cannot be installed).
#
# Options / environment:
#   --unsigned                    force an unsigned compile-check build
#   --simulator                   build for the iOS Simulator instead (preview): destination
#                                 generic/platform=iOS Simulator, CODE_SIGNING_ALLOWED=NO (no Apple
#                                 ID), its own derived data (default runner/build/DerivedData-simulator).
#                                 Also IPU_RUNNER_SIMULATOR=1.
#   --device <udid>               (signed builds) build for that connected device and register it
#                                 in the team, so the provisioning profile includes it
#                                 (-destination id=<udid> -allowProvisioningDeviceRegistration).
#                                 Also IPU_RUNNER_DEVICE_ID=<udid>.
#   IPU_RUNNER_DERIVED_DATA=dir   derived data dir (default: runner/build/DerivedData)
#   IPU_RUNNER_TEAM_ID=team       signing team (default: 6ZPXG4KVVS)
#   XCODEBUILD=path               xcodebuild binary (default: xcodebuild)
#   IPU_EMBED_LIB=path            link the on-phone daemon (libipu_embed.a from
#                                 scripts/build-ios-embed.sh) into the runner; without it the
#                                 runner builds and serves as before, with no daemon inside
#
# Never prints the ASC key values. Never installs or launches anything on a device.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT="$SCRIPT_DIR/IPhoneUseRunner/IPhoneUseRunner.xcodeproj"
SCHEME="IPhoneUseRunner"
DERIVED_DATA="${IPU_RUNNER_DERIVED_DATA:-$SCRIPT_DIR/build/DerivedData}"
TEAM_ID="${IPU_RUNNER_TEAM_ID:-6ZPXG4KVVS}"
XCODEBUILD_BIN="${XCODEBUILD:-xcodebuild}"
IPU_RUNNER_PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.runner.plist"

unsigned="${IPU_RUNNER_UNSIGNED:-0}"
simulator="${IPU_RUNNER_SIMULATOR:-0}"
device="${IPU_RUNNER_DEVICE_ID:-}"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --unsigned) unsigned=1 ;;
        --device)
            [ "$#" -ge 2 ] || { echo "--device needs a UDID" >&2; exit 2; }
            device="$2"
            shift
            ;;
        --simulator) simulator=1 ;;
        -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
if [ -n "$device" ] && ! printf '%s\n' "$device" | LC_ALL=C grep -Eq '^[A-Za-z0-9-]+$'; then
    echo "invalid device UDID: $device" >&2
    exit 2
fi
destination="generic/platform=iOS"
if [ -n "$device" ] && [ "$unsigned" != "1" ]; then
    destination="id=$device"
fi
if [ "$simulator" = "1" ]; then
    # A simulator build needs no signing and keeps its own products, so it never replaces
    # (or is mistaken for) the device build.
    destination="generic/platform=iOS Simulator"
    DERIVED_DATA="${IPU_RUNNER_DERIVED_DATA:-$SCRIPT_DIR/build/DerivedData-simulator}"
    if [ -n "$device" ]; then
        echo "ipu-runner build: --device ignored for a simulator build" >&2
    fi
fi

_plist_env() {
    [ -f "$IPU_RUNNER_PLIST" ] || return 0
    plutil -extract "EnvironmentVariables.$1" raw -o - "$IPU_RUNNER_PLIST" 2>/dev/null || true
}

if [ "$unsigned" != "1" ] && [ "$simulator" != "1" ]; then
    : "${IPU_RUNNER_ASC_KEY_PATH:=$(_plist_env IPU_RUNNER_ASC_KEY_PATH)}"
    : "${IPU_RUNNER_ASC_KEY_ID:=$(_plist_env IPU_RUNNER_ASC_KEY_ID)}"
    : "${IPU_RUNNER_ASC_ISSUER_ID:=$(_plist_env IPU_RUNNER_ASC_ISSUER_ID)}"
fi

args=(
    -project "$PROJECT"
    -scheme "$SCHEME"
    -configuration Debug
    -destination "$destination"
    -derivedDataPath "$DERIVED_DATA"
)

if [ "$simulator" = "1" ]; then
    echo "ipu-runner build: iOS Simulator (CODE_SIGNING_ALLOWED=NO)" >&2
    args+=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="")
    mode=simulator
elif [ "$unsigned" != "1" ] && [ -n "${IPU_RUNNER_ASC_KEY_PATH:-}" ] && [ -n "${IPU_RUNNER_ASC_KEY_ID:-}" ] \
    && [ -n "${IPU_RUNNER_ASC_ISSUER_ID:-}" ]; then
    if [ ! -f "$IPU_RUNNER_ASC_KEY_PATH" ]; then
        echo "ipu-runner build: IPU_RUNNER_ASC_KEY_PATH does not point at a file" >&2
        exit 1
    fi
    echo "ipu-runner build: signed (team $TEAM_ID, App Store Connect API key)" >&2
    args+=(
        -allowProvisioningUpdates
        -authenticationKeyPath "$IPU_RUNNER_ASC_KEY_PATH"
        -authenticationKeyID "$IPU_RUNNER_ASC_KEY_ID"
        -authenticationKeyIssuerID "$IPU_RUNNER_ASC_ISSUER_ID"
        DEVELOPMENT_TEAM="$TEAM_ID"
        CODE_SIGN_STYLE=Automatic
    )
    if [ -n "$device" ]; then
        echo "ipu-runner build: registering device $device in the team's provisioning profile" >&2
        args+=(-allowProvisioningDeviceRegistration)
    fi
    mode=signed
else
    if [ -n "$device" ]; then
        echo "ipu-runner build: --device ignored, an unsigned build registers nothing" >&2
        args=("${args[@]/#id=$device/generic/platform=iOS}")
    fi
    echo "ipu-runner build: unsigned compile check (CODE_SIGNING_ALLOWED=NO)" >&2
    args+=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="")
    mode=unsigned
fi

if [ -n "${IPU_EMBED_LIB:-}" ]; then
    if [ ! -f "$IPU_EMBED_LIB" ]; then
        echo "ipu-runner build: IPU_EMBED_LIB does not point at a file: $IPU_EMBED_LIB" >&2
        exit 1
    fi
    echo "ipu-runner build: linking the on-phone daemon ($IPU_EMBED_LIB)" >&2
    # -force_load: nothing references the entry points at link time (RunnerDaemon finds them
    # with dlsym), so a plain link would drop them.
    args+=("OTHER_LDFLAGS=\$(inherited) -force_load $IPU_EMBED_LIB -framework Security -framework SystemConfiguration")
fi

log="$DERIVED_DATA/../build-$mode.log"
mkdir -p "$DERIVED_DATA"
started=$SECONDS
if ! "$XCODEBUILD_BIN" "${args[@]}" build-for-testing >"$log" 2>&1; then
    grep -E "error:|BUILD FAILED|\*\* TEST BUILD FAILED" "$log" | head -40 >&2 || true
    echo "ipu-runner build: FAILED ($mode) — full log: $log" >&2
    exit 1
fi
echo "ipu-runner build: succeeded ($mode) in $((SECONDS - started))s — log: $log" >&2

xctestrun="$(ls -t "$DERIVED_DATA"/Build/Products/*.xctestrun 2>/dev/null | head -1 || true)"
if [ -z "$xctestrun" ]; then
    echo "ipu-runner build: no .xctestrun produced under $DERIVED_DATA/Build/Products" >&2
    exit 1
fi
echo "$xctestrun"
