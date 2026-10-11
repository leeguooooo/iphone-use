# Agent benchmark

Runs real tasks with `claude -p` against the installed `iphone-use-mcp` and
audits each run: pass/fail, tool calls by name and in order, failed calls,
turns, time, tokens and cost. Use it to check that the tool descriptions lead a
model down the short path. Run it after you change a tool description, the
skill, or an output format, and compare the results with an earlier run.

It drives a real iPhone and spends model tokens, so it is a manual check and
does not run in CI.

```bash
scripts/agent-bench/run.py                                   # every task in tasks/
scripts/agent-bench/run.py --task settings-ios-version --model claude-sonnet-5-5
scripts/agent-bench/run.py --from-jsonl scripts/agent-bench/out/<run>/<task>.jsonl
```

- Claude gets only the iphone-use MCP server (`--strict-mcp-config`), and only
  `mcp__iphone-use__*` tools are allowed.
- `--mcp <path>` (or `IPHONE_USE_MCP`) picks another build.
- Artifacts go to `out/<timestamp>/` (git-ignored): the stream JSONL per task
  and `results.json`.

A task is a Markdown prompt with an optional header:

```markdown
---
name: settings-ios-version
expect: "\d+\.\d+"   # regex the final answer must match
max_tool_calls: 8    # judged against the run's tool calls
---
Open Settings on my iPhone and tell me which iOS version it runs.
```

Keep tasks read-only or reversible. They run on a real phone.
