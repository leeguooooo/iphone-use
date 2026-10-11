# Agent tasks: one goal, handed to the daemon

A task is a goal in plain words ("打开支付宝领取会员积分") that the daemon carries out on its phone
by itself, step by step, and reports on. Three ways to use it, one API:

| Who asks | Where the daemon runs | Address |
|---|---|---|
| The iPhone app, for the phone it runs on | inside the device runner on that phone | `http://127.0.0.1:8787` |
| Any agent or the web page on a Mac | the Mac (`iphone-use serve`) | `http://<mac>:8787` |
| The iPhone app, for another phone | that phone's runner, or the Mac that drives it | its paired address |

The daemon on the phone is the same program as on the Mac (`crates/ios-embed` links it into the
runner; see `runner/README.md`). The runner is the one process on the phone that keeps running
while another app is in front, so a task keeps going after the app that started it is suspended.

## Endpoints

All need the agent credential (`Authorization: Bearer <token>`, or the session cookie). Writes also
need `X-Phone-Control: 1`; a task holds the phone's owner lease as `task-<id>` while it runs, so
other callers get `409 phone_owned` instead of fighting it.

### `POST /agent/task`

```json
{
  "goal": "打开设置查看电池电量",
  "app": "设置",                      // optional: app name or bundle id to start in
  "app_fresh": false,                 // optional: reopen at the app's first screen
  "max_steps": 30,                    // optional, 1–60
  "models": { ... },                  // optional, see Models; else the daemon's configured ones
  "confirm": "risky"                  // "risky" (default): pause before pay/send/delete/share;
                                      // "never": the caller accepts every step (Mac agents)
}
```

→ `202 {"id": "t-…", "status": "running"}`. One task at a time: a second POST while one runs is
`409 {"error": "task_running", "id": "<running id>"}`.

When `app` is omitted, an explicit opening instruction such as “打开设置…” selects
the app and starts it fresh, so a previous subpage does not become the starting
screen. Callers supplying `app` keep its current screen unless `app_fresh` is true.

### `GET /agent/task/<id>` (and `GET /agent/task/current`)

```json
{
  "id": "t-…",
  "goal": "…",
  "status": "running | needs_confirm | done | blocked | failed | cancelled | max_steps",
  "phase": "flow | jev | assist | acting | observing | summary",
  "planned_action": {"action": "CLICK", "target": "电池"} | null, // selected action, no typed text
  "flow": {"id": "settings/battery", "outcome": "ok"} | null,   // when a saved flow did it
  "steps": [
    {"n": 1, "action": "CLICK", "target": "电池", "text": null, "app": "设置",
     "ok": true, "error": null, "page_changed": true, "by": "jev | assist | flow", "ms": 640}
  ],
  "confirm": {"action": "CLICK", "target": "确认支付", "reason": "pays ¥55.51", "app": "拼多多"} | null,
  "summary": "电池电量 87%，低电量模式关闭。",   // plain words for the person, when finished
  "error": null,
  "started_at_ms": 0, "elapsed_ms": 0,
  "models_used": {"jev": "jev-latest", "assist": "…"}
}
```

`?since=<n>` returns only steps after step `n` (the app polls about once a second).

### `POST /agent/task/<id>/confirm` `{"approve": true | false}`

Answers a `needs_confirm`. Approve runs exactly the step shown, once; decline ends the task as
`blocked`. A task waiting longer than 5 minutes ends as `blocked`.

### `POST /agent/task/<id>/cancel`

Stops after the current step; `status: cancelled`.

### `GET /agent/tasks`

The last 20 tasks, newest first, without `steps`.

## How a task runs

1. **Saved flow first.** If the goal names an app and a saved flow (registry or local) matches the
   goal, the assist model selects it and its inputs, then the daemon runs the flow in one call.
   Flow execution uses no model tokens; selection and the final summary still use the model.
   Its outputs feed the summary.
2. **Jev** picks each step from the element table (`/agent/elements` semantics: snapshot-bound
   taps, occlusion refusal, `set_value` for text). About 1.5 s per step from a Mac, less on the
   phone itself.
3. **Assist** — a stronger chat model — takes over a step when Jev answers `BLOCKED` for a reason
   other than a risky action, repeats a step without progress, or answers outside the offer. It
   sees the same element table plus a screenshot and answers one action in the same action space.
   After it acts, Jev resumes.
4. **Confirm gate.** Before a step that would pay, send a message, delete, share, or submit an
   order — judged from the target label, the screen text and the goal — the task stops at
   `needs_confirm` unless `confirm` is `never`. On the phone itself the runner brings the app that
   started the task to the front (`launch_app` with the app's bundle id, passed as
   `X-Task-Return-Bundle`) so the person sees the question.
