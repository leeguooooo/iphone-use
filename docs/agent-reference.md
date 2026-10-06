# iphone-use reference

Details behind [SKILL.md](SKILL.md). Read the section you need, when you need it.

- [Phone states and recovery](#phone-states-and-recovery)
- [Setup blockers](#setup-blockers)
- [HTTP API](#http-api)
- [Actions catalogue](#actions-catalogue)
- [Reading results: settle, delta, wait_for](#reading-results-settle-delta-wait_for)
- [Gestures and controls](#gestures-and-controls)
- [MCP specifics](#mcp-specifics)
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
| `locked` | Phone is locked | Ask the operator to unlock it and keep it awake — only if the task needs the phone. |
| `released` / `released:true` | Normal idle | Leave it unless the task needs the phone; then see *Reconnect* below. |
| `releasing` | Release in progress | Do not reconnect or hold. Wait, then reassess, only if the task still needs it. |
| reconnecting (`reconnecting:true`) | Bring-up running | If `setup_blocked_on` is set, follow `hint`; else report `setup_phase`/`setup_message` and poll. A first build after an Xcode update can take minutes. |
| `blocked` / `offline` | Something stands in the way | Read `hint` and `setup_blocked_on` (see [Setup blockers](#setup-blockers)); resolve it first. |
| `degraded` | WDA answers but the last read/action did not complete (heavy page, stalled app) | **Not** a blocker; `setup_blocked_on` is empty. Wait `retry_after_secs` (3s) and read again; do not restart anything. |
| `released` + `human_handoff:true` | The operator handed the phone to a person | Input answers `409 phone_handed_to_human`. Ask the operator before sending `{"mode":"agent"}`. |

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
to keep the phone ready without a task.

## Setup blockers

`setup_blocked_on` is one of `warp | proxy | usb | trust | ddi | account |
automation_mode_disabled | locked | wda` (empty = known prerequisites passed;
KeepAlive keeps the last concrete blocker while it re-checks).

- **`warp`** (the #1 blocker): Cloudflare WARP or another VPN wedges the
  CoreDevice tunnel when its Split Tunnel exclusions omit `fe80::/10` or the
  device RSD range `fd00::/8`. If WARP is only needed for some destinations,
  prefer **Traffic only** mode with Split Tunnels **Include** limited to those
  IPs (avoids the Local proxy timeout that breaks long Git uploads). For
  full-tunnel WARP, add both IPv6 exclusions to the Zero Trust device profile.
  `warp-cli disconnect` is only a temporary workaround. `setup-wda.sh doctor`
  tells the states apart.
- **`proxy`**: an enabled macOS HTTP/HTTPS/SOCKS entry is malformed or points at
  a loopback port with no listener; start that proxy app or disable the stale entry.
- **`trust`**: a one-time "trust the Apple Development certificate" tap on the phone.
- **`automation_mode_disabled`**: the phone is unlocked but iOS has not enabled UI
  automation (runner log: "Timed out while enabling automation mode"). Ask the
  operator to turn on **Settings › Developer › Enable UI Automation** and accept
  any passcode / "Allow automation" prompt. KeepAlive retries; do not loop reconnect.
- **`locked`**: unlock the phone. **`usb` / `ddi` / `account` / `wda`**: follow `hint`.

Repeated bootstrap requests hide the real blocker — fix it, then reconnect once.

WDA itself has no authentication. Daemon bearer auth protects `/agent/*`, not
the phone's own 8100/9100 listeners. Use Direct only on a trusted network;
when practical, turn off iPhone Wi-Fi and keep the relays on USB loopback.

## HTTP API

| Call | Purpose |
|---|---|
| `GET /agent/status` | `{ok, backend, device_state, screen_state, wda, wda_actionable, wda_locked, drivable, released, owner, hint, setup_blocked_on, setup_phase, setup_message, version, latest, update_available, …}` — gate on `drivable` |
| `GET /agent/capabilities` | What this build supports + whether the phone is drivable now (`blocked_by`); touches nothing |
| `GET /agent/elements` | UI as text: `{snapshot, elements:[{kind,label,identifier?,rect,depth,value?,enabled?,visible?,accessible?,focused?,placeholder?}], ax_stats, alert?, registry?}`. `?since=<snapshot>` returns a `delta` `{added,changed,removed,unchanged}` (+ `app_changed`) instead of the full tree. With `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` rows also carry `actions`, `selected`, `min`/`max` |
| `GET /agent/screenshot` | Device PNG. `?max_side=1200` shrinks it (~0.9k image tokens instead of ~1.5k; MCP's default) and lets a current live frame answer while someone watches. `X-Capture-Redacted: 1` = wireframe of a protected screen ([below](#screens-hidden-from-capture)); `?raw=1` untouched |
| `POST /agent/input` | One action; `?return=delta` adds the settled change |
| `POST /agent/actions` | `{"steps":[…]}`: up to 24 `action` / `wait_for` / `pause` steps, validated first, stops at the first failure |
| `GET /agent/flow/draft` | The daemon's recorded trail as a flow v1 draft ([below](#saving-a-flow)) |
| `GET /agent/reference` | This document, as compiled into the running daemon (`text/markdown`) |
| `GET /agent/apps` | Installed apps `{device, apps:[{bundle,name,version,bundle_version,system,…}]}`; `?bundle=`; cached 10 min (`?refresh=1`). `503 apps_unavailable` = unknown, not "not installed" |
| `GET /agent/intents`, `POST /agent/intent` | Semantic intents ([below](#semantic-intents)) |
| `POST /agent/mode`, `/agent/hold`, `/agent/owner` | Reconnect / hold / release lease ([above](#phone-states-and-recovery)) |

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
{"type":"tap","element":3,"snapshot":"…"}               // from the SAME elements read
{"type":"tap","label":"新备忘录"}                         // exact, unique; add "kind":"Button" if a StaticText shares it
{"type":"tap_locator","locator":{"identifier":"save"}}  // label/identifier/kind/value/focused/enabled/visible
{"type":"tap","x":0.5,"y":0.3}                          // last resort
{"type":"text","text":"Health"}                         // into the FOCUSED field
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

`force_press` answers `422 force_press_unsupported` (retry-safe, nothing sent)
on every iPhone since XR/11; use `menu`. App uninstall: `{"type":"uninstall","bundle":…}`
(destructive; HTTP only).

## Reading results: settle, delta, wait_for

**`?return=delta`** (MCP `observe:true`) settles after an applied action and
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

## Gestures and controls

- **System alerts** show up as `alert:{text,buttons}` on elements. A coordinate or
  element tap on an alert button is acknowledged but often does nothing — use the
  `alert` action (MCP: an `alert` step in `phone_run_steps`).
- **Switches & sliders** (hardware-verified): a coordinate tap on a Switch ACKs
  but does not flip it. Use `{"type":"perform","action":"toggle",…}`; sliders take
  `increment`/`decrement` (~10%) or `adjust` with `"value"` 0..1. HTTP only — MCP
  has no `perform`. Verify the new `value` from a fresh read.
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
- **App launch**: `launch_app` by bundle (or built-in name). Spotlight + typing is
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

## MCP specifics

22 tools: `phone_status`, `phone_capabilities`, `phone_reconnect`, `phone_hold`,
`phone_release_owner`, `phone_screenshot`, `phone_elements`, `phone_tap`,
`phone_tap_element`, `phone_tap_label`, `phone_scroll`, `phone_type`,
`phone_key`, `phone_shortcut`, `phone_run_steps`, and `phone_flow_list / info /
run / draft / update / publish / report`.

- Act tools and `phone_capabilities` return JSON in `structuredContent`; the text
  block is a preview trimmed at 8 KiB. `phone_run_steps`, `phone_elements` and
  `phone_flow_*` return complete JSON as text; `phone_screenshot` an image.
- Single-step act tools take `observe` (= `?return=delta`).
- Decide by `retry_safe`, not `outcome`: `outcome:"unknown"` with
  `retry_safe:false` may have reached the phone. `not_sent` on one step never
  makes a whole batch replayable.
- Not in MCP: `perform`, `set_value`, `keyboard`, element/page `scroll`,
  uninstall, intents. `launch_app`, `alert`, `picker`, `swipe`, `drag`,
  `longpress`, `back` exist only as `phone_run_steps` step kinds.

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
  `{n, n_interactive, labeled_frac, coverage, container_only, max_depth}`:
  `n_interactive == 0 && container_only` → vision; `n_interactive < 3` or
  `labeled_frac < 0.3` or (`coverage < 0.3` and not `container_only`) → hybrid;
  otherwise stay AX.
- **Mode B — reading the tree kills the runner** (KakaoTalk, #44): elements
  returns 502 `wda_source_failed` / 504 `wda_source_timeout` twice in the same app
  while screenshots work. Remember that per app for the session.

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
