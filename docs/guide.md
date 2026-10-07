# iphone-use — full guide

The README is the short version; this is everything else.

## How it works

```text
Browser <── GET /agent/mjpeg ── iphone-use daemon ── 127.0.0.1:9100 ──┐
Browser ── POST /control ─────> iphone-use daemon ── 127.0.0.1:8100 ──┤ device runner on iPhone
Agent   ── /agent/* ──────────> iphone-use daemon ── 127.0.0.1:8100 ──┘
```

- `scripts/setup-wda.sh` builds and signs the iphone-use **device runner**
  (`runner/IPhoneUseRunner`, an XCTest UI-test bundle that replaced WebDriverAgent and
  speaks the same HTTP API), starts it on the phone, and pins two `iproxy` loopback
  relays: `8100` for control, `9100` for the MJPEG screen.
  The daemon only ever talks to localhost, so a background process never holds the
  phone's changing IP. USB is the supported path; Wi-Fi/`socat` is a manual experiment.
- The browser gets the live picture from `/agent/mjpeg` (PNG stills as fallback) and
  sends input through `POST /control`, which answers success or failure for every
  command instead of accepting it blindly over a possibly dead channel.
- Agents read the accessibility tree as text (`/agent/elements`), screenshots as PNG,
  and act through `/agent/input` or a guarded multi-step batch (`/agent/actions`).
- The daemon owns the runner lifecycle: it releases the phone after idle time, rebuilds
  the runner with backoff, and reports every state in `/agent/status`.
- "WDA" below, in status fields (`wda`, `wda_actionable`, …) and in environment
  variables (`WDA_*`, `PHONE_REMOTE_WDA_*`) is the historical name and now means this
  device runner.

Design, lifecycle, failure states, and security boundaries:
**[`docs/direct-device-architecture.html`](direct-device-architecture.html)**.

## Quick start

### Requirements

- macOS 15 or later, with **full Xcode.app** (Command Line Tools alone are not enough).
  Sign in under Xcode → Settings → Accounts and pick a development team; a free
  Personal Team works, but its runner profile needs periodic renewal. An App Store
  Connect API key (`WDA_ASC_KEY_PATH`, `WDA_ASC_KEY_ID`, `WDA_ASC_ISSUER_ID`) signs
  without an Xcode account.
- An iPhone with **Developer Mode** on, paired with and trusted by the Mac over USB.
- The phone **unlocked and awake** while the runner is built, launched, and used. It
  cannot get past Face ID or the passcode.
- `iproxy` from `brew install libimobiledevice`.
- A Rust toolchain only if you build from source.

### Install and connect the first phone

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

The installer fetches the latest GitHub Release, registers a per-user LaunchAgent,
writes the loopback runner endpoints, installs the matching agent skill, lays the
device runner sources down at `~/.iphone-use/runner`, and drops the setup helper at
`~/.iphone-use/setup-wda.sh`. It does not
prove your team, phone, runner, and relays work together — that is the next step, with
the phone connected, trusted, unlocked, and awake:

```bash
~/.iphone-use/setup-wda.sh doctor    # explains any USB / trust / DDI / WARP blocker
~/.iphone-use/setup-wda.sh           # build, sign, install, launch the runner, start relays
~/.iphone-use/setup-wda.sh status
```

Then open **`http://<mac-lan-ip>:44321/setup`**. The built-in guide translates
`/agent/status` into the current blocker (USB, trust, developer service, runner, external
host) without changing your VPN or running setup for you. Once the phone is drivable,
continue to **`/phone`** and enter the password `install.sh` printed.

More than one iPhone paired? Pin the same classic UDID in both places:

```bash
export PHONE_REMOTE_UDID=00008…
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
WDA_UDID="$PHONE_REMOTE_UDID" ~/.iphone-use/setup-wda.sh
```

### Hand the phone back to yourself

The runner occupies the phone while it runs. To use the phone by hand, pause it and
resume it before the next agent session:

```bash
~/.iphone-use/setup-wda.sh pause     # disables the launchd job, stops only PID-verified processes
~/.iphone-use/setup-wda.sh resume
```

Simpler still is the **交还 (hand back)** button in the web toolbar, or
`POST /agent/mode {"mode":"human"}`: the daemon stops the runner so the phone belongs to whoever
is holding it, and reports `human_handoff:true`; while that holds, agent input gets
409 `phone_handed_to_human` instead of restarting the runner under your fingers. The same
button then reads **交给 agent** (give to agent); press it, or send `{"mode":"agent"}`, to
put the phone back under remote control.

The daemon also does this on its own: after 5 minutes (doubling, up to an hour, when the phone is wanted back soon after a release) without agent activity or a
live viewer it stops the runner and parks its supervisor, and the phone stays parked
across logouts and reboots. A runner kept up around the clock is relaunched every time
iOS kills it, and each launch asks for the passcode to enable UI automation — so the
phone prompted all day while nobody was using it. The next agent request, or
`POST /agent/mode {"mode":"agent"}`, brings the runner back from its cached product (no
rebuild) — unlock the phone if asked. On `/phone` a parked phone shows **连接手机**
(connect phone); press it with the phone unlocked and awake. `PHONE_REMOTE_IDLE_RELEASE_SECS` changes the
window; `0` keeps the runner up (v0.6.3–v0.7.3 behaviour).

### Upgrade

