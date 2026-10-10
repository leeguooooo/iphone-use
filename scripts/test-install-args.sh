#!/usr/bin/env bash
# install.sh must refuse malformed options before it touches anything.
#
# Regression: a caller looping over option strings under zsh
# (`for a in "" "--instance ix --udid X"; do curl … | sh -s -- $a; done`)
# passes "--instance ix --udid X" as ONE argument, because zsh does not
# word-split unquoted $a. install.sh took it for an app path and the curl|sh
# bootstrap died with "A local app path is accepted only when install.sh itself
# is run from a local file", which read like a flaky upgrade. Each case runs
# the installer the way curl|sh does (script on stdin) against a throwaway
# HOME and asserts exit 2, the message, and that nothing was created.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/iphone-use-install-args.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

pass=0
ok_t() { pass=$((pass + 1)); printf 'ok %d - %s\n' "$pass" "$1"; }
bad()  { printf 'not ok - %s\n' "$1" >&2; exit 1; }

# run <shell> <args...>: prints "<exit>|<stderr>", HOME is an empty directory.
run() {
    local shell="$1" home code
    shift
    home="$(mktemp -d "$TMP/home.XXXXXX")"
    home="$(cd -P "$home" && pwd -P)"
    code=0
    HOME="$home" PATH="/usr/bin:/bin" "$shell" -s -- "$@" \
        <"$ROOT/install.sh" >"$TMP/out" 2>"$TMP/err" || code=$?
    [ -z "$(ls -A "$home")" ] || bad "$shell $*: created files in HOME: $(ls -A "$home")"
    printf '%s|%s' "$code" "$(cat "$TMP/err")"
}

for shell in /bin/sh /bin/bash; do
    r="$(run "$shell" '--instance ix --udid 63f53bbb05918cbf4154ba9d1d1f95b28e532597' --no-setup)"
    [ "${r%%|*}" = 2 ] || bad "$shell: joined options exit ${r%%|*}, want 2: $r"
    printf '%s' "$r" | grep -q "arrived as one argument" \
        || bad "$shell: joined options message missing: $r"
    printf '%s' "$r" | grep -q 'zsh does not word-split' \
        || bad "$shell: zsh hint missing: $r"
    ok_t "$shell: options joined into one argument are refused before any change"

    r="$(run "$shell" --bogus)"
    [ "${r%%|*}" = 2 ] || bad "$shell: unknown option exit ${r%%|*}, want 2: $r"
    printf '%s' "$r" | grep -q "unknown option '--bogus'" \
        || bad "$shell: unknown option message missing: $r"
    ok_t "$shell: an unknown option is refused before any change"

    r="$(run "$shell" --instance)"
    [ "${r%%|*}" = 2 ] || bad "$shell: --instance without value exit ${r%%|*}, want 2: $r"
    ok_t "$shell: --instance without a value is still refused"

    r="$(run "$shell" --instance --no-setup)"
    [ "${r%%|*}" = 2 ] || bad "$shell: --instance --no-setup exit ${r%%|*}, want 2: $r"
    printf '%s' "$r" | grep -q "requires a value, got the option '--no-setup'" \
        || bad "$shell: option-as-value message missing: $r"
    ok_t "$shell: an option is never consumed as another option's value"
done

printf '1..%d\n' "$pass"
