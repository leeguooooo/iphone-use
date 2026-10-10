# iphone-use reference

Details behind [SKILL.md](SKILL.md). Read the section you need, when you need it.

- [Phone states and recovery](#phone-states-and-recovery)
- [Setup blockers](#setup-blockers)
- [HTTP API](#http-api)
- [Actions catalogue](#actions-catalogue)
- [Reading results: settle, delta, wait_for](#reading-results-settle-delta-wait_for)
- [Long lists: collect and scroll_find](#long-lists-collect-and-scroll_find)
- [Gestures and controls](#gestures-and-controls)
- [When the person has to sign in](#when-the-person-has-to-sign-in)
- [Chat apps: multi-line messages](#chat-apps-multi-line-messages)
- [Getting files from the phone to the Mac](#getting-files-from-the-phone-to-the-mac)
- [MCP specifics](#mcp-specifics)
- [Task metrics, runs and advice](#task-metrics-runs-and-advice)
- [Flows: format, compat, saving, fixing](#flows-format-compat-saving-fixing)
- [Screens hidden from capture](#screens-hidden-from-capture)
- [Vision fallback](#vision-fallback)
- [Semantic intents](#semantic-intents)
- [Worked example: Apple Health export](#worked-example-apple-health-export)
- [Upgrade](#upgrade)
- [Filing issues](#filing-issues)

## Phone states and recovery

Status checks never take control. For initialization, health checks, or when no
unfinished user task needs the phone: report the state and stop. Do not
reconnect, hold, or poll screenshots/elements to keep it ready — idle release
is intentional, and every runner launch can make the operator type the
passcode.

`GET /agent/status` (MCP `phone_status`; MCP renames `wda_locked` → `locked`):

| `device_state` | Meaning | What to do |
|---|---|---|
| `ready` + `drivable:true` | Drivable | Proceed with the task. |
| `locked` | Phone is locked | Without a passcode the next `/agent/input` unlocks it first; with one, ask the operator to unlock it — only if the task needs the phone. While a session drives the phone it is kept from auto-locking (`PHONE_REMOTE_KEEP_AWAKE_SECS`). |
| `released` / `released:true` | Normal idle | Leave it unless the task needs the phone; then see *Reconnect* below. |
| `releasing` | Release in progress | Do not reconnect or hold. Wait, then reassess, only if the task still needs it. |
| reconnecting (`reconnecting:true`) | Bring-up running | If `setup_blocked_on` is set, follow `hint`; else report `setup_phase`/`setup_message` and poll. A first build after an Xcode update can take minutes. |
| `blocked` / `offline` | Something stands in the way | Read `hint` and `setup_blocked_on` (see [Setup blockers](#setup-blockers)); resolve it first. When the person has to act, relay `next_step` (`{zh,en}`, the same line the web page and iOS app show) instead of paraphrasing `hint`. |
| `degraded` | WDA answers but the last read/action did not complete (heavy page, stalled app) | **Not** a blocker; `setup_blocked_on` is empty. Wait `retry_after_secs` (3s) and read again; do not restart anything. |
| `released` + `human_handoff:true` | The operator handed the phone to a person | Input answers `409 phone_handed_to_human`. Ask the operator before sending `{"mode":"agent"}`. |

**Lock readiness.** `lock_readiness` says whether the phone will lock on its
own while nobody drives it, so an owner with many phones sees which ones get
stuck at the lock screen:

```json
"lock_readiness": {"passcode_protected": true, "auto_lock_secs": 30,
  "keep_awake": {"enabled": true, "supported": true, "active": false},
  "verdict": "will_lock_needs_person",
  "hint": {"zh": "这台手机设了锁屏密码，自动锁定 30 秒：…", "en": "This phone has a passcode and Auto-Lock 30 seconds: …"},
  "checked_at": 1791538375}
```

- `passcode_protected`: whether a passcode is set (lockdown `PasswordProtected`
  over USB or a Wi-Fi attachment, or the runner while it is up). Both sources
  only say "required right now", so `false` is recorded only from a locked
  phone; `null` until then (a phone that never locks may stay `null`).
- `auto_lock_secs`: the Auto-Lock setting in seconds, `"never"`, or `null`. The
  device runner reads it (no lockdown value carries it), so it is known once a
  runner from this release ran on the phone.
- `keep_awake`: whether keep-awake is configured (`enabled`), whether the runner
  has it (`supported`, `null` until asked) and whether it is holding the phone
  awake now (`active`).
- `verdict`: `ready` (Auto-Lock is Never), `will_lock_needs_person` (passcode
  set: after a pause a person must unlock it), `will_lock_auto_unlocks` (no
  passcode: the next `/agent/input` unlocks it) or `unknown`.
- `hint`: one line for a person, `{zh, en}`, naming the fix (Auto-Lock → Never).

The values are cached (refreshed when the runner comes up and every 5 minutes,
kept across daemon restarts), so a released phone shows its last reading.
Report a `will_lock_needs_person` hint to the owner; never ask for, store or
type a passcode.

**Which phone.** `device` names the phone this daemon drives, read from
lockdown (USB or a Wi-Fi attachment) and cached in the instance state dir, so
an unplugged phone keeps its last reading; `null` until a first read:

```json
"device": {"name": "Leo's iPhone", "model": "iPhone X", "product_type": "iPhone10,3", "ios": "16.5"}
```

`model` is the marketing name for `product_type`, or `product_type` itself
when the daemon does not know that identifier. Refreshed when the runner
comes up and every 30 minutes.

**Reconnect** only when the current task needs the phone, `recovery_owner` is
`daemon`, blockers are resolved, and no release/reconnect is in progress. Send
it once, then poll status until `drivable:true`:

```bash
curl -s -H "$AUTH" -H "$MUTATION" -H 'Content-Type: application/json' \
  -X POST "$HOST/agent/mode" -d '{"mode":"agent"}'      # MCP: phone_reconnect
```

The target is canonical: to switch devices, change `PHONE_REMOTE_UDID`, rerun
setup, and restart the daemon. Never pass a one-off UDID. `503
device_release_in_progress` (with `Retry-After`) means release already started.

**Owner lease.** Name yourself on every state-changing request with
`X-Phone-Owner: <session-name>` (MCP does it from `PHONE_REMOTE_OWNER`, else
`mcp-<pid>`). Status shows `owner` and `owner_lease_remaining_secs`. If `owner`
is someone else, do not drive: control calls answer `409 phone_owned`. Never
send `X-Phone-Owner-Takeover: 1` unless the user confirms the other session is
abandoned. Release yours when done: `POST /agent/owner {"release":true}` / MCP
`phone_release_owner`.

**Hold.** For a human-in-the-loop pause inside a task (operator types a PIN,
approves a prompt, fetches a code): `POST /agent/hold {"secs":600}` / MCP
`phone_hold(secs)` (max 14400; 0 clears). Clear it when the pause ends. A hold
prevents idle release; it does not start WDA or prove readiness. Never use it
to keep the phone ready without a task. Taking a hold extends your owner
lease to cover it, so no other session can drive the phone during the pause.
A live owner lease also prevents idle release; the idle window starts when you
release it.

**Pre-warm.** A released phone may already be coming back when you look:
`warming:true` means the daemon started the bring-up ahead of time (MCP does
this on startup and when status is read). Poll status until `drivable:true`
instead of calling reconnect. `POST /agent/prewarm {"reason":"status"}` asks
for it explicitly; the answer is `started` or `skipped` with a reason
(`no_recent_activity`, `locked`, `rate_limited`, …) and never takes the lease.

## Setup blockers

`setup_blocked_on` is one of `warp | proxy | not_connected | usb | trust | ddi | account |
automation_mode_disabled | automation_not_allowed | wifi_automation_refused | xcode_too_old | ios_too_old |
ddi_needs_reboot | legacy_needs_usb | locked | wda` (empty = known prerequisites passed;
KeepAlive keeps the last concrete blocker while it re-checks).

- **`warp`** (the #1 blocker): Cloudflare WARP or another VPN wedges the
  CoreDevice tunnel when its Split Tunnel exclusions omit `fe80::/10` or the
  device RSD range `fd00::/8`. If WARP is only needed for some destinations,
  prefer **Traffic only** mode with Split Tunnels **Include** limited to those
  IPs (avoids the Local proxy timeout that breaks long Git uploads). For
  full-tunnel WARP, add both IPv6 exclusions to the Zero Trust device profile.
  `warp-cli disconnect` is only a temporary workaround. `iphone-use doctor`
  tells the states apart.
- **`not_connected`**: the iPhone is not connected to this Mac at all (usbmuxd
  does not list it and CoreDevice reports it unavailable). Ask the operator to plug
  it in over USB (or join the same Wi-Fi) and unlock it. Nothing is rebuilt while it
  is away; the managed service reconnects on its own when it comes back. The relays
  are not the problem; do not reconnect or run doctor for this.
- **`proxy`**: an enabled macOS HTTP/HTTPS/SOCKS entry is malformed or points at
  a loopback port with no listener; start that proxy app or disable the stale entry.
- **`trust`**: a one-time "trust the Apple Development certificate" tap on the phone.
- **`automation_mode_disabled`**: the phone is unlocked but iOS has not enabled UI
  automation (runner log: "Timed out while enabling automation mode"). Ask the
  operator to turn on **Settings › Developer › Enable UI Automation** and accept
  any passcode / "Allow automation" prompt. KeepAlive retries; do not loop reconnect.
- **`xcode_too_old`**: the runner exited with code 74 (testmanagerd refused the
  IDE channel) and the phone runs a newer iOS than the selected Xcode SDK;
  `setup_message` names both versions. Only installing an Xcode that supports
  that iOS (a beta Xcode for a beta iOS) fixes it; a person can give just this
  phone that Xcode with `iphone-use setup --xcode <Xcode.app>`. KeepAlive waits
  15 minutes between attempts; do not reconnect.
- **`ios_too_old`**: this Mac cannot drive the phone's iOS yet. iOS 15 and 16
  are driven without an iOS update through the legacy path (below), which
  needs App Store Connect API-key signing (`WDA_ASC_KEY_PATH`, `WDA_ASC_KEY_ID`,
  `WDA_ASC_ISSUER_ID` of a paid developer account): the selected Xcode cannot
  see the phone, so only the API can register it and provision the runner.
  iOS 14 and older are below the runner's floor and need an iOS update.
  `setup_message` says which case it is. A cable, WARP or a retry does not
  help; KeepAlive waits 15 minutes between attempts; do not reconnect.
- **`ddi_needs_reboot`** (iOS 15/16): a stale Developer Disk Image is mounted;
  lockdownd answers every developer service with `InvalidService` even after
  setup remounted the right image. Ask a person to restart the iPhone once and
  keep it plugged in and unlocked; KeepAlive re-checks every few minutes.
- **`legacy_needs_usb`** (iOS 15/16): the runner of such a phone runs over USB
  only (see the legacy path below); ask a person to plug it in, unlocked. The
  managed service starts it as soon as usbmuxd lists the phone on USB.
- **`automation_not_allowed`**: the same code-74 refusal, but this Xcode supports
  the phone's iOS (over USB): the phone did not authorize the UI-automation
  session. A passcode / "Allow" prompt appears on the phone while the runner
  starts and times out after about 30 s; ask the operator to unlock it, check
  **Settings › Developer › Enable UI Automation**, and answer that prompt on the
  next attempt. KeepAlive retries quietly every 5 s to 1 min; do not loop reconnect.
- **`wifi_automation_refused`**: that refusal over Wi-Fi. The phone's iOS will
  not start the runner's UI-automation session over the network (seen on an
  iOS 27.2 beta with Enable UI Automation on and no prompt; any Xcode). Nothing
  on the phone fixes it. Ask the operator to plug the phone in by USB once,
  unlocked: the runner starts in about 20 s, and after the unplug it keeps
  working over Wi-Fi until it has to start again (phone restart, runner crash).
  While `wifi_start_refused` is true and `transport` is `wifi-tunnel`, idle
  release keeps the runner up instead of stopping it, since it could not be
  started again without the cable. KeepAlive waits 15 minutes between Wi-Fi
  attempts and retries at once on USB; do not reconnect.
- **`usb`**: the configured iPhone is neither on USB nor on its encrypted
  CoreDevice Wi-Fi tunnel (or `WDA_TRANSPORT=usb` requires the cable). Off the
  cable, setup and relaunch go through that tunnel on their own, with no flag;
  ask the operator to plug the phone in or keep it unlocked on the Mac's network.
- **`locked`**: unlock the phone. **`ddi` / `account` / `wda`**: follow `hint`.

### iOS 15 and 16 (the legacy path)

Xcode 26 and later cannot see an iOS 15/16 phone at all, so setup drives it
without Xcode's device support: it builds the runner for `generic/platform=iOS`,
swaps Xcode's iOS 17-only test host for a small host of its own
(`runner/IPhoneUseRunner/LegacyHost/main.m`), registers the phone and creates a
development profile through the App Store Connect API, downloads the pinned
Developer Disk Image for that iOS (doronz88/DeveloperDiskImage, sha256-checked,
cached in `~/.iphone-use/ddi/`), and mounts it (proving testmanagerd starts),
installs and launches the runner with `iphone-use-legacy-launch` (crates/legacy-launch,
on the MIT `idevice` crate). From there it is the usual runner on the usual ports
and relays. `GET /agent/status` adds `legacy_ios`: `{ios, wifi_ready, lan_ip,
launch_transport, unplug_ok}` (else `null`).

The runner lives as long as the launcher's testmanagerd session. Setup launches
over Wi-Fi (lockdown at the phone's LAN address, heartbeat held) whenever that
answers, so `unplug_ok: true` means the cable can be pulled and restarts also go
over Wi-Fi; `launch_transport: "usb"` means pulling the cable ends the runner.
This Wi-Fi start is what `WDA_TRANSPORT=auto` (the default) means for iOS 15/16,
which has no CoreDevice tunnel: lockdown's TLS session with this Mac's pair record,
not a plain LAN relay. `WDA_TRANSPORT=usb` turns it off (cable only). Controlling
the runner off the cable is a separate question: usbmuxd's Wi-Fi attachment does not
carry the runner's port (measured on an iPhone X, iOS 16.5: runner alive on its LAN
address, relay dead), so only a LAN relay reaches it. The runner refuses requests
not signed with its per-launch token, so setup enables that relay by default once the
runner has proved it (an unsigned `/status` gets 401); a runner older than request
signing still needs `WDA_ALLOW_LAN=1`. The LAN path is signed but not encrypted:
screen contents cross the Wi-Fi in the clear. `unplug_ok` is true for a Wi-Fi start
with that relay; otherwise keep the cable in. Idle release
works as usual; the next request starts the runner again in ~8–12 s.

Repeated bootstrap requests hide the real blocker — fix it, then reconnect once.

The native runner's 8100/9100 listeners refuse any request not signed with the
token setup generates at each runner launch (`<state dir>/runner-token`, 0600;
HMAC-SHA256 over method, target, body, timestamp and a one-time nonce, so a captured
request cannot be replayed or altered). Daemon bearer auth protects `/agent/*`.
The Mac's relays listen on loopback only
and reach the phone over USB or, off the cable, through CoreDevice's encrypted
Wi-Fi tunnel (`transport: "wifi-tunnel"`, the default `WDA_TRANSPORT=auto`;
`WDA_TRANSPORT=usb` requires the cable). A LAN relay to the phone's address
(`transport: "wifi"`) is used by default only for an iOS 15/16 runner that enforces
request signing; anything else (the socat relay, a runner older than request
signing) needs the explicit `WDA_ALLOW_LAN=1`. The LAN path is unencrypted: use it
only on a trusted network.

## HTTP API

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{ok, backend, device_state, screen_state, wda, wda_actionable, wda_locked, drivable, released, owner, hint, setup_blocked_on, setup_phase, setup_message, version, latest, update_available, lock_readiness, device, …}` — gate on `drivable` |
| `GET /agent/capabilities` | What this build supports + whether the phone is drivable now (`blocked_by`); touches nothing |
| `GET /agent/elements` | UI as text: `{snapshot, elements:[{kind,label,identifier?,rect,depth,value?,enabled?,visible?,accessible?,focused?,placeholder?}], ax_stats, alert?, registry?}`. `?since=<snapshot>` returns a `delta` `{added,changed,removed,unchanged}` (+ `app_changed`) instead of the full tree. With `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` rows also carry `actions`, `selected`, `min`/`max` |
| `GET /agent/screenshot` | Device PNG. `?max_side=1200` shrinks it (~0.9k image tokens instead of ~1.5k; MCP's default) and lets a current live frame answer while someone watches. `X-Capture-Redacted: 1` = wireframe of a protected screen ([below](#screens-hidden-from-capture)); `?raw=1` untouched |
| `POST /agent/input` | One action; `?return=delta` adds the settled change |
| `POST /agent/actions` | `{"steps":[…]}`: up to 24 `action` / `wait_for` / `pause` steps, validated first, stops at the first failure |
| `POST /agent/collect` | Read a list across pages: rows of `row_kind`, one swipe per page, deduplicated ([below](#long-lists-collect-and-scroll_find)) |
| `POST /agent/scroll_find` | Is this label on screen and tappable? Swipe at most `max_swipes` (default 1) to find it ([below](#long-lists-collect-and-scroll_find)) |
| `GET /agent/flow/draft` | The daemon's recorded trail as a flow v1 draft ([below](#saving-a-flow)) |
| `GET /agent/reference` | This document, as compiled into the running daemon (`text/markdown`) |
| `GET /agent/apps` | Installed apps `{device, apps:[{bundle,name,version,bundle_version,system,…}]}`; `?bundle=`; cached 10 min (`?refresh=1`). `503 apps_unavailable` = unknown, not "not installed" |
| `GET /agent/apps?query=<name>` | App lookup by name: `{candidates:[{name,bundle_id,source,installed_verified,match,publisher?}], installation_checked, warnings}`. `source=auto` (phone's app list → bundled catalog → App Store), `installed`, `catalog` (offline), `apple` (`country=cn`, rate limited). Exact matches hide partial ones. MCP `phone_apps` |
| `GET /agent/intents`, `POST /agent/intent` | Semantic intents ([below](#semantic-intents)) |
| `POST /agent/mode`, `/agent/hold`, `/agent/owner`, `/agent/prewarm` | Reconnect / hold / release lease / pre-warm ([above](#phone-states-and-recovery)) |

Every state-changing POST needs `X-Phone-Control: 1`; a 403 names the missing
header — fix the request once, do not repeat it. Give your client **at least
40s** on `/agent/elements`: the daemon retries a failed source read until its
own 35s deadline, so a shorter client timeout hands you an empty body that is
your timeout, not an empty screen. Five consecutive auth failures lock you out
for 30s.

## Actions catalogue

Coordinates are normalized `[0,1]` (`0,0` top-left).

```jsonc
{"type":"launch_app","bundle":"com.apple.Health"}       // or "name":"健康" for built-in apps
{"type":"launch_app","app":"微信"}                       // any app's exact name; 422 ambiguous_app|app_not_found|app_not_installed (+candidates), nothing sent
{"type":"tap","element":3,"snapshot":"…"}               // from the SAME elements read
{"type":"tap","label":"新备忘录"}                         // exact, unique; add "kind":"Button" if a StaticText shares it
{"type":"tap_locator","locator":{"identifier":"save"}}  // label/identifier/kind/value/focused/enabled/visible
{"type":"tap","x":0.5,"y":0.3}                          // last resort
{"type":"text","text":"Health"}                         // into the FOCUSED field; up to 20000 chars (see long text below)
{"type":"set_value","element":5,"snapshot":"…","value":"你好"}  // write a field directly; 409 value_not_applied → tap + text instead (web views)
{"type":"key","name":"return"}                          // return|enter|send|go|search fire Return; escape, space, tab, delete/backspace, arrows
{"type":"keyboard"}                                     // dismiss the keyboard
{"type":"shortcut","name":"home"}                       // home | spotlight (switcher is unsupported)
{"type":"scroll","x":0.5,"y":0.5,"dx":0,"dy":80}        // ≈15% of a screen; ≈400 → 75%
{"type":"scroll","element":7,"snapshot":"…","dy":120}   // inside that element only
{"type":"scroll","page":true,"dy":300}                  // the page scroller (long web forms); 422 no_page_scroller if none
{"type":"swipe","x1":0.5,"y1":0.8,"x2":0.5,"y2":0.2}    // also drag (+hold_ms), longpress (+duration_ms)
{"type":"back"}                                         // left-edge swipe, not a Back button
{"type":"alert","button":"允许"}                        // or "action":"accept"|"dismiss"
{"type":"picker","column":0,"value":"2026"}
{"type":"perform","element":9,"snapshot":"…","action":"toggle"}  // increment|decrement|adjust(+value)|toggle|menu|double_tap|two_finger_tap|scroll_to_visible|pinch|rotate
```

**Long text.** Text over 200 characters is typed in chunks of about 200 (never
splitting an emoji, flag, or combining mark), roughly 60 characters a second;
the request's deadline grows with the text. If typing stops part-way the answer
is `502 text_partially_typed` with `characters_confirmed` (Unicode scalar
values from the start), `characters_uncertain`, `remaining_text`, and
`retry_safe:false`: read the field, then send only what is missing with
`clear:false` — never the whole text again. One batch types at most 20000
characters in total.

`force_press` answers `422 force_press_unsupported` (retry-safe, nothing sent)
on every iPhone since XR/11; use `menu`. App uninstall: `{"type":"uninstall","bundle":…}`
(destructive; HTTP only).

## Jev: hand over a whole goal

`iphone-use-mcp jev run --goal "<goal>" [--app <bundle>] [--max-steps 30]`
(MCP `phone_jev_run`) is a phone agent: each step reads `/agent/elements`,
builds an indexed table of the visible controls (buttons, cells, keys, tabs,
switches, fields), and asks TypeSafe's Jev for the operation — CLICK,
TYPE_TEXT, SCROLL_DOWN/UP, BACK, PRESS_RETURN (only with a keyboard up), WAIT,
DONE, BLOCKED — and its target in one request. A small OpenAI-compatible model
writes a field's text only for TYPE_TEXT (entered with `set_value`, so a
Chinese keyboard cannot swallow it). Every step is an ordinary daemon action
with your owner lease, so the daemon's trail — and `phone_flow_draft` — works
on a Jev run too.

Keys: `TYPESAFE_API_KEY` or `~/.config/typesafe/key`; for typing
`TEXT_MODEL_API_KEY` or `~/.config/openrouter/key` (`TEXT_MODEL`, default
`inception/mercury-2.5`; `TEXT_MODEL_BASE_URL`). The report has `status`
(`done`, `blocked`, `max_steps`, `error`), `history` and a timing split
(`jev_ms`, `act_ms`, `observe_ms`, `text_ms`). Jev answers BLOCKED rather than
send, pay, delete or share what the goal did not ask for; still confirm such
goals with the user, and verify the end screen. The policy is adapted from
browser-use/jev-ultrafast (MIT) via chrome-use's `jev run`.

## Batches first

A task should take a few turns, not one per tap. `POST /agent/actions`
(`phone_run_steps`) runs up to 24 steps under one control lock and stops at the
first failure; `{"steps":[…], "observe":true}` (MCP: on by default) also
returns `snapshot`, `elements`, `settle` and `alert` for the screen the batch
ended on. Pattern: read once → batch what you can see → read the batch's own
observation → next batch. Three single `/agent/input` actions in a row earn a
one-shot `batch_hint`; a batch, or a pause over a minute, resets the count.

## When a read is blocked by an alert

A system alert over the app (paste permission, location, notifications) can
make every element-tree read fail. `/agent/elements` then answers `409
{"error":"alert_blocking","alert":{"text","buttons"}}` within about a second
instead of retrying for its 35 s budget, and a read that still times out or
fails carries the same `alert` block when one is up. Answer it with
`{"type":"alert","button":"<exact text>"}` — a permission is the user's call —
then read again.

Optional alerts: an alert step with `"if_present": true` (MCP/flow:
`{"kind":"alert","button":"允许粘贴","if_present":true}`) answers the alert when
one is up and passes as `skipped: "no_alert"` when none is — for prompts that
appear only sometimes. Flows that use it need a client that knows the field.

Controls that ignore element taps: some custom buttons acknowledge
XCUIElement's click and do nothing (hardware: Xiaohongshu's back button). Add
`"via":"point"` to `tap_locator` (`{"kind":"tap_locator","locator":{…},"via":"point"}`):
the locator is still proven unique, then the centre of the element's live
frame is tapped. Keep the default (`element`) otherwise — coordinate taps on
`/source` frames miss in system sheets.

## Reading results: settle, delta, wait_for

**`?return=delta`** (MCP act tools: on by default, `observe:false` to skip) settles after an applied action and
returns `{ok, snapshot, baseline, delta}` (full `elements` when the baseline
is no longer cached). The action result and the observation are separate facts:
`ok:true` stands even when the observation fails.

| `settle.reason` | Meaning | Do |
|---|---|---|
| `stable` | Two consecutive reads matched over a non-empty tree | Check your postcondition — stable is not "it worked" |
| `budget_exhausted` | Stability not confirmed in time (or `settle_ms` too short / 0) | Re-read with `GET /agent/elements` |
| `observation_failed` | The read broke (also legacy `delta_error`) | Action still applied; verify with a read |

- `sparse:true`: tree was empty or containers only — evidence you cannot see, not
  that nothing changed. `stale:true`: the tree is the last good read, not fresh.
- `no_visible_change:true`: a readable, settled tree with no row changed — the
  gesture hit nothing (touch landed on an input or nested scroller, target
  moved). Re-read and retarget; its absence does not prove success.
- `settle_ms` default 15000, max 20000; it is a ceiling, reading stops when two
  reads match. One large-tree read was measured at 6–7s.
- `app_changed:{from,to}`: the frontmost app changed — usually a banner swallowed
  your tap. Stop, `{"type":"home"}`, re-enter the intended app.
- Rows with `overlay` (`notification`, `dynamic_island`, `cover_sheet`) belong to
  a system layer: never tap them; wait and re-read.
- A failed `wait_for` carries `observation`: `read:false` proves nothing;
  `read:true, stale:true` is the last valid view; plain `read:true` means the
  condition really was not met. Locators an empty tree would have "satisfied" are
  listed in `absent_unproven` and do not pass. Never conclude something is gone
  from a tree with nothing in it — screenshot instead.
- A 502/504 after dispatch (`outcome_unknown`, `retry_safe:false`) may have
  acted. Read elements or a screenshot before deciding; never replay text,
  scroll, back, pay, send or delete blindly.
- A non-2xx read or an empty tree with `error` is a failed checkpoint even if
  the last status said `drivable:true`.

## Long lists: collect and scroll_find

`POST /agent/collect` (MCP `phone_collect_list`) reads a scrolling list in one
call: the rows of one kind on screen, a swipe of about 60% of the list (the
rest stays as overlap, so no row is skipped), the next read, and so on.

```jsonc
{"row_kind":"Cell",            // default; StaticText, Button, Link, …
 "region":[0,0.15,1,0.75],     // optional: rows centred here, swipes here
 "max_pages":6,                // 1–10
 "end_label":"没有更多了",       // optional, exact
 "direction":"down"}           // or "up"
```

A row without a label (most Cells) is named by its texts, joined with ` · `.
Rows are deduplicated on `(kind, label, value)`, so identical-looking rows
collapse into one. `stop_reason`:

| `stop_reason` | Meaning |
|---|---|
| `end_label` | The end label was on screen — the only case with `complete:true` |
| `duplicate_page` | After a swipe the page was the same: the list did not move (its end, or the swipe hit something that does not scroll) |
| `no_progress` | The list moved but showed nothing new |
| `max_pages` | Page budget used up; more rows may follow |
| `deadline` | Too little of the 80 s call budget was left to swipe and read another page |
| `read_failed` / `swipe_failed` | Stopped early; rows so far are returned, no snapshot |
| `no_rows` | No row of that kind on the first page: check `row_kind` or `region` |

`complete` is false in every case but `end_label`; `duplicate_page` and
`no_progress` are not proof the list ended. `snapshot` is the last page's
tree; rows from earlier pages are off screen and carry no index.

`POST /agent/scroll_find` (MCP `phone_scroll_find`) looks for one exact label:
`{"label":"…","kind":"Button","max_swipes":1,"direction":"down","region":[…]}`.
On screen and tappable: `found:true`, `target` (index, kind, frame) and a
`snapshot` to tap it by. A label in the tree but below or above the screen is
swiped toward. It stops AT ONCE, without swiping, when the label is ambiguous
(`candidates`), covered (`element_occluded`, `covered_by`) or not drawn
(`element_not_visible`). Not found after the budget: `element_not_found` with
on-screen labels containing the text as `candidates`. No screenshot is taken;
look at the screen before swiping further.

Both need `X-Phone-Control: 1`, take the owner lease, and leave the screen
where they stopped.

## Gestures and controls

- **System alerts** show up as `alert:{text,buttons}` on elements. A coordinate or
  element tap on an alert button is acknowledged but often does nothing — use the
  `alert` action (MCP: an `alert` step in `phone_run_steps`).
- **Switches & sliders** (hardware-verified): a coordinate tap on a Switch ACKs
  but does not flip it. Use `{"type":"perform","action":"toggle",…}`; sliders take
  `increment`/`decrement` (~10%) or `adjust` with `"value"` 0..1. MCP: a
  `phone_run_steps` step `{"kind":"perform","element":N,"snapshot":"…","action":"toggle"}`.
  Verify the new `value` from a fresh read.
- **Element taps** answer `409 element_occluded` (not sent) when something covers
  the centre — scroll it clear (`perform scroll_to_visible`) and re-read;
  `"allow_occluded":true` taps whatever is on top. Rows with `visible:false` are
  not drawn: `409 element_not_visible`; ignore them.
- **Scroll**: positive `dy` reveals content below. Travel is 1.5×|dy| points,
  clamped to 15–75% of the screen. Coordinate scrolls keep 44pt clear of the
  edges (an anchor at `x=0.06` got taken by the back-swipe). A gesture that must
  start at an edge belongs in `swipe`/`drag`. On long web forms prefer
  `"page":true` (travel capped at 60% of the page; repeat and verify).
- **Text** lands in whatever field has focus. Focus and verify first. Unicode
  (incl. CJK) is sent on-device; the Mac clipboard is untouched. After typing in
  a web form, send `{"type":"keyboard"}` before tapping buttons it covers. Submit
  chats/searches with `key return`: tapping a third-party keyboard's 发送/前往
  key ACKs without sending.
- **`back`** is a left-edge swipe; on a screen with no back target it can switch
  apps. Prefer tapping the on-screen back control.
- **App launch**: `launch_app` by bundle (or built-in name, or any app's exact
  name as `app`). Never guess a bundle id: look it up with `phone_apps` /
  `GET /agent/apps?query=`. Spotlight + typing is
  the slow path and breaks under the Chinese IME.
- **Do Not Disturb is automatic** when the intents registry lists the bridge's
  `focus_on` / `focus_off` verbs. The session's first action runs `focus_on`
  (Shortcuts flashes ~5 s, then the previous app comes back); releasing the
  phone (`phone_release_owner`), idling out or handing it to a person runs
  `focus_off`. The response that did it carries `agent_focus` — tell the user:
  - `requested_on` / `requested_off`: DND on for your session / off again; the
    phone shows a notice each time.
  - `left_as_is`: a Focus was already on; it is left alone, also on release.
  - `waiting_for_permission`, with the action refused as
    `focus_permission_pending` (`not_sent`, retry-safe): a one-time iOS prompt
    for the bridge (allow notifications) is on screen. It is not in the element
    tree — screenshot, tap **Allow / 允许**, then send the action again.
  Banners otherwise hijack taps (hardware-seen: a chat banner opened WeChat; a
  recurring alert banner broke flow runs). Opt out with `PHONE_REMOTE_AUTO_FOCUS=0`.

## Logging in from the password vault

When an app shows its login page during a task the user asked for, call
`phone_login` (HTTP `POST /agent/login`, CLI `iphone-use auth login --bwu`). The
daemon matches the app to an entry in the user's own vault (bitwarden-use on the
Mac: an `iosapp://<bundle>` URI, then a web host the bundle id implies, then the
app's name), reads it, types the account and password into the fields and taps
Log in. Nothing in the answer, the logs or later element reads carries a value:
the entry is named, the account masked (`le***@qq.com`), and password fields
always read `••••••••`.

| Answer | Meaning / what to do |
|---|---|
| `ok:true`, `filled`, `submitted` | Read the screen to confirm. `login_form_still_visible:true` → an error on screen (wrong password, captcha): tell the user. |
| `needs_code: {via}` | The app sent a code: call `phone_login` with `code_via` `sms`/`mail` and `code_from` (the sender) once it arrived (`POST /agent/login/code`). At most two code requests per login. An authenticator code from the entry is entered automatically. |
| `ambiguous_vault_entry` + `candidates` | Ask the user which entry; pass its name as `item` (and `user` when names repeat). |
| `no_vault_entry` | Ask the user which entry to use. Never guess. |
| `vault_locked` | Ask the user to unlock the vault on the Mac (`bwu unlock`). |
| `not_a_login_form` | Two password fields: sign-up or a password change. Not done automatically. |
| `value_not_applied` | The account field rejected the typed text (a composing keyboard): ask the user to switch the phone's keyboard to English. |
| `value_landed_elsewhere` | Typing went into another field; that field was cleared at once. Finish this login by hand with the user. |

Existing-account login only: never sign-up, password changes or payment, and a
push approval on another device needs the user.

## When the person has to sign in

Face ID / Touch ID, an app passcode or PIN, a one-time code with no source
`phone_login` can read, a login `phone_login` could not finish, or a payment
confirmation: these belong to the person.

1. Stop sending input. Take `phone_hold(secs)` so the phone stays ready while
   they act.
2. Ask with the host's question tool (for example `AskUserQuestion` in Claude
   Code, `request_user_input` in Codex), not a line in your reply. Name the app, what the screen
   asks for and what you will do next; offer "Done, continue" and "Can't right
   now". Fall back to plain text when the host has no such tool, or has one
   that is unavailable or refused right now (Codex allows `request_user_input`
   only in some modes); then wait for the person's reply.
3. Never ask for the password, PIN or code, and never type one with
   `phone_type` or a batch, even when the field is not marked secure.
4. After "Done": clear the hold, read the screen again (`phone_elements`) and
   plan from what is there. The person may have moved on; old snapshots,
   element ids and coordinates are stale. If something was submitted during
   the hand-off, read the result before resending anything.

## Chat apps: multi-line messages

In WeChat and most chat apps Return sends. A `\n` inside `type` text is typed
as Return, so each line goes out as its own message. Default to one line: join
the lines with spaces or ` / `. When the user really wants line breaks, type
the first line, tap inside the input box to bring up the edit menu and pick
换行 / New Line where the app offers it, type the next line, repeat; read the
whole draft back, then send once. No such menu item: stop and ask whether one
line is acceptable. Never press Return to get a line break.

## Getting files from the phone to the Mac

AirDrop is the shortest path for a photo, video or document the task needs
on the Mac: in the app, Share → AirDrop → this Mac (its name:
`scutil --get ComputerName`). Accept the prompt on the Mac if one appears
(that is the person's screen; ask them). The file lands in `~/Downloads`; pick
the newest match (`ls -t ~/Downloads | head`) and use its absolute path. For
large exports (Health), Save to Files → iCloud Drive also works; see the
worked example below.

## MCP specifics

31 tools (one, `phone_screen_frame`, is hidden from the model and used only by the live screen panel): `phone_status`, `phone_screen`, `phone_capabilities`, `phone_reconnect`, `phone_hold`,
`phone_release_owner`, `phone_login`, `phone_apps`, `phone_screenshot`, `phone_elements`, `phone_tap`,
`phone_tap_element`, `phone_tap_label`, `phone_scroll`, `phone_scroll_find`, `phone_collect_list`, `phone_type`,
`phone_key`, `phone_shortcut`, `phone_run_steps`, `phone_jev_run`,
`phone_run_start`, `phone_run_end`, and `phone_flow_list / info / run / draft /
update / publish / report`.

- The text block is compact and meant for the model. It carries the settled
  change, one line per element, or a batch's verdict and end screen; for a
  failed batch, the failed step, its error, `retry_safe` and the end screen.
  `phone_flow_list` gives one row per flow (`detail=true` for every field).
- `structuredContent` is slim by default: the top-level verdict fields plus
  `summary`. `IPHONE_USE_MCP_STRUCTURED=full` sends the whole JSON; `off`
  sends none. Claude Code shows the model only `structuredContent` on success;
  Codex CLI shows both parts.
- `phone_status` and `phone_capabilities` return the JSON itself;
  `phone_screenshot` returns an image.
- Single-step act tools observe by default (= `?return=delta`, baseline = the
  last snapshot this client saw, so only the change comes back); `observe:false`
  skips it. The settled screen is captured in memory, not sent: the next
  `phone_screenshot` / `GET /agent/screenshot` returns it without a capture
  (`X-Screenshot-Source: settled-after-action`; `?fresh=1` forces one). It is
  dropped by the next action and never written to disk.
- Decide by `retry_safe`, not `outcome`: `outcome:"unknown"` with
  `retry_safe:false` may have reached the phone. `not_sent` on one step never
  makes a whole batch replayable.
- Not in MCP: `set_value`, `keyboard`, element/page `scroll`, uninstall,
  intents. `launch_app`, `alert`, `picker`, `perform`, `swipe`, `drag`,
  `longpress`, `back` exist only as `phone_run_steps` step kinds.

## Task metrics, runs and advice

**Metrics.** `GET /agent/metrics[?owner=NAME]` (CLI `iphone-use metrics
[--owner NAME] [--json]`) reports open runs, recent closed runs and loss
counters. Live summaries in `open` carry `closed:"open"`; `recent` and the
JSONL log retain the actual close reason (`ended`, `idle`, `expired`,
`lifetime_exceeded`, or `evicted`). The CLI prints that state on each run.
A run counts HTTP calls to the daemon — **not model turns** —
plus batches, observed actions, flow calls, stale and unknown outcomes,
failure classes, and p50/p95 call time over `stored_calls`.
`runner_busy_union_ms` counts overlapping runner time once;
`runner_summed_ms` is the plain sum. Only authenticated calls count;
`/agent/status`, `/agent/metrics` and `/agent/run` never do. A call that was
cancelled, dropped or rejected makes the run `incomplete`. Closed runs are
appended to `agent-runs.jsonl` in the state directory.

**Runs.** Without an explicit run, runs are inferred per `X-Phone-Owner` from
idle gaps of more than 2 minutes. To count one task exactly, use
`POST /agent/run {"action":"start","run_id":"…"}` (MCP `phone_run_start`), send
`X-Agent-Run: <id>` on its calls (the MCP client does this for you), and end
with `{"action":"end","run_id":"…"}` (`phone_run_end`). `model_round_trips` is
reported only if the start declared `"complete_trace":true` and the end hands
over every model turn id, including turns that made no tool call; `[]` means
the task used no model turns. Otherwise it is `null`.

**`no_progress` advice.** When the same observed tap or scroll, on the same
screen, settles twice in a row with an identical accessibility tree and no
app switch, the answer carries `no_progress`. It is advice: nothing was resent.
Re-read and change approach. It never fires while a spinner is on screen,
across overlapping calls, or for targets it cannot name: relative scrolls and
locators.

**Scoped reads.** `GET /agent/elements?scope=…` filters the same fresh read.
The `snapshot` and every row's original `index` stay valid for taps.
- `app`: the active app's subtree, plus the keyboard. The top-level `alert`
  block is kept.
- `interactive`: controls only.
- `focused`: the focused field's container.
- `changed`: needs `since=<snapshot>`. A baseline the daemon no longer holds
  answers 400 `baseline_unavailable`; it never falls back to a full tree.

**Images only when needed.** `?image=auto` attaches a screenshot only when the
tree is unusable (no interactive rows, containers only). The screenshot is
always a fresh capture taken after the read. It is labelled with
`requested_at_ms`, `received_at_ms`, its `source` and its own
`capture_redacted`, never presented as the same instant as the tree. It is
sized to keep the whole answer under about 3.5 MB; otherwise the answer says
`image_omitted` and the text stays. MCP `phone_elements` reads with
`image=auto` and returns that same image once as image content, with its
labels in the text and no base64 in the text or the structured copy.

## Flows: format, compat, saving, fixing

The [official registry](https://github.com/leeguooooo/iphone-use-flows) is
mirrored to `~/.iphone-use/flows`. The daemon fetches it when it is missing or
over 24h old (opt out: `IPHONE_USE_FLOWS_NO_AUTO_UPDATE=1`); `flow update` /
`phone_flow_update` forces it.

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow list [--app com.apple.Health] [--category health]
"$MCP" flow info health/export-all
"$MCP" flow run health/export-all            # or a file: flow run ./draft.json --input text1=…
"$MCP" flow draft --out ./draft.json         # the daemon's recorded trail, validated
"$MCP" flow validate ./draft.json            # offline
"$MCP" flow add ./draft.json --as notes/new  # keep a private flow; survives update
"$MCP" flow publish ./draft.json --as notes/new --alias 备忘录 --note "iPhone 17 Pro Max, iOS 26"
"$MCP" flow report health/export-all --note "profile button is now '资料'"
"$MCP" flow apps                             # installed app versions used for compat
```

### The `registry` and `flow_suggestion` blocks

- `registry` `{key, app?, bundle?, flows:[{id,name,risk,verified,inputs?,locale?,compat?}], next}`
  rides on `launch_app` responses (single or batch) and on the first
  `/agent/elements` read in a newly entered app — not on every read. Flows are
  sorted read-only first. `installed:0` means the store is not on this Mac yet.
  MCP adds `compat` per flow.
- `flow_suggestion` `{key, steps, message}` appears once per task after ≥5
  applied steps in one app that has no flow, at most once per app per 14 days
  (opt out: `IPHONE_USE_FLOWS_NO_SUGGEST=1`). Flow runs never count.

### Compat

| compat | Do |
|---|---|
| `verified` | Run it. |
| `untested-newer` | App updated since verification: run, take one checkpoint screenshot, and if it worked offer to publish the new `verified_on`. |
| `incompatible` / `broken` | `flow run` refuses without `--force`/`force=true`: do it by hand, then offer to publish the fix. |
| `needs-verification` | Nightly re-verification failed: treat as broken. |
| `draft` | No hardware record: run with a checkpoint, then offer to publish `verified_on`. |
| `unknown` | No version data: behave as `untested-newer`. |

Labels are locale-specific (`locale`): an `en` flow fails closed on a Chinese
phone. Pick the matching variant or record one (`health/export-all-zh-cn`).
`risk: side_effect` (send / publish / pay / delete) refuses without
`--confirm` / `confirm=true`; confirm only after the user approved the exact
target and inputs.

### Flow v1 format

```json
{
  "version": 1,
  "name": "New note",
  "description": "Open Notes and start a note with the given text.",
  "app": "com.apple.mobilenotes",
  "category": "productivity",
  "risk": "navigation",
  "locale": "zh-CN",
  "inputs": { "body": { "type": "string", "description": "note text" } },
  "verified_on": [{ "device": "iPhone 17 Pro Max", "ios": "26", "app_version": "26.0", "date": "2026-10-06" }],
  "steps": [
    { "kind": "launch_app", "bundle": "com.apple.mobilenotes" },
    { "kind": "wait_for", "expect": { "application": "备忘录" }, "timeout_ms": 15000 },
    { "kind": "tap_locator", "locator": { "label": "新建备忘录", "kind": "Button" } },
    { "kind": "wait_for", "expect": { "present": [{ "kind": "TextView" }] } },
    { "kind": "type", "input": "body" }
  ]
}
```

Top-level fields are closed (unknown fields are rejected): `version, name,
description, inputs, steps, app, category, risk, locale, tags, verified_on,
app_version_min, example_inputs`. Step kinds: `tap, longpress, swipe, drag,
tap_label, tap_locator, type, key, shortcut, scroll, launch_app, back, alert,
picker, wait_for, pause`. `wait_for.expect` takes `application`, `present[]`,
`absent[]`. Only a `type` step may reference an input (`"input":"name"`), and
every declared input must be used. New fields wait for a client release before
they can enter the registry.

### Outputs: flows that return data

A flow can return what it read: `outputs` names values taken off the final
screen once every step passed. `flow run` / `phone_flow_run` put them in
`outputs` (top level in MCP); a value that could not be read is `null` and is
listed in `missing_outputs`.

```json
"outputs": {
  "steps":  {"locator": {"identifier": "StepCount"}, "type": "number"},
  "title":  {"locator": {"kind": "NavigationBar"}, "field": "label"},
  "chats":  {"locator": {"kind": "Cell"}, "field": "label", "all": true},
  "km":     {"label_contains": "公里", "type": "number"}
}
```

`locator` matches like `tap_locator`; `label_contains` narrows by substring;
`field` is `value` or `label` (default: value when present); `type: number`
takes the first number in the text (`8,532 步` → 8532); `all` returns every
match. End the flow with a `wait_for` on the screen the outputs live on.

**Verify.** `iphone-use-mcp flow verify <flow> --write-fixture` records the
SHAPE of a good result (types only, under `~/.iphone-use/flow-fixtures/`);
later `flow verify <flow>` (or `phone_flow_run verify=true`) fails when an
output went missing, changed type, or came back an empty list — the app
changed and the flow reads the wrong thing even though every step passed.

**Locale fallback.** A `read_only` / `navigation` registry flow that fails on
a missing element runs its other-language variant once (`health/export-all`
↔ `health/export-all-zh-cn`), inputs carried over by name; the result says
`fallback_from`. Flows that send, pay or delete never re-run.

### Saving a flow

When a response carries `flow_suggestion`, or you finish a multi-step task no
flow covers, **ask the user** whether to keep it. Only if they agree:

1. `phone_flow_draft(save_as="./x.json")` / `flow draft --out ./x.json` /
   `GET /agent/flow/draft`. Snapshot taps became locators; typed text became
   `text1`, `text2`… inputs (the text itself is never recorded). The response
   says `source: current|previous` and lists `todo`.
2. Work the `todo`: real name, description, input names; a `wait_for` after
   every screen change; `risk` / `category` / `locale`; replace coordinate taps
   with locators; strip personal data (contact names, amounts) from labels.
3. `flow validate`, then run the file once (`phone_flow_run(id="./x.json")` /
   `flow run ./x.json`) and add `verified_on`.
4. Ask again before publishing — it opens a public PR with the user's GitHub
   login: `phone_flow_publish(source, id, aliases, confirm=true)`. `aliases` =
   the app's foreground label in each language (`Health`, `健康`) so the
   `registry` block finds it. Unverified files open as draft PRs. Private
   tasks: `flow add` instead.

Authoring rules: prefer an accessibility identifier, then unique kind + label,
over coordinates; never persist snapshot indexes, WDA element ids or snapshot
tokens; wait for states, not fixed sleeps (`pause`/`after_ms` cap at 3s); never
make passwords, codes, private content, or payment/send/publish/delete targets
into inputs. The browser's **流程** panel can also record v1 JSON.

### Fixing a flow

A flow that stops (`failed_step`, `element_not_found`, `missing_present`) is a
registry bug until proven otherwise; `result.diagnosis` shows candidates.
Never replay it blindly — the first tap may have acted. Read elements to see
where it stopped. If the flow is wrong (label changed, app updated, locale),
tell the user and, with their OK, `phone_flow_report(id, note, confirm=true)`
(the last failure is already captured, redacted). If you can fix it, publish
the corrected file. Do not report a phone that was merely locked, offline, or
on the wrong app.

## Screens hidden from capture

Payment and bank apps (PayPay, wallets) mark content as protected; every
capture path gets a blank area. `GET /agent/screenshot` detects it and returns
`X-Capture-Redacted: 1` with a **wireframe** of the accessibility tree (blue
buttons/cells, green inputs, purple images, gray text) under a yellow banner.
Drive these screens by elements, never by coordinates guessed from a white
image, and never take the vision fallback on them. Blank in `?raw=1` with no
labelled elements = genuinely empty or loading: wait and read again.

## Vision fallback

AX first; vision only when the tree is unusable.

- **Mode A — tree too sparse** (games, canvas). Judge `ax_stats`
  `{n, n_interactive, labeled_frac, coverage, container_only, max_depth, visibility?}`:
  `n_interactive == 0 && container_only` → vision; `n_interactive < 3` or
  `labeled_frac < 0.3` or (`coverage < 0.3` and not `container_only`) → hybrid;
  otherwise stay AX.
- **Mode B — reading the tree kills the runner** (KakaoTalk, #44): elements
  returns 502 `wda_source_failed` / 504 `wda_source_timeout` twice in the same app
  while screenshots work. Remember that per app for the session. Huge trees
  (WeChat's chat list: 3,400 nodes) no longer cause this: above 1,000 nodes the
  daemon reads without WDA's per-node `isVisible` (~5 s instead of a runner
  kill) and says so with `ax_stats.visibility: "geometric"`. On such a tree
  `visible:false` only marks rows outside the screen; a row hidden inside it is
  not flagged, so read rects with care before tapping by element.

AX-free loop: screenshot → pick a target yourself → coordinate `tap` /
`longpress` / `swipe` / `scroll` → verify with a screenshot after ~300–800 ms.
Below ~0.5 confidence send nothing (re-screenshot, crop, scroll, then report).
In Mode B: **no `?return=delta`, no element/label taps, no element `wait_for`** —
each one reads the tree and takes the runner down. An applied action is
dispatched, not achieved; one adjusted retry, then stop. Destructive targets
keep their explicit-verification rules. Once a vision sequence works, save it
as a flow.

## Semantic intents

When `GET /agent/intents` (check once per session) lists a verb for the task,
`POST /agent/intent {"name":"battery","args":{}}` runs the bridge shortcut and
returns an `id`; the result lands on `/agent/inbox` (GET to peek,
`POST /agent/inbox/drain` to consume). An empty list is normal. The Shortcuts
app foregrounds during the run — never interleave with a UI flow. The first
run of each verb needs a one-time permission tap on the phone. `not_sent` is
retry-safe; `unknown` (`intent_timeout` / `intent_dispatch_failed`) means check
the inbox before re-sending anything with side effects.

## Worked example: Apple Health export

Look for it first: `phone_flow_list(app="com.apple.Health")` /
`flow run health/export-all` (or the `-zh-cn` variant on a Chinese phone). By
hand it is: `launch_app com.apple.Health` → avatar (top right) → scroll to the
bottom → "Export All Health Data" → "Export" → wait ~60s for the share sheet →
Save to Files → iCloud Drive → Save. On the Mac the zip appears at
`~/Library/Mobile Documents/com~apple~CloudDocs/导出.zip` or `Export.zip`
(`brctl download <path>` forces it); stream-parse
`apple_health_export/export.xml` (it can be hundreds of MB).

## Upgrade

`/agent/status` reports `version`, `latest`, `update_available`. When an update
exists (or a command prints `iphone-use X is available`), tell the user once per
session and offer `iphone-use upgrade` (`--check` / `--json` to look only). Only
upgrade when asked: it restarts the daemon and ends any phone session. If the
skill and the live API disagree (404, missing field), the skill is probably
stale — `iphone-use upgrade` or the installer
(`curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh`)
installs daemon and skill from the same release. Plugin installs:
`claude plugin update iphone-use@leeguooooo-plugins`.

## Filing issues

- A registry flow failed → [Fixing a flow](#fixing-a-flow). A flow you wish
  existed: `gh issue create -R leeguooooo/iphone-use-flows -l new-flow -t "flow request: <app> — <task>"`.
- iphone-use itself is broken, misleading or needlessly slow → tell the user and,
  with their OK:

```bash
gh issue create -R leeguooooo/iphone-use -t "agent feedback: <one-line symptom>" -b "$(cat <<'EOF'
**What I was doing**: <task, 1-2 lines>
**What happened**: <exact error/output>
**Expected**: <what would have been better>
**Env**: daemon <version>, device_state <state>, <macOS/iOS if known>
**Repro**: <exact calls>

*filed by an AI agent via the iphone-use skill, with user consent*
EOF
)"
```
