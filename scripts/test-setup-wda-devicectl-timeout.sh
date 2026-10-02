#!/bin/bash
# _devicectl_t returns as soon as devicectl does, and still caps a hung call.
# Regression: its old `( sleep N; kill ) &` watchdog left the sleep running, so
# every call lasted the full timeout (8-10 s each, ~26 s per connect).
set -u
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
eval "$(sed -n '/^_devicectl_t() {/,/^}/p' "$here/setup-wda.sh")"
now() { python3 -c 'import time; print(time.time())'; }
elapsed() { python3 -c "import time; print(time.time() - $1)"; }
lt() { python3 -c "import sys; sys.exit(0 if $1 < $2 else 1)"; }
n=0; fail=0
check() { n=$((n + 1)); if "$@"; then echo "ok $n - $desc"; else echo "not ok $n - $desc"; fail=1; fi; }

printf '#!/bin/bash\necho fast-answer\n' > "$tmp/bin/xcrun"; chmod +x "$tmp/bin/xcrun"
t0="$(now)"; out="$(PATH="$tmp/bin:$PATH" _devicectl_t 8 list devices)"; took="$(elapsed "$t0")"
desc="a quick answer returns at once ($took s), not after the 8 s cap"; check lt "$took" 2
desc="the answer is passed through"; check [ "$out" = "fast-answer" ]

printf '#!/bin/bash\nsleep 30\necho late\n' > "$tmp/bin/xcrun"; chmod +x "$tmp/bin/xcrun"
t0="$(now)"; out="$(PATH="$tmp/bin:$PATH" _devicectl_t 1 list devices)"; took="$(elapsed "$t0")"
desc="a hung call is cut off near its 1 s cap ($took s)"; check lt "$took" 3
desc="a cut-off call yields no output"; check [ -z "$out" ]
echo "1..$n"
exit "$fail"
