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
| `--from N[:M]` | resume at case N, or at step M of case N ([below](#resuming-retries-and-the-timing-trace)) |
| `--retries N` | run a failed case again, up to N times (at most 10) |

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

Every run, passing or not, also writes `<artifacts-dir>/trace.ndjson` (see below).

That is usually enough to tell a broken app from a broken test. An example: the iPhone 13's
General row sits under the iOS 26 floating search bar. Its screenshot showed the tap had
landed on the bar.

### Resuming, retries and the timing trace

**`--from N[:M]`** starts a run part way through. Cases and steps count from 1; a case's steps
are its `steps` (a `flow` step counts once per step of that flow), then its assertions.

- `--from 3` runs `setup`, then case 3 and the cases after it.
- `--from 3:2` skips `setup` and starts case 3 at its second step, on whatever screen the phone
  is showing. Use it after fixing the phone or the suite by hand.

A failed case prints where to resume (`resume: --from 3:2`, the step that failed; `resume_from`
in `--json`). Skipped cases count as `skipped` in the report and the JUnit file.

**`--retries N`** runs a failed case again from its first step, up to N more times. The case
passes if any attempt passes; the report says so (`passed on attempt 2`, `attempts` and
`earlier_failures` in `--json`). Evidence is kept for the last failed attempt only. Since a
retry starts where the last attempt stopped, it helps most when each case begins by bringing
the app to a known screen.

**`trace.ndjson`** has one JSON line per step that ran, setup and every attempt included:

```json
{"case":"open General","attempt":1,"step":1,"origin":"cases[1].steps[0]","kind":"tap_locator","role":"step","ms":412,"outcome":"ok"}
{"case":"open General","attempt":1,"step":2,"origin":"cases[1].assert[0]","kind":"wait_for","role":"assert","ms":5003,"outcome":"expectation_timeout"}
```

`step` is the `M` of `--from N:M`, `outcome` is `ok` or the error that failed the step. Each run
replaces the file; `jq -s 'sort_by(-.ms)[:5]' trace.ndjson` lists the slowest steps.

### Writing reliable suites

- Prefer `tap_locator` with a `kind` over coordinates.
- After a `scroll`, give the list time to stop gliding before you tap (`after_ms: 2500`).
  A tap on a moving list only stops it.
- An app reopens where it was left. Bring it to a known screen in `setup`.
- One suite per UI language. Labels are what the screen says
  (`settings-smoke.en.yaml` and `settings-smoke.zh-CN.yaml`).

### Running suites on a simulator in CI (preview)

A simulator needs no phone and no Apple ID, so a suite can run on a GitHub-hosted Mac.
`iphone-use simulator start` boots the simulator, builds the runner for it, starts the runner
and a daemon, and returns once both answer. Simulator support is a preview and has not been
verified on every Xcode yet; see [the design note](research/2026-10-simulator-support.md).

```yaml
# .github/workflows/ui-tests.yml
jobs:
  ui-tests:
    runs-on: macos-14
    steps:
      - uses: actions/checkout@v4
      - name: Install iphone-use (no device setup, no MCP client registration)
        run: |
          curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh \
            | IPHONE_USE_NO_SETUP=1 IPHONE_USE_NO_CLIENT_SETUP=1 sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - name: Boot a simulator, start the runner and the daemon
        run: iphone-use simulator start "iPhone 15" --instance ci-sim
      - name: Build and install the app under test
        run: |
          xcodebuild -scheme MyApp -destination 'platform=iOS Simulator,name=iPhone 15' \
            -derivedDataPath build build
          xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/MyApp.app
      - name: Run the suite
        run: IPHONE_USE_INSTANCE=ci-sim iphone-use-mcp test tests/smoke.yaml --junit report.xml
      - if: always()
        uses: actions/upload-artifact@v4
        with:
          name: ui-tests
          path: |
            report.xml
            ~/.iphone-use/instances/ci-sim/simulator-daemon.log
      - if: always()
        run: iphone-use simulator stop "iPhone 15" --instance ci-sim || true
```

- The suite's exit code fails the job: `0` all passed, `1` a case failed, `2` the suite could
  not run.
- `--instance ci-sim` names the instance the test step reaches with `IPHONE_USE_INSTANCE`.
  Add `--retries 1` to the test step if the app has known slow screens.
- `simulator start` turns off the Simulator's "Connect Hardware Keyboard" preference, so
  `type` steps get the software keyboard.
- Simulators have no camera, no App Store apps, and no push notifications without `simctl push`.
  Keep suites that need those on a phone.

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
