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

`--env-file` is a JSON object of `UPPER_CASE` names to strings, applied to the
trial's process. `{run_dir}` in a value becomes the run's private directory, so
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
trials expose Bash with allow rules for the exact executable; the CLI path
must not contain whitespace or shell/glob characters. Both request `dontAsk`
permissions, disable hooks, use user settings only, and run outside this
repository so its project instructions do not enter the comparison.

Record a backend's own setup and human interactions separately from
ready-state task latency. Do not run against a default, another available
phone, or a cloud session.

Each live context records the interface, label, requested model, prompt and
reference digests, MCP configuration digest, driver digest and entrypoint
digest. The environment digest is included. Observed model IDs and cache-token counts come from the Claude trace.
The entrypoint digest covers that file, not all imported backend modules:
retain the installed source revision/version as separate evidence. Runs that
use another backend's tool namespace or whose entrypoint changes fail checks.

Keep raw traces and MCP configuration private: they may contain phone data or
environment credentials. Publish sanitized summaries and verifiable receipts.
These launch paths alone provide no comparative performance result.