> v0.9 removed the iPhone Mirroring backend; installs that had
> `PHONE_REMOTE_BACKEND=mirror` are served over WDA after upgrading.

```bash
iphone-use upgrade            # install the latest release (daemon + skill), then refresh other skill copies
iphone-use upgrade --check    # change nothing: "iphone-use 0.6.6 -> 0.6.7" or "... is up to date"
iphone-use upgrade --json     # the same as JSON: name, current, latest, update_available, skills
```

The installer links `~/.local/bin/iphone-use` to the app's executable, so the command is on
PATH once `~/.local/bin` is. `upgrade` runs the same `install.sh` one-liner as installing
(which also refreshes the installer-managed skill), then runs
`claude plugin update iphone-use@leeguooooo-plugins` or `git pull --ff-only` for a plugin
or git-checkout copy of the skill; any other copy is reported as not release-matched, to be
replaced by the installer's. Exit code `0` = upgraded / already current / check ran; `2` = the check or download
failed. A `cargo build` binary is never replaced: `upgrade` prints the install command
instead.

The daemon checks GitHub daily and reports `version` / `latest` / `update_available` in
`/agent/status`; the web client shows a banner. One-shot commands (`iphone-use stop`) print
`iphone-use X is available (you have Y). Upgrade: iphone-use upgrade` on stderr at most
once a day (cached in `${XDG_CACHE_HOME:-~/.cache}/iphone-use/update-check.json`, 2 s
timeout). `CI`, `IPHONE_USE_NO_UPDATE_CHECK`, `USE_NO_UPDATE_CHECK` or the older
`PHONE_REMOTE_NO_UPDATE_CHECK` turn off both the daily check and the notice. Details of
what the installer verifies are under [Operations → Upgrades](#upgrades).

**Unattended upgrades** are opt-in and gated on the phone being idle:

```bash
~/.iphone-use/auto-update.sh enable     # daily at 04:30; `disable` / `status` / `run --dry-run`
```

Each run resolves the latest release and upgrades only when it is newer *and* nobody
is using the phone: no `X-Phone-Owner` lease, no hold, the daemon is not
releasing/reconnecting, no live viewer, and no agent request in the last 15 minutes
(`idle_secs` on `/agent/status`; override with `AUTO_UPDATE_IDLE_SECS`). A connected
phone with WDA up but nobody driving it counts as idle. Otherwise it logs one line to
`~/Library/Logs/iPhoneUse/auto-update.log` and tries again tomorrow. The upgrade itself is
`install.sh` with its SHA-256 checks and rollback, after which the script refreshes its
own installed copy from the new release. `run --force` skips the idle gate;
`run --reinstall` reinstalls the current release. (`scripts/auto-update.sh` self-installs on
`enable`; the installer will offer `--auto-update` once the multi-instance work lands.)

## Drive the phone from the browser

`/phone` shows the live screen: as H.264 from `/agent/h264` where the browser has
WebCodecs (the daemon re-encodes WDA's JPEG frames on the Mac's hardware encoder,
~0.7–2.5 Mbit/s), otherwise as `/agent/mjpeg` (25–40 Mbit/s, LAN only). It turns your clicks, drags, long-presses,
scrolls, and typing (Unicode included) into acknowledged `POST /control` commands with a
bounded `ttl_ms`. Nothing steals Mac focus. The **Controls** panel shows the accessibility
tree so you can tap by exact label instead of by pixel.

The **流程** (flow) panel records what you do into a replayable flow file:

- Only acknowledged actions are recorded. Exact accessibility labels are preferred;
  coordinate gestures are marked fragile.
- Typed text becomes a named runtime input; the literal value is discarded and never
  written to the downloaded JSON.
- After each action the recorder diffs the element tree and, when a new unique
  identifier or a foreground-app change is provable, inserts a reviewable `wait_for`
  checkpoint; otherwise a short visible pause remains. It never copies arbitrary labels
  or values into a checkpoint, because they may be private.
- Review, reorder, delete, fill inputs, then download valid flow v1 JSON or run it once.
  A recording with unpersisted actions is labelled an incomplete draft and cannot run.
  Running requires every input filled and an explicit "no irreversible actions" check.
- **打开脚本** reopens a saved flow after strict client-side validation (same limits as
  the CLI; literal typed text is rejected in favour of named inputs).

What you record here is what the [registry](#flows-and-the-official-flow-registry)
distributes.

## Drive the phone from the native iOS app

`apps/ios` is a native SwiftUI app (iOS 17+). Connect by scanning: on the Mac, open the
iphone-use page and press **Scan** in the toolbar, then scan the QR code with the app (or the
iPhone's own camera, which can also open plain browser control). The code works once, within
5 minutes, and the app keeps a device token that renews its session; changing the control
password revokes every paired phone. Typing the address and password still works. It plays the H.264 feed with the system's hardware decoder and sends taps, long
presses, swipes, drags, text and Home to `POST /control`, the same contract as the web
page. The phone being controlled must stay unlocked: iOS does not let automation type the
lock-screen passcode, so when it is locked the app says so within seconds and connects as
soon as it is unlocked. On screens an app hides from capture (payment and banking apps),
the app shows the daemon's wireframe of the accessibility tree instead of a white picture.
Build with `cd apps/ios && xcodegen generate`, then open in Xcode.

## Agent API

Full reference: **[`docs/agent-api.html`](agent-api.html)**. The bundled skill
([`skills/iphone-use/SKILL.md`](../skills/iphone-use/SKILL.md)) teaches an agent the loop.

### Authentication and headers

| Header | When | Meaning |
|---|---|---|
| `Authorization: Bearer <token>` | every `/agent/*` call | `PHONE_REMOTE_AGENT_TOKEN` if set; otherwise the daemon password (legacy fallback). |
| `X-Phone-Control: 1` | every state-changing POST | CSRF/intent guard on top of auth, not a replacement. Required by `/control`, `/agent/input`, `/agent/actions`, `/agent/mode`, `/agent/hold`, `/agent/owner`, and the POST forms of `/agent/inbox`. The web and MCP clients add it. |
| `X-Phone-Owner: <session>` | control requests | Claims the phone for this session (issue #72). While the lease is live (refreshed per request, `PHONE_REMOTE_OWNER_LEASE_SECS` default 300) other sessions — and header-less clients — get `409 phone_owned` with the owner and seconds left. Read-only calls are unaffected. `X-Phone-Owner-Takeover: 1` replaces a live lease and is logged. |

### Endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/agent/status` | Readiness and lifecycle: `backend`, `device_state`, `drivable`, `wda_actionable`, `recovery_owner`, `setup_blocked_on` / `setup_phase` / `setup_message`, `hint`, viewer counts, `instance`, `udid`, `owner` / `owner_lease_remaining_secs`, `hold_remaining_secs`, `idle_secs` (since the last agent request; status polls do not count), `capture_redacted` (the watched picture is blank because the app hides it from capture), `version` / `latest`. |
| `GET` | `/agent/screenshot` | Current screen as PNG, from the phone. On a screen the app hides from capture, the accessibility tree drawn as a labelled wireframe over the blank area, with `X-Capture-Redacted: 1`; `?raw=1` returns the capture untouched. |
| `GET` | `/agent/elements` | Flattened accessibility tree with an ephemeral `snapshot` token, an `ax_stats` usability block, and a sparse `alert` block when a system alert is up. `?since=<snapshot>` returns a `delta` instead of the full tree. WDA missing/busy → `503`; failed source → `502`, never a fake empty `200`. |
| `GET` | `/agent/mjpeg` | Authenticated live MJPEG stream. |
| `POST` | `/agent/input` | One action: tap, drag, long-press, scroll, text, key, `home`/`spotlight`, `launch_app`, `set_value`, `perform`, `alert`. `?return=delta` also attempts a post-action tree read and returns the change plus a `settle` block (`settled`, `reason`: `stable` / `budget_exhausted` / `observation_failed`, `waited_ms`, `captures`, `budget_ms`, and `sparse` / `stale` when they apply). Observation is best-effort: a slow or failed read never downgrades an applied action to an unknown outcome. A settled delta with no row added, changed or removed also carries `no_visible_change: true` (a scroll or tap that hit nothing responsive). |
| `POST` | `/agent/actions` | Up to 24 `action` / `wait_for` / `pause` steps validated as a whole, run under one WDA lock, stopped at the first failure. Response: `completed`, `applied_actions`, `failed_step`, `outcome` (the failed step), `failed_step_outcome`, `batch_outcome` (`nothing_applied` / `partially_applied` / `unknown`), `retry_safe` (the whole batch, and the only field that authorises a replay). |
| `POST` | `/agent/mode` | `{"mode":"agent"}` brings WDA up on the configured target (never changes the UDID). `{"mode":"human"}` stops WDA and hands the phone to its holder; agent input then answers `409 phone_handed_to_human` until `agent` takes it back. |
| `POST` | `/agent/hold` | `{"secs":N}` (0 clears, max 14400) keeps the phone from idle release around a human pause. `503 device_release_in_progress` if release already started. |
| `POST` | `/agent/owner` | `{"release":true}` hands the owner lease back early. |
| `GET` | `/agent/apps` | Installed apps with `version` / `bundle_version` / `system`, plus `device.ios`, from `devicectl` on the daemon's Mac. Cached 10 min; `?bundle=<id>` filters, `?refresh=1` bypasses the cache. `503 apps_unavailable` on failure (never an empty list); `409 target_required` with several phones and no configured UDID. |
| `GET` | `/agent/intents` | The curated semantic-intent registry (see [Semantic intents](#semantic-intents-shortcuts-on-device)). |
| `POST` | `/agent/intent` | Dispatch one registered verb; the result arrives on `/agent/inbox`. |
| `GET` / `POST` | `/agent/inbox`, `/agent/inbox/drain` | Peek / append / atomically drain the Shortcuts result queue. |
| `POST` | `/control` | Cookie-authenticated browser input with a required 1–2500 ms `ttl_ms`. |

### Semantics an agent must respect

- **Gate on `drivable:true`** (and `wda_actionable:true`). `device_state` is one of
  `ready`, `locked`, `blocked`, `offline`, `releasing`, `released`, `reconnecting`;
  `mode` is `agent` or `offline`.
- **At-most-once delivery.** Expiry before dispatch → `408 not_sent`, `retry_safe:true`.
  Transport failure after dispatch → `502`, post-dispatch deadline → `504`, both
  `outcome_unknown`, `retry_safe:false`: read the screen before doing anything again.
- **Snapshot-bound targets.** An element index is valid only with the `snapshot` from the
  same `/agent/elements` response; a changed tree fails with `409 stale_element_snapshot`.
  Exact-label taps fail closed on zero or multiple matches. Persist labels, identifiers,
  and locators in scripts — never indexes or snapshot tokens.
- **Element-scoped actions.** `set_value` writes a field (clear-then-type), `scroll` with
  `element` keeps the gesture inside that element, `perform` invokes a named affordance
  (`increment`, `decrement`, `adjust`, `toggle`, `menu`, `double_tap`, `two_finger_tap`,
  `scroll_to_visible`, `pinch`, `rotate`, `force_press`). `force_press` needs 3D Touch:
  on every iPhone since the XR / 11 WDA refuses it before touching the screen, and the
  daemon answers `422 force_press_unsupported` (`not_sent`, `retry_safe:true`); use
  `menu` (long press) instead. `{"type":"scroll","page":true,"dy":N}`
  scrolls the page itself: the daemon finds the page scroller and starts the drag clear
  of inputs, buttons, nested scrollers and bars (`422 no_page_scroller` when there is
  none). Rows of a system layer over the app carry `overlay` (`notification`,
  `dynamic_island`, `cover_sheet`). With
  `PHONE_REMOTE_ELEMENTS_AFFORDANCES=1` the tree advertises which actions each row
  supports.
- **Taps that would land elsewhere are refused.** An element tap whose centre is covered
  by another control (a fixed header, the keyboard, a floating button) answers
  `409 element_occluded`, `not_sent`: scroll it clear (`perform` `scroll_to_visible`)
  and read again. A target WDA reports as `visible:false` (in the tree but not drawn, like
  Chrome's tab grid behind the page) answers `409 element_not_visible`.
  `"allow_occluded":true` overrides both. Label taps take an optional
  `"kind":"Button"`, and rows outside the screen never compete with a visible match.
- **`set_value` is read back.** A field that kept its old contents (web views often
  ignore direct writes) answers `409 value_not_applied`, `outcome:"no_effect"`; tap the
  field and send `text` instead. Formatting the field adds itself still counts as applied.
- **Screens hidden from capture** (PayPay, banks, wallets) come back from
  `/agent/screenshot` as a wireframe with `X-Capture-Redacted: 1`. Drive them by
  elements; never run vision on a blank capture.
- **Every `/agent/*` answer is timed**: a `timing` object (`total_ms`, `wda_ms`,
  `daemon_ms`, and each WDA call with count, ms and bytes) in JSON bodies, a
  `Server-Timing` header, and one line per request in `~/.iphone-use/agent-timing.jsonl`
  (route, owner and timings only — never request or screen content).
- An unknown action `type` answers `400 invalid_action` with the supported list, and a
  common guess (`scroll_into_view`, `click`, `fill`, …) also gets the exact request to send.
- **System alerts** are a separate surface: taps on their buttons are acknowledged
  without effect. Use `{"type":"alert","button":"…"}` or `{"action":"accept"|"dismiss"}`.
  App Switcher and Control Center are system gestures WDA cannot reach.
- **`/agent/actions`** never reports a replay as safe once any action applied.
  `tap_locator` uses the same exact label/identifier/kind/value/state fields as
  `wait_for` and requires one unique match.

```bash
HOST=http://<mac-lan-ip>:44321; AUTH="Authorization: Bearer $TOKEN"
MUTATION="X-Phone-Control: 1"; OWNER="X-Phone-Owner: my-script"
curl -s -H "$AUTH" "$HOST/agent/status"
curl -s -H "$AUTH" "$HOST/agent/screenshot" -o screen.png
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"tap","x":0.5,"y":0.3}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/input" -d '{"type":"text","text":"你好"}'
curl -s -H "$AUTH" -H "$MUTATION" -H "$OWNER" -X POST "$HOST/agent/actions" \
  -d '{"steps":[{"kind":"action","action":{"type":"shortcut","name":"home"}},{"kind":"wait_for","expect":{"present":[{"label":"搜索"}]},"timeout_ms":3000}]}'
```

### Semantic intents (Shortcuts, on-device)

For things the UI cannot reach efficiently (battery level, Health samples, sending a
message with a native confirmation), a curated set of **verbs** runs through one bridge
shortcut. The daemon opens `shortcuts://run-shortcut` on the phone via WDA — no Spotlight
or clipboard — and the shortcut posts its result back to `/agent/inbox`.

```bash
python3 deploy/make-bridge-shortcut.py --token "$PHONE_REMOTE_AGENT_TOKEN" \
  --verb ping --verb battery --verb focus_on --verb focus_off
cp "iU Bridge.shortcut" ~/Library/Mobile\ Documents/com~apple~CloudDocs/
```

Then import it on the phone — an agent can do this itself: Files → search
`iU Bridge` → tap the file → **Add Shortcut** (no Mac dialog needed; opening the file
on the Mac and accepting the import works too).

**Do Not Disturb while an agent drives.** With `focus_on` / `focus_off` in the bridge and
the registry, the daemon turns DND on before an agent session's first action — only if no
Focus is already on — and off again when the phone is released, idles out or is handed to
a person. The phone shows a notice both times; the first run asks once to allow the
bridge's notifications (the agent is told, as `agent_focus: waiting_for_permission`).
Fire-and-forget, so it works with the return path below left closed.
`PHONE_REMOTE_AUTO_FOCUS=0` turns it off.

Verbs live in `~/.iphone-use/intents-registry.json` (start from
[`deploy/intents-registry.example.json`](../deploy/intents-registry.example.json)); the
shortcut's name must equal the registry's `bridge.name`, and the bearer token lives in
the shortcut's own request headers. `--self-test` checks the plist parts that fail
silently. Each verb needs one interactive permission grant on first use, and Shortcuts
foregrounds during a call.

**The return path needs the phone to reach the daemon** (issue #59). The hardened
default `PHONE_REMOTE_HOST=127.0.0.1` can dispatch a verb but never hear the answer, so
intents are off until you choose one of:

| Return path | How | Trade-off |
|---|---|---|
| LAN bind | `PHONE_REMOTE_HOST=0.0.0.0` with the password **and** `PHONE_REMOTE_AGENT_TOKEN` set | Simplest; exposes the daemon's authenticated surface to your whole LAN. |
| USB reverse tunnel | Forward a phone-side port back to the Mac loopback listener | No LAN exposure; more moving parts. |

Fire-and-forget verbs work on plain loopback. Never bind `0.0.0.0` on an untrusted
network: WDA's own `8100`/`9100` have no authentication.

## MCP server

[`iphone-use-mcp`](../crates/mcp/README.md) ships inside the installed app
(`~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp`) and as a checksummed
standalone archive on every release. It speaks MCP over stdio and adds
`X-Phone-Control` and `X-Phone-Owner` (`PHONE_REMOTE_OWNER`, default `mcp-<pid>`) to
its daemon requests automatically.

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOUR_ACCOUNT/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": {
        "PHONE_REMOTE_URL": "http://127.0.0.1:44321",
        "PHONE_REMOTE_TOKEN": "<your-agent-token>"
      }
    }
  }
}
```

| Group | Tools |
|---|---|
| See | `phone_status`, `phone_capabilities` (what this build supports vs what is possible right now; wakes nothing), `phone_screenshot`, `phone_elements` (carries a `registry` block naming installed flows for the app on screen) |
| Act | `phone_tap`, `phone_tap_element` (snapshot-bound), `phone_tap_label` (unique exact label), `phone_scroll`, `phone_type` (CJK-clean), `phone_key`, `phone_shortcut` (`home`/`spotlight`) — each takes an optional `observe` |
| Batch | `phone_run_steps` — up to 24 steps incl. `tap_locator`, `launch_app`, `picker`, `alert`, long-press/swipe/drag, `wait_for` |
| Lifecycle | `phone_reconnect` (restart WDA on the configured phone, never a UDID switch), `phone_hold`, `phone_release_owner` |
| Flows | `phone_flow_list`, `phone_flow_info`, `phone_flow_run`, `phone_flow_update`, `phone_flow_publish`, `phone_flow_report` |

For those seven act tools and `phone_capabilities`, the parsed JSON arrives as MCP
`structuredContent` and the text block is a preview trimmed at 8 KiB — parse the
structured field. `phone_run_steps` carries its complete batch result in BOTH, so either
is safe to parse. Every other tool keeps the return it always had: complete JSON as
text for most (including `phone_flow_run`'s execution result, passed or failed), an
image for `phone_screenshot`, and explanatory text for errors raised before a call
reaches the phone. Read `structuredContent` when it is present; otherwise read
`content` according to the tool. When a result cannot be confirmed,
`outcome: "unknown"` with `retry_safe: false` says so in a form a program can branch on
— and branch on the explicit `retry_safe` boolean, never on `outcome`. Full table:
[`crates/mcp/README.md`](../crates/mcp/README.md).

`observe: true` on a single-step act tool asks the daemon to watch the screen settle and
return what changed (`settle`, `snapshot`, `delta`) with the result. It is off by default
because the wait costs latency an action does not otherwise pay. `settle.reason` is
`stable`, `budget_exhausted` (the observation window ran out — the action still happened)
or `observation_failed` (the read itself broke); `stale: true` means the tree returned is
the previous successful read rather than the current screen, and `sparse: true` marks an
empty or container-only tree, which is never called stable.

Keyboard dismissal, uninstall, and target configuration stay HTTP-only. Full schemas:
[`crates/mcp/README.md`](../crates/mcp/README.md).

## Flows and the official flow registry

A **flow** is a strict JSON file (`version: 1`) with the same guarded steps as
`phone_run_steps` plus named string inputs. The `iphone-use-mcp` binary validates and
runs one without any model in the loop:

```bash
MCP="$HOME/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
"$MCP" flow validate examples/flows/search-spotlight.json          # offline
PHONE_REMOTE_TOKEN=… "$MCP" flow run examples/flows/search-spotlight.json --input 'query=coffee'
```

The **official registry** — [`leeguooooo/iphone-use-flows`](https://github.com/leeguooooo/iphone-use-flows),
the only supported source — turns this into an installable catalogue of reviewed per-app
flows, the way chrome-use ships site packs:

```bash
"$MCP" flow update                        # mirror into ~/.iphone-use/flows: sha256 + strict validation, 0600
"$MCP" flow list --category health        # id · risk · verified · inputs · name
"$MCP" flow info health/export-all-zh-cn  # metadata and step templates
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn
PHONE_REMOTE_TOKEN=… "$MCP" flow run health/export-all-zh-cn --artifacts-dir ./runs   # record the run (0700 dir, 0600 files)
"$MCP" flow add my.json --as myapp/daily  # your own flow; survives update
"$MCP" flow publish my.json --as myapp/daily --alias MyApp --note "iPhone 17 Pro Max, iOS 26"   # opens the PR via gh
"$MCP" flow report health/export-all --result @run.json --note "profile button renamed"           # files a flow-broken issue
```

When a run fails, the result grows a **`diagnosis`** block: the daemon's own
0-based `failed_step`, whether the screen could be read at all (`observable`),
a `reason` (`locator_matches_now`, `locator_no_match`, `locator_ambiguous`,
`still_present`, `no_similar_element`, `no_readable_tree`, `screen_unreadable`,
`diagnosis_timeout`, `step_has_no_locator`), and up to five
`candidates` with the `matched` / `differed` locator fields they were picked
on. It is one bounded read taken after the run: nothing is re-sent, the flow is
never silently edited, and the run's own `outcome` / `applied_actions` /
`retry_safe` are not touched by it.

`--artifacts-dir DIR` writes a machine-readable record of the run — schema,
flow name and sha256, the versions it ran against (`unavailable` when they
could not be read, never guessed), timings, and the projected result. The
directory is created `0700` and checked for writability *before* anything is
sent; files are `0600`. Structure only: typed input and screen text are never
written. If the write fails after the phone has already acted, the result is
still printed in full with an added `artifact_error` — a failure to record
something cannot rewrite what happened.

Registry metadata on a flow is optional: `app` (bundle id), `category`, `risk`
(`read_only` · `navigation` · `side_effect` — the last refuses to run without
`--confirm` / `confirm=true`), `locale` (labels are language-specific), `tags`, and
`verified_on` (hardware runs that proved the exact file). Files are pure JSON, so
installing the registry never executes code; a checksum or validation failure aborts the
whole update and leaves the store untouched.

Rules baked into the format: `--input KEY=VALUE` resolves only for the current run and is
never written back; a flow stops at the first failed step and never retries; command-line
values can appear in shell history, so inputs must not carry credentials, codes, or
private content, and send/publish/pay/delete actions must be declared `side_effect`.

An app update does not silently break the registry: each flow records the app (or iOS)
version it was proved on, the CLI reads what the phone has installed (`flow apps`), and
every listing shows a `compat` verdict — `verified`, `untested-newer`, `incompatible`,
`broken`, `needs-verification`, `draft`, `unknown`. `flow run` refuses broken or
incompatible flows without `--force`. A nightly canary (`scripts/flow-reverify.py`) re-runs
the verified read-only flows on a real phone and reaches one of three verdicts per flow:
**verified** (refresh `verified_on`), **failed** (tag `needs-verification` and file a
`flow-broken` issue), or **skipped** — the phone was locked, owned by someone else, not
drivable, or the daemon could not determine the outcome. A skipped flow is left exactly as
it was: a night the phone was unavailable says nothing about the flow, so it is neither
marked broken nor credited with a fresh date.

Agents are pushed toward the registry rather than asked to remember it: `phone_elements`
lists the installed flows for the app on screen, a 3+-step `phone_run_steps` success
suggests saving the sequence, and a failed `phone_flow_run` keeps the failure so
`phone_flow_report` needs only a note. The research behind the format is in
[`docs/scripted-flows-research.html`](scripted-flows-research.html).

## Operations

### A second phone (named instances)

One daemon drives one phone. To drive another iPhone at the same time, install a
named instance for it; the default install is left exactly as it is:

```bash
./install.sh --instance lab --udid <UDID>      # UDID from: xcrun devicectl list devices
PHONE_REMOTE_INSTANCE=lab ~/.iphone-use/instances/lab/setup-wda.sh
```

The instance gets its own copy of the app, state directory
(`~/.iphone-use/instances/lab`, including its own runner build products under
`runner-build/`; the sources at `~/.iphone-use/runner` are shared), launchd labels (`com.leeguoo.iphone-use.lab`,
`com.leeguoo.iphone-use.wda.lab`), loopback-only daemon, agent token and ports. Ports
are derived from the name and persisted. The installer prints them, and
`PHONE_REMOTE_INSTANCE=lab setup-wda.sh instance-context` shows them later. Signing
(team, bundle ID, App Store Connect key) is inherited from the default instance's runner
supervisor unless set explicitly. A phone that another instance already drives, or a port
another instance owns, is refused before anything changes. Agents target the instance
with `PHONE_REMOTE_URL=http://127.0.0.1:<port>` and that instance's
`PHONE_REMOTE_AGENT_TOKEN` (in `~/Library/LaunchAgents/com.leeguoo.iphone-use.lab.plist`).
Remove only that instance with `uninstall.sh --instance lab`. The default uninstall
refuses while named instances remain.

### Lifecycle and recovery

`/agent/status` is the source of truth. `recovery_owner` is `daemon` for managed
loopback WDA, `unconfigured` until a first-run target is persisted, `external` for an
unmanaged endpoint. Before launching WDA the daemon asks the phone whether it is locked: a
locked phone shows `setup_blocked_on:"locked"` within seconds of connecting (instead of
after a ~70 s Xcode timeout), and the runner starts once two consecutive readings say it
is unlocked. A lock-screen failure that still slips through retries quietly from 5 s to
1 min; other failures back off from 5 s to 5 min; a verified recovery resets both.
Interactive setup waits at most 5 min for an unlock.

Measured on an iPhone 17 Pro Max (iOS 27): an unlocked cold connect is drivable in
13–22 s; a locked one shows "unlock" in ~4 s and is drivable ~14 s after the unlock; a
snapshot-bound tap with `?return=delta` (tap, then the settled tree) takes ~4 s. `POST /agent/mode {"mode":"agent"}` (or MCP `phone_reconnect`) restarts the
configured target once — do not loop it; read `hint` and `setup_blocked_on`
(`warp|proxy|usb|trust|ddi|account|automation_mode_disabled|xcode_too_old|locked`) first.
`automation_mode_disabled` means the phone is unlocked but iOS has not enabled UI
automation: turn on Settings › Developer › Enable UI Automation and accept any passcode or
"Allow automation" prompt on the phone.
`xcode_too_old` means the runner exited with code 74 and the phone runs a newer iOS
than the selected Xcode supports: install an Xcode that supports it (a beta Xcode for a
beta iOS). KeepAlive then retries only every 15 minutes, since each attempt launches the
runner on the phone; `setup-wda.sh doctor` prints the phone's iOS next to the Xcode SDK.

**Who may end a reconnect.** A bring-up is owned by the task that started it, and only
that owner ends it. Every begin mints a generation, so a late task cannot end the round
that replaced it, and `GET /agent/status` never ends one: reading status refreshes the
health cache but does not move the lifecycle. A wait ends for exactly one reason —
the phone became drivable, it is locked, setup published a prerequisite, the budget ran
out, or another round took over — and each is logged. The whole wait is bounded by that
budget: a probe is cut off by the absolute deadline rather than its own ceiling, evidence
that arrives after the deadline is discarded, and a wait that is cancelled (runtime
shutdown, a dropped future) releases its own round rather than leaving `reconnecting`
set. Evidence cached before a bring-up is never treated as proof that the bring-up
finished — that briefly shipped in v0.6.4 and opened input onto a runner that was still
being replaced.

### Upgrades

```bash
iphone-use upgrade    # or: curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

The installer resolves the release tag to one commit for helpers and the skill, fetches
the daemon app from the matching Release asset, checks its SHA-256, installs and
byte-verifies the skill at `~/.agents/skills/iphone-use` plus its Claude Code discovery
link, and only then replaces the daemon. A skill failure aborts the upgrade; a later
daemon failure restores the previous skill. `IPHONE_USE_SKIP_SKILL=1` leaves the skill
untouched (a degraded install with no compatibility claim). Existing settings
(`PHONE_REMOTE_WDA_URL`, password, token, UDID) carry over.
`PHONE_REMOTE_NO_UPDATE_CHECK=1` (or `IPHONE_USE_NO_UPDATE_CHECK=1` / `USE_NO_UPDATE_CHECK=1`) disables the daily check.

On install the app keeps a valid existing signature and repairs an invalid one with
keychain-free ad-hoc signing; the daemon holds no macOS permission grant that a new
signature could invalidate.

### Configuration

| Variable | Default | Purpose |
|---|---|---|
| `PHONE_REMOTE_HOST` / `PHONE_REMOTE_PORT` | `127.0.0.1` / `44321` | Listen address and port (`0.0.0.0` for LAN; a password is then mandatory). |
| `PHONE_REMOTE_PASSWORD` | *(none)* | Browser login; doubles as the agent bearer only when no agent token is set. |
| `PHONE_REMOTE_AGENT_TOKEN` | *(none)* | Dedicated agent bearer. When set, it is the **only** accepted bearer. |
| `PHONE_REMOTE_UDID` | detected and persisted by the installer | Canonical iPhone for the managed runner and destructive commands. Requests cannot switch it; change the deployment and restart. Pass the same value as `WDA_UDID` to setup. |
| `PHONE_REMOTE_WDA_URL` / `PHONE_REMOTE_WDA_MJPEG_URL` | `http://127.0.0.1:8100` / `:9100` | Runner control and MJPEG loopbacks (any WebDriverAgent-compatible endpoint works). Control fails closed when unreachable. |
| `PHONE_REMOTE_WDA_MANAGED` | on for loopback endpoints | Whether this daemon owns the runner supervisor/relay lifecycle. |
| `PHONE_REMOTE_IDLE_RELEASE_SECS` | `300` | Stop the runner and park its supervisor after this many idle seconds; the next agent request starts it again. `0` keeps the runner up, at the cost of a passcode prompt each time iOS kills it. |
| `PHONE_REMOTE_OWNER_LEASE_SECS` | `300` | How long an `X-Phone-Owner` lease lives without a refreshing request. |
| `IPU_RUNNER_SRC` | `~/.iphone-use/runner` | Device runner sources setup builds (a repo checkout's `scripts/setup-wda.sh` uses its own `runner/`). Persisted only when not the default. |
| `WDA_RUNNER_REBUILD` | off | `1` makes the next setup ignore the recorded runner product and build again. |
| `PHONE_REMOTE_WDA_SNAPSHOT_MAX_DEPTH` | runner default 64 | Applies to an external WebDriverAgent only; the device runner bounds its own tree reads (depth ladder, 5000 nodes). |
| `PHONE_REMOTE_WDA_SNAPSHOT_TIMEOUT_S` | — | Same: WebDriverAgent only. |
| `PHONE_REMOTE_ELEMENTS_AFFORDANCES` | off | `1` adds sparse `actions`, `selected`, `min`/`max` to `/agent/elements` rows. |
| `PHONE_REMOTE_ELEMENTS_TRAITS` | off | `1` also emits raw accessibility trait names. |
| `PHONE_REMOTE_NO_UPDATE_CHECK` | off | Skip the daily release check. |

## Security

The daemon exposes live phone control over the network; treat its URL and password as
credentials.

- The password / cookie / bearer protects port `44321` only. **The runner's own `8100`
  and `9100` on the phone have no authentication**, and the USB `iproxy` relay does not add
  any — another host on the phone's Wi-Fi can reach them directly. Use it only on a
  trusted, isolated network; turning off iPhone Wi-Fi while on USB removes that exposure.
- A real authenticated device transport is Phase 2 (a companion app or a controlled
  tunnel). Until then, daemon login does not protect the runner.
- From outside the LAN, reach `44321` through an authenticated HTTPS reverse proxy or a
  trusted VPN/tunnel (Tailscale, for example) — never by exposing the runner's ports. The daemon
  serves plain HTTP, honours `X-Forwarded-Proto`, and sets an `HttpOnly` +
  `SameSite=Lax` session cookie.
- The owner lease (`X-Phone-Owner`) is coordination between cooperating sessions, not a
  security boundary.
- Do not leave payment apps, private chats, or 2FA screens open while exposing access.
  Stop the LaunchAgent when not in use.

### WARP / VPN

WARP and similar VPNs break the CoreDevice tunnel WDA needs. `setup-wda.sh doctor`
detects it and `/agent/status` reports `device_state:"blocked"`,
`setup_blocked_on:"warp"`; neither changes your VPN — that is an operator decision, and
managed Macs need an administrator split-tunnel rule.

## Development

```bash
cargo build --release --bin iphone-use --bin iphone-use-mcp
./scripts/make-app.sh                  # → ./iPhoneUse.app
./install.sh ./iPhoneUse.app           # sign, install, write the LaunchAgent (uses the worktree skill)

# or run the daemon without installing
PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100 \
PHONE_REMOTE_WDA_MJPEG_URL=http://127.0.0.1:9100 \
PHONE_REMOTE_HOST=0.0.0.0 PHONE_REMOTE_PASSWORD=secret ./target/release/iphone-use serve
```

Release: `scripts/release.sh 0.6.8` bumps the crates, runs the release gate's tests, pushes the tag, waits for the release build and syncs the plugin marketplace (`--dry-run` to preview).

| Path | What lives there |
|---|---|
| `crates/server` | daemon: device control (WebDriverAgent protocol), MJPEG proxy, browser `/control`, agent API |
| `runner/` | the device runner: XCTest UI-test bundle serving the control API and MJPEG on the phone |
| `crates/mcp` | `iphone-use-mcp`: MCP server, flow runner, registry client, `flow publish` / `report` |
| `crates/core` | shared auth helpers |
| `web/index.html` | browser client (MJPEG + `/control`) |
| `skills/iphone-use` | the agent skill the installer ships |
| `scripts/`, `deploy/`, `install.sh` | runner setup, packaging, LaunchAgent, bridge-shortcut generator |
| `docs/` | architecture, agent API reference, device setup pitfalls, flows research |

### Roadmap

- [x] Direct/WDA element-tree control, Unicode text, label taps, on-device screenshots (component-validated on iPhone 17 / iOS 27; see [`docs/wda-setup.html`](wda-setup.html)).
- [x] MCP server; release binaries in CI with a one-line installer.
- [x] Deterministic flows, the official flow registry, publish/report loop.
- [ ] Record the direct-browser hardware acceptance matrix below.
- [ ] Make first-device setup, signing renewal, sleep/reconnect recovery, and multi-device selection understandable from the product UI.
- [ ] Revalidate every advertised command on real hardware.
- [x] Native iOS companion app with scan-to-connect pairing (`apps/ios`).
- [x] Per-request timing, and the first round of latency work driven by it (snapshot taps and settle read the tree once).
- [ ] Phase 2 authenticated device transport beyond the LAN (a controlled tunnel).
- [ ] A short demo of an agent driving the phone.

### Hardware acceptance boundary

The direct browser default is accepted only after all of these are observed on a real
iPhone:

1. On a Mac with no macOS privacy grants for the app, install, run WDA setup, and keep WDA up.
2. `/agent/status` reports `backend:"direct"`, `wda:true`, `wda_actionable:true`, `drivable:true` for the intended UDID.
3. `/phone` from another device shows a continuously updating picture; stopping the 9100 relay makes the UI report degraded/offline, not success.
4. Tap, drag, long-press, scroll, ASCII and CJK text through `/control` are each acknowledged and land exactly once.
5. `/agent/elements`, `/agent/screenshot`, `/agent/input` work through bearer auth, including with a failed WDA endpoint; no command ever moves the Mac cursor.
6. `releasing → released → reconnecting → ready` is observed, plus lock/unlock, USB reconnect, Mac restart, WDA renew/reinstall, and no silent target change on a multi-device Mac.
7. On an isolated network, record whether the phone's IP exposes unauthenticated `8100/9100`.

## Feedback

Rough edge? [Open an issue](https://github.com/leeguooooo/iphone-use/issues). AI agents
are explicitly invited: the bundled skill tells them to file structured issues (with the
user's consent) when the API misleads them, and to send flow problems to the
[registry](https://github.com/leeguooooo/iphone-use-flows/issues).

## License

[MIT](../LICENSE)
