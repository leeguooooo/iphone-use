---
name: iphone-use
description: Use when a task needs a real iPhone — operating iOS apps that have no API (Apple Health, banking, IM apps), exporting on-phone data, tapping/typing/scrolling on the phone, or taking phone screenshots. Replays saved per-app flows from the iphone-use-flows registry in one call and offers to save new ones after multi-step tasks. Drives the iphone-use daemon's Direct/WDA HTTP agent API (or its MCP server).
---

# iphone-use — drive a real iPhone

The [iphone-use](https://github.com/leeguooooo/iphone-use) daemon drives a
physical iPhone over WebDriverAgent. It never touches the Mac's screen or
cursor. Use the HTTP API below or the bundled MCP server (`phone_*` tools); the
loop is the same. Details live in the reference: `curl -s -H "$AUTH"
"$HOST/agent/reference"` serves the copy that matches the running daemon
(sections below are named by heading; also
[docs/agent-reference.md](https://github.com/leeguooooo/iphone-use/blob/main/docs/agent-reference.md)).

```bash
HOST="${PHONE_REMOTE_URL:-http://127.0.0.1:44321}"
AUTH="Authorization: Bearer $PHONE_REMOTE_TOKEN"   # daemon password or PHONE_REMOTE_AGENT_TOKEN
MUTATION="X-Phone-Control: 1"                      # required on every state-changing POST
OWNER="X-Phone-Owner: <your-session-name>"          # MCP sends this for you
curl -s -H "$AUTH" "$HOST/agent/status"             # probe first; on failure stop and report
```

## The loop

1. **Probe.** `GET /agent/status` / `phone_status`. Act only when
   `drivable:true`. Otherwise report `hint` and stop. Reconnect once
   (`phone_reconnect` / `POST /agent/mode {"mode":"agent"}`) only when the
   user's current task needs the phone, never for a health check: each runner
   launch can make the operator type the passcode. If `owner` is another
   session, do not drive. → reference: *Phone states and recovery*

2. **Look for a saved flow first.** Entering an app — a `launch_app` response,
   or the first `/agent/elements` read in a newly entered app — carries a
   `registry` block listing the saved flows for it. If one does the task, run
   it: one call instead of dozens, no screenshots. `phone_flow_list` /
   `iphone-use-mcp flow list --app <bundle>` shows them all. Check `risk` and
   `compat` before running. → reference: *Flows*

   ```bash
   "$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp" flow run system/spotlight-search --input query=Health
   ```

3. **No flow? Hand a clear goal to Jev.** `phone_jev_run` /
   `iphone-use-mcp jev run --goal "…" [--app <bundle>]` runs a fast on-phone
   agent (TypeSafe's Jev picks each step, ~1–2 s/step, no model turns) and
   returns `done` / `blocked` with its step history. Confirm goals that send,
   pay or delete with the user first; check the end screen yourself.

4. **Otherwise act in batches, not taps.** Every call costs you a full turn;
   the phone takes ~0.5 s. Open the app with `launch_app`, read
   `GET /agent/elements` once, then send everything you can already see how to
   do as ONE batch (`POST /agent/actions` / `phone_run_steps`, up to 24 steps,
   `wait_for` after each screen change) with `observe:true` (MCP default): the
   reply carries the screen the batch ended on, so you decide the next batch
   without another read. Single taps (element + snapshot, unique label,
   locator) are for exploring a screen you have not read yet; after three in a
   row the daemon says so (`batch_hint`).

5. **Verify** each step against your postcondition (`?return=delta` /
   `observe:true` returns the settled change in the same call). `ok:true` means
   the action was sent, not that it achieved anything.
   → reference: *Reading results*

6. **Offer to save it.** When a response carries `flow_suggestion`, or you
   finish a multi-step task that no flow covered, **ask the user** whether to
   keep it as a flow. Only if they agree: get the draft
   (`phone_flow_draft(save_as=…)` / `flow draft --out …` /
   `GET /agent/flow/draft`), fix its `todo` list, validate it, and run the file
   once. Publishing opens a public PR, so ask again before
   `phone_flow_publish(confirm=true)`. → reference: *Saving a flow*

When the task is done, release the phone: `phone_release_owner` /
`POST /agent/owner {"release":true}`. If a response carried `agent_focus`, the
phone is on Do Not Disturb for your session: tell the user, and releasing turns
it off again.

## Core actions

| What | HTTP `/agent/input` `type` | MCP | Flow / batch step `kind` |
|---|---|---|---|
| Open an app | `launch_app` `bundle` (or `name` for built-ins: 健康, Settings…) | `phone_run_steps` step | `launch_app` |
| Read the screen | `GET /agent/elements` | `phone_elements` | `wait_for` (`application`, `present`, `absent`) |
| Tap an element | `tap` + `element` + `snapshot` | `phone_tap_element` | `tap_locator` (durable) |
| Tap a unique label | `tap` + `label` | `phone_tap_label` | `tap_label` |
| Tap by locator | `tap_locator` + `locator` | `phone_run_steps` step | `tap_locator` |
| Tap a point | `tap` + `x`,`y` | `phone_tap` | `tap` |
| Type | `text` (into the focused field) | `phone_type` | `type` (`input` names a runtime value) |
| Key | `key` `return`… | `phone_key` | `key` |
| Home / Spotlight | `shortcut` | `phone_shortcut` | `shortcut` |
| Scroll | `scroll` `dy` (80 ≈ 15% of a screen, 400 ≈ 75%) | `phone_scroll` | `scroll` |
| System alert | `alert` `button` / `action` | `phone_run_steps` step | `alert` |
| Switch / slider | `perform` `toggle` / `adjust` | — (not in MCP) | — |

All actions, including swipe, drag, picker, set_value and the scroll variants,
are listed in the reference: *Actions catalogue*.

```bash
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" \
  -d '{"type":"launch_app","bundle":"com.apple.Health"}'        # response may carry `registry`
curl -s -m 40 -H "$AUTH" "$HOST/agent/elements"                  # give it ≥40s
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input?return=delta" \
  -d '{"type":"tap","element":3,"snapshot":"<from that read>"}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/actions" -d '{"steps":[
  {"kind":"action","action":{"type":"tap_locator","locator":{"label":"资料","kind":"Button"}}},
  {"kind":"wait_for","expect":{"present":[{"label":"导出所有健康数据"}]},"timeout_ms":8000}]}'
```

## A flow is just these steps saved

```json
{
  "version": 1,
  "name": "New note",
  "app": "com.apple.mobilenotes",
  "risk": "navigation",
  "locale": "zh-CN",
  "inputs": { "body": { "type": "string", "description": "note text" } },
  "steps": [
    { "kind": "launch_app", "bundle": "com.apple.mobilenotes" },
    { "kind": "wait_for", "expect": { "application": "备忘录" }, "timeout_ms": 15000 },
    { "kind": "tap_locator", "locator": { "label": "新建备忘录", "kind": "Button" } },
    { "kind": "type", "input": "body" }
  ]
}
```

The full field list, compat values, and publish and report steps are in
reference: *Flows*.

## Hard rules

1. **`retry_safe:false` or `outcome_unknown` means do not replay.** The phone may
   already have acted. Read the screen first; never resend text, send, pay or
   delete blindly. The same applies to a failed flow.
2. **`risk: side_effect` and destructive taps need the user's explicit OK** on
   the exact target and inputs (`confirm=true` / `--confirm`). Never operate
   payment or 2FA screens unattended.
3. **Text goes to whatever field has focus.** If a person is mid-chat, your words
   land in their message. Confirm the foreground app and the focused field first.
4. **System alerts need the `alert` action.** Taps on alert buttons ACK but often
   do nothing. **Switches** need `perform toggle` (HTTP only). Verify the new
   `value` either way.
5. **One session per phone.** Send `X-Phone-Owner`. On `409 phone_owned`, wait;
   never take over unless the user says the other session is abandoned. Release
   your lease when done.
6. **Saving and publishing are the user's call.** Ask before saving a flow, and
   ask again before publishing it or filing an issue. Both use their GitHub
   account.

If the screen changes under you (`app_changed`, a banner, a person using the
phone), stop and re-read before continuing the old plan. If the skill
disagrees with the live API, it is probably stale; see
reference: *Upgrade*. If something in iphone-use itself is broken or
confusing, offer to file an issue (reference: *Filing issues*).
