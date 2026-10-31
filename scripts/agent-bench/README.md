# Agent benchmark

Runs real tasks with `claude -p` against the installed `iphone-use-mcp` and
audits each run: execution checks, independent verification, tool calls by name
and in order, failed calls,
turns, time, tokens and cost. Use it to check that the tool descriptions lead a
model down the short path. Run it after you change a tool description, the
skill, or an output format, and compare the results with an earlier run.

It drives a real iPhone and spends model tokens, so it is a manual check and
does not run in CI.

A final answer, matching regex or successful API acknowledgement does not verify
the task. Runs without an independent postcondition check report `UNVERIFIED`.
Metrics remain available for those runs; they are not successful-task results.

```bash
scripts/agent-bench/run.py                                   # every task in tasks/
scripts/agent-bench/run.py --task settings-ios-version --model claude-sonnet-5-5
scripts/agent-bench/run.py --from-jsonl scripts/agent-bench/out/<run>/<task>.jsonl
```

- Claude gets only the iphone-use MCP server (`--strict-mcp-config`), and only
  `mcp__iphone-use__*` tools are allowed.
- `--mcp <path>` (or `IPHONE_USE_MCP`) picks another build.
- Artifacts go to a unique private `out/<timestamp>-<suffix>/` directory
  (git-ignored): the stream JSONL, MCP configuration and context per task,
  and `results.json`. Files are mode 0600 and the run directory is 0700.
- Offline replay applies the same task header as a live run. It infers the task
  from the trace filename, or accepts one explicit `--task NAME`. An unmatched
  trace cannot silently bypass the task rules.

A task is a Markdown prompt with an optional header:

```markdown
---
name: settings-ios-version
expect: "\d+\.\d+"   # answer format only; not proof of the actual version
max_tool_calls: 8    # call budget; not proof of task completion
---
Open Settings on my iPhone and tell me which iOS version it runs.
```

Keep tasks read-only or reversible. They run on a real phone.

After a separate harness or a person checks the task's postconditions, save a
receipt outside the model trace:

```json
{
  "task": "settings-ios-version",
  "trace_sha256": "<SHA-256 of settings-ios-version.jsonl>",
  "context_sha256": "<context_sha256 from results.json>",
  "verified": true,
  "verifier": "independent-device-assertions",
  "evidence": "/path/to/postcondition-results.json"
}
```

The receipt records an independent check; this driver does not perform that
check itself. The check must cover both the requested answer and the on-device
postconditions. Do not generate it from the agent's answer or a tool's `ok:true`.
The exact task and trace digest must match. A failed check uses `verified:false`.
New live runs also bind the receipt to the captured context. Replay finds the
sibling `<task>.context.json`, or accepts `--context PATH`. A changed backend
label, configuration or model context requires a new independent receipt.
Legacy traces without context still use the original trace-only contract.

```bash
scripts/agent-bench/run.py --from-jsonl out/run/settings-ios-version.jsonl \
  --task settings-ios-version --returncode 0 --verification verification.json
```

Use the original process exit code, recorded as `process_returncode` in the live
run's `results.json`. If it is unknown for an old trace, omit `--returncode` and
keep the result unverified. Do not invent a zero exit code to obtain a pass.

Result schema 2 separates `checks_passed` from `verification`. `passed` is `null`
when verification or the original exit status is missing. Exit codes are 0 for
verified passes, 1 for failed checks or postconditions, 2 for invalid arguments
or receipts, and 3 for unverified results. Compare runs with the same model,
device, task, initial state and verification procedure.

## Other backends

The driver can use a single local stdio MCP server, or a native local phone
CLI. It does not wrap a CLI as an MCP tool. Alternative backends require an
explicit `--model`; use the same exact model ID and task for each backend.

An MCP backend is a standard configuration with exactly one server:

```json
{
  "mcpServers": {
    "alt": {
      "command": "/absolute/path/to/server",
      "args": ["--state-dir", "/private/alt-state"]
    }
  }
}
```

```bash
scripts/agent-bench/run.py --task settings-ios-version \
  --model <exact-model-id> --mcp-config /private/alt-mcp.json \
  --label alt-mcp --prepare-only
```

A CLI backend needs its executable, an API reference for the helpers the task
uses, and usually its own environment:

