# Testing and scheduled runs

[中文](testing.zh-CN.md)

Two things build on flows:

- **Test suites** (`iphone-use-mcp test`) turn "open it, tap around, check it's right" into a
  file you can rerun. The exit code drops straight into CI.
- **Schedules** (`iphone-use-mcp schedule`) let the daemon run a flow or a suite at set times.
  It waits when someone else is using the phone, and tells you when a run fails.

Both run through the same engine as `flow run`. Nothing new talks to the phone.

`iphone-use test …` and `iphone-use schedule …` are the same commands, pointed at the installed
daemon for you. Add `--instance NAME` to use a second phone.

## Test suites

```bash
iphone-use-mcp test examples/tests/settings-smoke.zh-CN.yaml
```

```text
suite: 设置冒烟测试 (4 cases)
  ✓ 首页列出通用                              0.14s
  ✓ 打开通用                                  4.35s
  ✓ 打开关于本机                              0.89s
  ✓ 返回首页                                  2.14s
4 passed, 0 failed (11.31s)
```

| Flag | |
|---|---|
| `--json` | the full report: per case, per step, with timings |
| `--junit FILE` | also write a JUnit XML report |
| `--artifacts-dir DIR` | where failed cases leave their evidence (default `./iphone-use-test-artifacts`) |
| `--confirm` | required when the suite sends, publishes, pays or deletes |
| `--owner NAME` | the phone owner name for this run (`X-Phone-Owner`) |
| `--validate` | parse and check the file only; never contacts the daemon |

Exit code: `0` every case passed, `1` a case failed, `2` the suite is invalid or the phone
cannot be driven (locked, owned by another session, unreachable). A released phone is
reconnected once first.

### Format

YAML (`.yaml`, `.yml`) or JSON (`.json`):

```yaml
suite: Settings smoke          # optional label; default is the file name
app: com.apple.Preferences     # optional: launched before setup
risk: navigation               # optional, as for flows; side_effect needs --confirm
setup:                         # optional: steps run once before the cases
  - {kind: back, after_ms: 300}
cases:
  - name: open General
    steps:                     # flow steps, verbatim
      - {kind: tap_locator, locator: {label: General, kind: Button}}
    assert:                    # every assertion must hold
      - present: About         # a label, a locator, or a list of either
        timeout_ms: 8000       # default 5000
      - application: Settings
        absent: [{label: Bluetooth}]
```

