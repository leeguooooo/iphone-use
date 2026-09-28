#!/usr/bin/env bash
# install.sh links ~/.local/bin/iphone-use to the app executable so
# `iphone-use upgrade` is on PATH; uninstall.sh removes only that exact link.
# Both functions are extracted and run against a throwaway HOME: nothing here
# touches a real install.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-cli-link.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass=0
ok_t()  { pass=$((pass + 1)); printf 'ok %d - %s\n' "$pass" "$1"; }
bad()   { printf 'not ok - %s\n' "$1" >&2; exit 1; }

INSTALL_FN="$(sed -n '/^install_cli_link() {$/,/^}$/p' "$ROOT/install.sh")"
REMOVE_FN="$(sed -n '/^remove_cli_link() {$/,/^}$/p' "$ROOT/uninstall.sh")"
[ -n "$INSTALL_FN" ] || bad "install_cli_link not found in install.sh"
[ -n "$REMOVE_FN" ] || bad "remove_cli_link not found in uninstall.sh"

# Run one function in a clean shell with the scripts' own helper shapes.
run_install() {  # home target [PATH]
    HOME="$1" PATH="${3:-/usr/bin:/bin}" /bin/bash -euc "
        ok() { printf 'OK %s\n' \"\$*\"; }
        warn() { printf 'WARN %s\n' \"\$*\"; }
        info() { printf 'INFO %s\n' \"\$*\"; }
        $INSTALL_FN
        install_cli_link \"\$0\"
    " "$2"
}
run_remove() {  # home dry_run
    HOME="$1" /bin/bash -euc "
        ok() { printf 'OK %s\n' \"\$*\"; }
        warn() { printf 'WARN %s\n' \"\$*\"; }
        fail() { printf 'FAIL %s\n' \"\$*\"; }
        plan() { printf 'PLAN %s\n' \"\$*\"; }
        APP_BINARY=\"\$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use\"
        DRY_RUN=\"\$0\"
        $REMOVE_FN
        remove_cli_link
    " "$2"
}

new_home() {
    local home="$TMP/$1"
    mkdir -p "$home/Applications/iPhoneUse.app/Contents/MacOS"
    printf '#!/bin/sh\n' > "$home/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
    printf '%s\n' "$home"
}

# 1. Fresh install creates the link (and ~/.local/bin).
H="$(new_home fresh)"
T="$H/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
out="$(run_install "$H" "$T")"
[ "$(readlink "$H/.local/bin/iphone-use")" = "$T" ] || bad "fresh install did not link: $out"
printf '%s' "$out" | grep -q "not on PATH" || bad "missing PATH hint: $out"
ok_t "fresh install links ~/.local/bin/iphone-use and warns when it is not on PATH"

# 2. Re-running is a no-op and quiet about PATH when it is on PATH.
out="$(run_install "$H" "$T" "$H/.local/bin:/usr/bin:/bin")"
[ "$(readlink "$H/.local/bin/iphone-use")" = "$T" ] || bad "rerun changed the link"
printf '%s' "$out" | grep -q "not on PATH" && bad "PATH hint while on PATH: $out"
ok_t "rerun keeps the link"

# 3. A link to another iPhoneUse.app executable is ours to repoint.
H="$(new_home repoint)"
T="$H/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
mkdir -p "$H/.local/bin"
ln -s "/Old/Place/iPhoneUse.app/Contents/MacOS/iphone-use" "$H/.local/bin/iphone-use"
run_install "$H" "$T" >/dev/null
[ "$(readlink "$H/.local/bin/iphone-use")" = "$T" ] || bad "did not repoint an old iphone-use link"
ok_t "an older iPhoneUse.app link is repointed"

# 4. A foreign link or file is never replaced.
H="$(new_home foreign-link)"
T="$H/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
mkdir -p "$H/.local/bin"
ln -s /usr/bin/true "$H/.local/bin/iphone-use"
out="$(run_install "$H" "$T")"
[ "$(readlink "$H/.local/bin/iphone-use")" = "/usr/bin/true" ] || bad "replaced a foreign link"
printf '%s' "$out" | grep -q "Left" || bad "no warning for a foreign link: $out"
H="$(new_home foreign-file)"
T="$H/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
mkdir -p "$H/.local/bin"
printf 'mine\n' > "$H/.local/bin/iphone-use"
run_install "$H" "$T" >/dev/null
[ ! -L "$H/.local/bin/iphone-use" ] && grep -qx mine "$H/.local/bin/iphone-use" \
    || bad "replaced a foreign file"
ok_t "foreign links and files are left alone"

# 5. Uninstall: dry-run plans, real run removes only our link.
H="$(new_home uninstall)"
T="$H/Applications/iPhoneUse.app/Contents/MacOS/iphone-use"
run_install "$H" "$T" >/dev/null
out="$(run_remove "$H" 1)"
[ -L "$H/.local/bin/iphone-use" ] || bad "dry-run removed the link"
printf '%s' "$out" | grep -q "PLAN" || bad "dry-run did not plan: $out"
run_remove "$H" 0 >/dev/null
[ ! -e "$H/.local/bin/iphone-use" ] && [ ! -L "$H/.local/bin/iphone-use" ] \
    || bad "uninstall kept the link"
H="$(new_home uninstall-foreign)"
mkdir -p "$H/.local/bin"
ln -s /usr/bin/true "$H/.local/bin/iphone-use"
run_remove "$H" 0 >/dev/null
[ -L "$H/.local/bin/iphone-use" ] || bad "uninstall removed a foreign link"
ok_t "uninstall removes only the installer's link"

printf '1..%d\n' "$pass"