```bash
scripts/agent-bench/run.py --task settings-ios-version \
  --model <same-exact-model-id> --cli /absolute/path/to/phone-cli \
  --instructions /private/cli-api.md --env-file /private/cli-env.json \
  --label alt-cli --prepare-only
```

`--env-file` is a JSON object of `UPPER_CASE` names to strings, applied only to
the backend. MCP values go into its single server configuration; CLI values
are set by a private launcher that forwards the original arguments unchanged. `{run_dir}` in a value becomes the run's private directory, so
a backend can get a fresh home and no inherited session, for example
`{"ALT_HOME": "{run_dir}/alt-home", "ALT_SESSION": "", "ALT_TELEMETRY": "0"}`.
Its digest is recorded in the run context. Keep backend-specific files
(configurations, references, environments) outside this repository.

Use absolute executable, source and state paths. The benchmark does not
discover devices, install anything or verify which phone a backend drives:
confirm that independently for the authorized device, and record the device,
source revision/version, transport and initial screen with the evidence.

`--prepare-only` writes the launch configuration and prints `PREPARED`, with
`passed:null`. It never starts Claude, an MCP server or a phone CLI. Inspect
the prepared invocation and device binding, then omit this flag for a live
trial. A prepared configuration is not a measured or successful task.

MCP trials disable built-in execution tools and expose only the selected server.
Claude's `ToolSearch` and `WaitForMcpServers` remain available for native MCP
discovery/readiness, and their calls count in the metrics. See the
[Claude MCP tool availability documentation](https://code.claude.com/docs/en/mcp#tool-availability). CLI
trials expose Bash with allow rules for the private environment launcher; the CLI path
must not contain whitespace or shell/glob characters. Both request `dontAsk`
permissions, disable hooks, use user settings only, and run outside this
repository so its project instructions do not enter the comparison.

Record a backend's own setup and human interactions separately from
ready-state task latency. Do not run against a default, another available
phone, or a cloud session.

Each live context records the interface, label, requested model, prompt and
reference digests, MCP configuration digest, driver digest and entrypoint
digest. Template and expanded backend environment digests are included. Observed model IDs and cache-token counts come from the Claude trace.
The entrypoint digest covers that file, not all imported backend modules:
retain the installed source revision/version as separate evidence. Runs that
use another backend's tool namespace or whose entrypoint changes fail checks.

Keep raw traces and MCP configuration private: they may contain phone data or
environment credentials. Publish sanitized summaries and verifiable receipts.
These launch paths alone provide no comparative performance result.

## Model identity for controlled comparisons

An explicit request is not proof of the model that ran. New live contexts are
checked against all reported IDs from init, assistant messages and every result
usage event. An init declaration alone does not prove which model executed;
assistant/usage identity is required. The local `<synthetic>` marker is not a
model identity. Multiple final result events invalidate execution metrics. Pass the
exact reported model ID: aliases and unresolved mappings are not inferred.
A different or mixed model set fails execution checks; a requested model with
no reported identity remains `UNVERIFIED`, even with a valid device receipt.
Raw metrics and independent device-verification evidence are retained.

`model_check` records requested and observed IDs. `comparison_eligible` is true
only when execution checks, the exact model pin and independent postconditions
all pass. A legacy/default run with no model pin may verify its task, but is not
an eligible controlled-comparison sample. An eligible individual sample still
needs matched task/device/source/setup conditions and repeat runs before any
performance verdict.

## Agent runtime and backend environment

A live run resolves `claude` from the parent environment to an absolute real
path before launching. The agent keeps that parent environment, including its
PATH, HOME and auth/settings; a backend's PATH/HOME/session overrides cannot
select a different agent or interpreter. For CLI backends, the model invokes a
private (mode0700) launcher. Its quoted environment assignments are applied by
`/usr/bin/env` before exec of the absolute backend, and argv is preserved.
Launcher time is part of the measured end-to-end path; do not report it as raw
backend-only latency. Private configurations/launchers may contain credentials.

New contexts record the agent executable and environment hashes, pre-run driver
and config hashes, expanded backend environment hash and launcher hash. Changes
to runtime, driver, backend entrypoint, launcher or config fail execution checks.
These file fingerprints do not cover all imported modules or all managed user
settings. Retain versions/source and settings evidence independently.

`runtime_check` describes the isolation evidence. `comparison_eligible` now also
requires the new runtime controls. Old contexts retain their task verification
and metrics, but lack controlled-comparison eligibility. The flag still does not
prove paired device/source/setup equivalence, repeatability or a speedup.
`--prepare-only` remains available without an installed agent and never executes
the private launcher, backend, model or phone tool.

CLI trace auditing accepts only one literal invocation of this run's launcher
per Bash call. Quoted literal arguments are supported; expansions, pipelines,
redirections, compound commands and unsupported syntax do not qualify. This is
post-run evidence checking, not a security sandbox or a device-action blocker.
A successful correlated backend tool result is required for comparison
eligibility; an attempted/denied call or framework-only discovery is not proof
of backend work. Unresolved tool results invalidate new controlled traces.
The driver source digest is frozen on module load and checked before preparation,
before agent launch and after the run.

## Execution limits and read-only comparison profiles

Each new run requests `--max-turns 12`, `--max-budget-usd 0.50` and
`--no-session-persistence` from Claude. The driver independently enforces a
300-second wall deadline. Override these with the corresponding driver flags,
including `--timeout-seconds`. A deadline kills only the owned agent process
group and records `<task>.process.json`; the task is not automatically retried.
Stopping the agent does not roll back an already dispatched device action.
The limits bound a task run; they do not make `max_tool_calls` a live call limit.
The dollar threshold can be exceeded by the model request already in flight;
it is not a hard billing cap. Turn-limit errors may also report a count above
the requested limit. Such samples fail the usage check.

Use repeated `--allow-mcp-tool NAME` flags for a profile limited to literal
backend tools. Names exclude the MCP namespace and wildcards. Framework
discovery remains enabled; trace auditing also rejects calls outside the exact
captured list. Without this option, the existing whole-backend permission
remains. Tool-name restrictions do not validate tool arguments or establish
that the selected tools are read-only; choose and verify the profile separately.

An external task can be selected with `--task-file /absolute/task.md`, including
for replay. It cannot be combined with `--task`. For example, preparation alone
does not start an agent or backend:

```sh
scripts/agent-bench/run.py --task-file /absolute/task.md \
  --mcp-config /absolute/backend.json --model EXACT_MODEL_ID \
  --allow-mcp-tool phone_get_elements --max-turns 4 \
  --max-budget-usd 0.25 --timeout-seconds 120 --prepare-only
```

New contexts capture these limits and whether the deadline fired.
`execution_policy_check` distinguishes recorded requests from reported usage
within limits. A qualifying sample needs a bound process receipt, confirmed
group-stop request and finite nonnegative reported cost and turn count within
the limits. This does not prove the model service's budget enforcement.
Calibrate the actual agent version against a synthetic
backend before trusting its turn, dollar or permission behavior. Legacy runs
remain auditable but cannot qualify for a controlled comparison without the
new policy evidence. Eligibility alone still does not prove a capability or
performance advantage.
The supervisor keeps its group identifier reserved until cleanup, including
when the agent exits normally. Descendants that deliberately create a new
session escape this group; the driver does not claim to stop those processes.

## Blind selector task fixture

`selector_oracle.py` constructs eight rows with the same neutral label and
random opaque identifiers and order. It distinguishes one StaticText with an
explicitly empty raw value from absent/whitespace values and Button decoys.
Keep each private seed, case file and App-held oracle out of agent prompts and
file tools. A pair shares rows, while each launch has its own run ID and case
digest. The task prompt contains no target identifier or position.

The App writes actual UIKit properties. Task verification checks the case,
run ID, process identity, live foreground identity before and after, delivered
window input events, cumulative foreground loss, a strict JSON answer and a
correlated successful read returning the target identifier. Input detection
covers touch/press events delivered to the fixture, not system-wide input.
State-write failure invalidates the fixture; a stale clean file cannot replace
the required live foreground/PID check. Only MCP envelopes and actual top-level
element/node collections qualify as reads; echoed selectors do not.

The owned Simulator wrapper accepts `--probe agent-selector` with the same
explicit binaries, Runner source/recipe and runtime arguments as `--probe
selector`. It installs a new disposable fixture and verifies seven scenarios:
clean reads, static/disabled/blank taps, swipe, background/return and a failed
state write. These deliberately injected inputs test the oracle; they are not
agent runs and do not establish a competitor or performance advantage.
`python3 -I scripts/test-agent-selector-oracle.py` exercises offline refusal
cases without starting a model or target.