- **Steps** are [flow steps](agent-reference.md#flows): `tap_locator`, `tap_label`, `type`,
  `key`, `scroll`, `swipe`, `launch_app`, `back`, `alert`, `wait_for`, `pause` and the rest,
  checked by the same validator.
- **`{kind: flow, id: …, inputs: {…}}`** runs a saved flow (a registry id or a file) as part
  of a case. A `side_effect` flow makes the whole suite need `--confirm`.
- **An assertion** is a `wait_for` expectation: `application`, `present` and `absent`, using
  the same locator fields (`label`, `identifier`, `kind`, `value`, `enabled`, `visible`,
  `focused`). It is polled until it holds or its `timeout_ms` runs out.

The steps run in order, then the assertions. A case stops at its first failure, and the next
case still runs. `setup` failing stops the suite, because no case could mean anything.

### When a case fails

It leaves `<artifacts-dir>/<case>/` (mode 0700) with:

- `screenshot.png`: the screen right after the failure.
- `elements.json`: the element tree at that moment.
- `steps.json`: every step that ran, its time and the daemon's answer.
- `error.json`: which step or assertion failed, and why.

That is usually enough to tell a broken app from a broken test. An example: the iPhone 13's
General row sits under the iOS 26 floating search bar. Its screenshot showed the tap had
landed on the bar.

### Writing reliable suites

- Prefer `tap_locator` with a `kind` over coordinates.
- After a `scroll`, give the list time to stop gliding before you tap (`after_ms: 2500`).
  A tap on a moving list only stops it.
- An app reopens where it was left. Bring it to a known screen in `setup`.
- One suite per UI language. Labels are what the screen says
  (`settings-smoke.en.yaml` and `settings-smoke.zh-CN.yaml`).

## Schedules

The daemon keeps a list of schedules: a cron line plus a flow or a suite to run.

```bash
# weekdays at 9:00, local time
iphone-use-mcp schedule add --cron "0 9 * * 1-5" --test ~/suites/smoke.yaml --name "morning smoke"
iphone-use-mcp schedule add --cron "@daily" --flow health/export-all-zh-cn
iphone-use-mcp schedule list
iphone-use-mcp schedule runs [ID]
iphone-use-mcp schedule run ID              # queue a run now
iphone-use-mcp schedule disable|enable ID
iphone-use-mcp schedule rm ID
```

The same list is on the web at `http://<mac>:44321/schedules`, with Run now, Pause and Delete
buttons and a form for new schedules.

**Cron** has five fields: minute, hour, day of month, month, day of week. It uses local time.
`*`, `a-b`, lists, `*/n` and `@hourly`, `@daily`, `@weekly`, `@monthly` all work. Sunday is
0 or 7.

### What happens at the scheduled time

The scheduler reads `/agent/status` like any careful agent before starting:

| The phone is… | The run… |
|---|---|
| free and drivable | starts, as owner `schedule-<id>`, and releases the phone when it is done |
| owned by another session, or someone is watching it live | is **postponed** and tried again every 3 minutes |
| locked | **waits for unlock** and starts once the phone is unlocked |
| released after idle | reconnects first, which may make iOS ask for the passcode |
| still not free when its window closes (default 60 minutes, `--window-mins`) | is recorded as **missed** |

A run is never stacked on top of an unfinished one. That occurrence is recorded as
**skipped**. If the daemon was down at the scheduled time and comes back within the window,
the run still happens. One run is kept for however many minutes were missed.

**Side effects.** A flow or suite that sends, publishes, pays or deletes can only be
scheduled with `--confirm` (`confirm_side_effects: true`). You give that OK once, for that
exact target and those inputs, when you create the schedule.

**Results.** Each schedule keeps its last 20 runs: when the run happened, how long it took,
and the outcome (`passed`, `failed`, `missed` or `skipped`). It also keeps a one-line
summary, the error, and the evidence directory
(`<state dir>/schedule-runs/<run>/`, with stdout, stderr and the failed case's artifacts).

**Notifications.** A failed, missed or skipped run raises a macOS notification. Set
`IPHONE_USE_SCHEDULE_NO_NOTIFY=1` in the daemon's environment to turn them off.
`--webhook URL` also POSTs a JSON event there, for Slack, Discord or anything that takes
one. The URL is never shown back in full, because webhook URLs often carry a secret.

### HTTP API

Every call takes the usual bearer token or signed-in browser. A mutation also needs
`X-Phone-Control: 1`.

| Call | |
|---|---|
| `GET /agent/schedules` | the schedules, each with `next_run_at` and `last_run` |
| `POST /agent/schedules` | `{cron, kind: "flow"\|"test", target, inputs?, confirm_side_effects?, name?, webhook?, window_mins?, enabled?}` → `201`; `400` `invalid_cron` / `invalid_target` / `confirm_required` / … |
| `PATCH /agent/schedules/:id` | `{enabled: bool}` |
| `DELETE /agent/schedules/:id` | removes it and its history |
| `POST /agent/schedules/:id/run` | queue a run now → `202`; `409 run_open` when one is already queued or running |
| `GET /agent/schedules/:id/runs`, `GET /agent/schedules/runs` | run history, newest first |

`target` is a registry id or an absolute flow file for `flow`, and an absolute suite path for
`test`. The daemon validates it with `iphone-use-mcp` before saving.

### The nightly flow canary

`scripts/flow-reverify.py` (launchd `com.leeguoo.iphone-use.flow-reverify`, 03:30) does more
than run flows. It refreshes `verified_on`, files `flow report` issues, and opens one review
PR. It also skips a parked phone on purpose, so nobody is asked for a passcode at night. A
schedule covers the running part. To check that a set of flows still works on your phone
every morning, put them in a suite as `{kind: flow, id: …}` steps and schedule the suite. The
registry upkeep stays with the canary job for now.
