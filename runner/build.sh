#!/bin/bash
# Build the iphone-use native runner for a generic iOS device (build-for-testing) and print the
# produced .xctestrun path on the last line of stdout.
#
# Signing:
#   - WDA_ASC_KEY_PATH / WDA_ASC_KEY_ID / WDA_ASC_ISSUER_ID set (or readable from the WDA
#     LaunchAgent plist) → automatic signing for team 6ZPXG4KVVS through the App Store Connect
#     API key (-allowProvisioningUpdates -authenticationKey*).
#   - otherwise, or with --unsigned / IPU_RUNNER_UNSIGNED=1 → CODE_SIGNING_ALLOWED=NO (compile check;
#     the products cannot be installed).
#
# Options / environment:
#   --unsigned                    force an unsigned compile-check build
#   IPU_RUNNER_DERIVED_DATA=dir   derived data dir (default: runner/build/DerivedData)
#   IPU_RUNNER_TEAM_ID=team       signing team (default: 6ZPXG4KVVS)
#   XCODEBUILD=path               xcodebuild binary (default: xcodebuild)
#
# Never prints the ASC key values. Does not install anything or touch a device.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT="$SCRIPT_DIR/IPhoneUseRunner/IPhoneUseRunner.xcodeproj"
SCHEME="IPhoneUseRunner"
DERIVED_DATA="${IPU_RUNNER_DERIVED_DATA:-$SCRIPT_DIR/build/DerivedData}"
TEAM_ID="${IPU_RUNNER_TEAM_ID:-6ZPXG4KVVS}"
XCODEBUILD_BIN="${XCODEBUILD:-xcodebuild}"
WDA_PLIST="$HOME/Library/LaunchAgents/com.leeguoo.iphone-use.wda.plist"

unsigned="${IPU_RUNNER_UNSIGNED:-0}"
for argument in "$@"; do
    case "$argument" in
        --unsigned) unsigned=1 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument: $argument" >&2; exit 2 ;;
    esac
done

_plist_env() {
    [ -f "$WDA_PLIST" ] || return 0
    plutil -extract "EnvironmentVariables.$1" raw -o - "$WDA_PLIST" 2>/dev/null || true
}

if [ "$unsigned" != "1" ]; then
    : "${WDA_ASC_KEY_PATH:=$(_plist_env WDA_ASC_KEY_PATH)}"
    : "${WDA_ASC_KEY_ID:=$(_plist_env WDA_ASC_KEY_ID)}"
    : "${WDA_ASC_ISSUER_ID:=$(_plist_env WDA_ASC_ISSUER_ID)}"
fi

args=(
    -project "$PROJECT"
    -scheme "$SCHEME"
    -configuration Debug
    -destination "generic/platform=iOS"
    -derivedDataPath "$DERIVED_DATA"
)

if [ "$unsigned" != "1" ] && [ -n "${WDA_ASC_KEY_PATH:-}" ] && [ -n "${WDA_ASC_KEY_ID:-}" ] \
    && [ -n "${WDA_ASC_ISSUER_ID:-}" ]; then
    if [ ! -f "$WDA_ASC_KEY_PATH" ]; then
        echo "ipu-runner build: WDA_ASC_KEY_PATH does not point at a file" >&2
        exit 1
    fi
    echo "ipu-runner build: signed (team $TEAM_ID, App Store Connect API key)" >&2
    args+=(
        -allowProvisioningUpdates
        -authenticationKeyPath "$WDA_ASC_KEY_PATH"
        -authenticationKeyID "$WDA_ASC_KEY_ID"
        -authenticationKeyIssuerID "$WDA_ASC_ISSUER_ID"
        DEVELOPMENT_TEAM="$TEAM_ID"
        CODE_SIGN_STYLE=Automatic
    )
    mode=signed
else
    echo "ipu-runner build: unsigned compile check (CODE_SIGNING_ALLOWED=NO)" >&2
    args+=(CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY="")
    mode=unsigned
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
