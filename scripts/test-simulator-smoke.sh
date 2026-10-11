#!/usr/bin/env bash
# End-to-end check of the simulator preview against real simctl/xcodebuild:
# `simulator start` (runner build, runner, daemon), then the daemon's
# simulator routes and an MCP test suite found through IPHONE_USE_INSTANCE.
#
# Needs a Mac with Xcode and an iOS simulator runtime. It boots a simulator,
# builds the runner unsigned and stops everything it started at the end.
#
#   scripts/test-simulator-smoke.sh                 # first iPhone simulator
#   SIM="iPhone 16 Pro" scripts/test-simulator-smoke.sh
#
# Binaries: target/release by default (IPU_BIN_DIR overrides). Runner
# sources: this checkout's runner/. CI runs it in simulator-smoke.yml.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${IPU_BIN_DIR:-$ROOT/target/release}"
INSTANCE="${IPU_SMOKE_INSTANCE:-ci-sim}"
export IPU_RUNNER_SRC="$ROOT/runner"
STATE="$HOME/.iphone-use/instances/$INSTANCE"

pass=0
ok_t() { pass=$((pass + 1)); printf 'ok %d - %s\n' "$pass" "$1"; }
bad()  { printf 'not ok - %s\n' "$1" >&2; exit 1; }
# json <python expression over `d`> < body
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }

[ -x "$BIN/iphone-use" ] && [ -x "$BIN/iphone-use-mcp" ] \
    || bad "build first: cargo build --release -p server -p iphone-use-mcp (or set IPU_BIN_DIR)"

SIM="${SIM:-$("$BIN/iphone-use" simulators --json \
    | json 'next((x["udid"] for x in d if x["name"].startswith("iPhone")), "")')}"
[ -n "$SIM" ] || bad "no iPhone simulator on this Mac"
echo "# simulator: $SIM"

# Shut the simulator down at the end only when this script booted it.
was_booted="$("$BIN/iphone-use" simulators --json \
    | json "any(x['udid'] == '$SIM' and x['state'] == 'Booted' for x in d)")"
cleanup() {
    local shutdown=--shutdown
    [ "$was_booted" = "True" ] && shutdown=
    "$BIN/iphone-use" simulator stop "$SIM" --instance "$INSTANCE" $shutdown >/dev/null 2>&1 || true
}
trap cleanup EXIT

"$BIN/iphone-use" simulator start "$SIM" --instance "$INSTANCE" \
    || { tail -40 "$STATE/simulator-runner.log" "$STATE/simulator-daemon.log" 2>/dev/null; bad "simulator start failed"; }
ok_t "simulator start built and started the runner and the daemon"

PORT="$(json 'd["daemon_port"]' <"$STATE/simulator.json")"
B="http://127.0.0.1:$PORT"
H=(-H "X-Phone-Control: 1" -H "X-Phone-Owner: smoke" -H "Content-Type: application/json")

status=""
for _ in $(seq 1 90); do
    status="$(curl -fsS -m 10 "$B/agent/status" || true)"
    [ "$(printf '%s' "$status" | json 'd.get("drivable")' 2>/dev/null)" = "True" ] && break
    sleep 2
done
[ "$(printf '%s' "$status" | json 'd.get("target")')" = "simulator" ] || bad "status target: $status"
[ "$(printf '%s' "$status" | json 'd.get("drivable")')" = "True" ] || bad "not drivable: $status"
ok_t "the daemon drives the simulator (target simulator, drivable)"

body="$(curl -fsS -m 60 "$B/agent/elements")"
[ "$(printf '%s' "$body" | json 'len(d.get("elements") or [])')" -gt 0 ] || bad "no elements: ${body:0:300}"
ok_t "the screen reads as elements"

body="$(curl -fsS -m 90 "$B/agent/apps?refresh=1")"
printf '%s' "$body" | grep -q 'com.apple.Preferences' || bad "Settings missing from the app list: ${body:0:300}"
ok_t "the app inventory comes from simctl"

body="$(curl -fsS -m 30 "${H[@]}" -X POST "$B/agent/location" -d '{"lat":37.3349,"lon":-122.009}')"
[ "$(printf '%s' "$body" | json 'd.get("ok")')" = "True" ] || bad "location: $body"
curl -fsS -m 30 "${H[@]}" -X POST "$B/agent/location" -d '{"clear":true}' >/dev/null || bad "clear location"
ok_t "simulated location set and cleared"

curl -fsS -m 30 "${H[@]}" -X POST "$B/agent/logs" -d '{"action":"start"}' >/dev/null || bad "start logs"
sleep 5
body="$(curl -fsS -m 30 "$B/agent/logs?limit=20")"
[ "$(printf '%s' "$body" | json 'd.get("ok")')" = "True" ] || bad "logs: ${body:0:300}"
ok_t "device logs stream from simctl"

body="$(curl -fsS -m 30 "${H[@]}" -X POST "$B/agent/record" -d '{"action":"start"}')"
[ "$(printf '%s' "$body" | json 'd["recording"]["via"]')" = "simctl" ] || bad "record start: $body"
sleep 4
body="$(curl -fsS -m 60 "${H[@]}" -X POST "$B/agent/record" -d '{"action":"stop"}')"
bytes="$(printf '%s' "$body" | json 'd["summary"]["bytes"]')" || bad "record stop: $body"
[ "$bytes" -gt 0 ] || bad "empty recording: $body"
ok_t "recording via simctl recordVideo saved $bytes bytes"

# Give the owner lease back, so the MCP client (another owner) may drive.
curl -fsS -m 30 "${H[@]}" -X POST "$B/agent/owner" -d '{"release":true}' >/dev/null || bad "release the owner lease"

suite="$(mktemp -d)/settings.yaml"
cat >"$suite" <<'YAML'
suite: Simulator smoke
app: com.apple.Preferences
cases:
  - name: Settings opens
    steps:
      - {kind: pause, ms: 1500}
    assert:
      - application: Settings
        timeout_ms: 15000
YAML
IPHONE_USE_URL= IPHONE_USE_INSTANCE="$INSTANCE" "$BIN/iphone-use-mcp" test "$suite" --junit "$(dirname "$suite")/report.xml" \
    || bad "the MCP suite failed (IPHONE_USE_INSTANCE=$INSTANCE)"
ok_t "an MCP test suite reaches the daemon through IPHONE_USE_INSTANCE=$INSTANCE"

"$BIN/iphone-use" simulator stop "$SIM" --instance "$INSTANCE" || bad "simulator stop"
[ ! -e "$STATE/simulator.json" ] || bad "stop left simulator.json behind"
ok_t "simulator stop stopped both processes"

printf '1..%d\n' "$pass"
