# Usage telemetry

**Status: off.** This build ships no project token, so the daemon sends
nothing. The code below exists so that counting can be switched on later in a
release that says so; until then it is inert.

## What would be counted

When a token is configured, the Mac daemon counts its own agent API calls.
Each event carries only:

| Field | Values |
|---|---|
| `endpoint` | one of a fixed list: `/agent/input`, `/agent/actions`, `/agent/elements`, `/agent/screenshot`, `/agent/collect`, `/agent/scroll_find`, `/agent/mode`, `/agent/hold`, `/agent/owner`, `/agent/prewarm`, `/agent/login`, `/agent/login/code`, `/agent/apps`, `/agent/intents`, `/agent/intent`, `/agent/capabilities`, `/agent/flow/draft`, `/agent/reference` |
| `outcome` | `ok`, `error`, `unknown` |
| `error_code` | `none`, `element_not_found`, `element_not_visible`, `element_occluded`, `ambiguous_element_label`, `value_not_applied`, `expectation_timeout`, `phone_owned`, `not_drivable`, `transport`, `stale_element_snapshot`, `outcome_unknown`; anything else is `other` |
| `duration_ms` | the call's duration, capped at one hour |
| `via` | `flow` (a saved flow run) or `direct` |
| `app_version`, `platform`, `arch` | the daemon's version, `macos`, `aarch64`/`x86_64` |
| `session_id` | random, new each time the daemon starts |
| `distinct_id` | a random install id (see below) |

There is also one `iphone_use_daemon_started` event per daemon start, with the
common fields only.

Never sent: any text typed or read, element labels or values, screenshots or
video, app bundle ids, phone UDIDs or names, owner or session names, request
bodies, query strings, file paths, error messages. Status polls
(`/agent/status`), video streams, the browser UI, schedules and refused
(401/403/429) requests are not counted at all. Events go to PostHog with
person profiles and GeoIP disabled.

The install id is a random UUID in `telemetry-id` in the daemon's state
directory (mode 0600). It names an install, not a person or a phone; delete
the file and the next start makes a new one. A disabled daemon never creates
it.

## Turning it off

Telemetry stays off unless a token is present. With a token, either of these
turns it off completely (no id file, no background task):

```sh
IPHONE_USE_TELEMETRY=0     # also: false, off, no
DO_NOT_TRACK=1             # also: true, yes
```

The daemon runs as a LaunchAgent, so the variable goes into its plist and the
daemon is restarted:

```sh
plist=~/Library/LaunchAgents/com.leeguoo.iphone-use.plist
/usr/libexec/PlistBuddy -c "Add :EnvironmentVariables:IPHONE_USE_TELEMETRY string 0" "$plist"
launchctl bootout gui/$(id -u) "$plist"; launchctl bootstrap gui/$(id -u) "$plist"
```

## Turning it on (maintainers)

The token is read from `IPHONE_USE_TELEMETRY_TOKEN`, else from
`BUILT_IN_TOKEN` in `crates/server/src/telemetry.rs` (empty). An empty value
means off. Shipping a non-empty built-in token is a product decision: update
this page and `docs/privacy.md` in the same release.

## Delivery

Events wait in a bounded in-memory queue (256; a full queue drops new events)
and leave in batches of up to 20, about five seconds after the first one, from
a background task. Each request has a three-second timeout and one retry on a
network error, HTTP 429 or 5xx. Nothing is written to disk for later. A phone
action never waits on telemetry, and a delivery failure never changes a
response. This is usage counting, not an audit trail: events can be lost.