5. **Summary.** When the task ends, the assist model (or Jev's text model) writes two or three
   sentences for the person from the goal, the final screen text and the steps: what was done and
   the values read ("积分 8017 → 8167").
   It also checks whether the whole goal was achieved. A `DONE` decision without verified results
   ends as `blocked`, with the missing result explained; opening an app alone is not completion.

Screen text is untrusted data, never instructions, at every step.

## Models

```json
"models": {
  "jev":    {"base_url": "https://api.typesafe.ai/v1", "key": "…", "model": "jev-latest"},
  "text":   {"base_url": "https://openrouter.ai/api/v1", "key": "…", "model": "inception/mercury-2.5"},
  "assist": [                                  // tried in order; the first that answers is used
    // "model": "free" (or any "…:free" that fails) picks from the gateway's current free models
    {"format": "anthropic", "base_url": "https://api.anthropic.com", "key": "…", "model": "claude-sonnet-5-5"},
    {"format": "openai", "base_url": "https://api.kilo.ai/api/openrouter", "key": "", "model": "…:free"}
  ]
}
```

Keys sent with a task are used for that task only and never written to disk or logs. Without
`models`, the daemon reads its environment (`TYPESAFE_API_KEY`, `TEXT_MODEL_*`, `ASSIST_MODEL_*`)
as the Mac CLI does. Without any Jev key the task runs on `assist` alone.

The app keeps presets (`apps/ios`, Keychain): a free tier that needs no key (Kilo Gateway `:free`
models, OpenRouter `:free` with the person's own key), and the person's own providers. OpenCode
Zen's free models are only for OpenCode itself; iphone-use never presents itself as OpenCode.

## The phone's own daemon: credentials

The runner starts its daemon with `IPU_DAEMON_TOKEN` (agent bearer token), `IPU_DAEMON_PASSWORD`
and `IPU_DAEMON_HOST` (`127.0.0.1` by default; `0.0.0.0` lets other devices drive this phone, the
third way above). Setup on the Mac generates and keeps them, and the Mac's web page offers a QR
code that pairs the iPhone app with its own phone's daemon (`iphoneuse://pair?u=http://127.0.0.1:8787&c=…`).
Setup keeps the token and password in the instance state dir (`phone-daemon-token`,
`phone-daemon-password`, 0600) and passes them at every launch; the QR comes from the Mac's
`POST /agent/phone-daemon/pair`, which asks the runner (`POST /daemon/pair-code`) for a code the
embedded daemon mints in-process. `/agent/status` reports the runner's daemon as `phone_daemon`.
Details: `runner/README.md`, "The on-phone daemon".

## Flows from the phone: record, save, schedule

The person's own flows live in the daemon's local store (on the phone: inside the runner's
container) next to the registry's; tasks and schedules on that daemon find both.

- `POST /agent/flow/record {"action": "start"}` starts a recording. Every step applied through
  `/control` (the app's remote view), `/agent/input` or `/agent/actions` (a task) lands in it,
  across apps and pauses. A person's coordinate tap is recorded as the element under the finger
  (`tap_locator` by label/identifier), read just before the tap; typed text becomes an input
  (`text1`, …). `{"action": "stop"}` ends it and answers `{"draft": {"flow": …, "todo": […]}}`.
- `GET /agent/flow/record` → `{"recording": bool, "steps": n, "draft": …}` (live preview).
- `POST /agent/flow/save {"id": "myapp/daily-check", "name": "…", "description"?: "…", "flow"?: {…}}`
  validates and installs it as a local flow. Without `flow`: the running/last recording, else the
  current trail's draft (what the last task did). → `{"ok": true, "saved": {"id", "source": "local", …}}`;
  `422 invalid_flow` with the reason.
- Schedules: `GET/POST /agent/schedules` (`{"cron": "0 7 * * *", "kind": "flow", "target":
  "myapp/daily-check", "inputs": {}, "confirm_side_effects": false, "name": "…"}`),
  `DELETE/PATCH /agent/schedules/<id>`, `POST /agent/schedules/<id>/run` (run now),
  `GET /agent/schedules/<id>/runs`, `GET /agent/schedules/runs`. On the phone the daemon runs them
  in process; a flow that sends, pays or deletes needs `confirm_side_effects: true`.
- `POST /agent/flow/run {"id": "app/flow", "inputs"?: {…}, "confirm"?: bool}` runs a flow now, in process, as the caller's owner, and answers `{"ok", "result"}`.
- `GET /agent/flows[?app=<bundle>]` lists the flows this daemon can run (registry + local), as `iphone-use-mcp flow list --json` prints them.
