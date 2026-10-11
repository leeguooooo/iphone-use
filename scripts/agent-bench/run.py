#!/usr/bin/env python3
"""Run real agent tasks against iphone-use with `claude -p` and audit them.

Each task is a Markdown file in tasks/ with a small header:

    ---
    name: settings-ios-version
    expect: "iOS \\d+"          # regex the final answer must match (optional)
    max_tool_calls: 8            # budget the run is judged against (optional)
    ---
    Open Settings on my iPhone and tell me which iOS version it runs.

The driver gives Claude only the iphone-use MCP server (the installed one by
default), records Claude's stream JSONL, and reports per task: pass/fail, tool
calls by name, failed tool calls, turns, wall time, tokens and cost. This tells
whether the tool descriptions lead a model to the short path, the way
XcodeBuildMCP's and mobile-mcp's agent benchmarks do.

It drives a real phone and spends model tokens, so it is manual, not CI:

    scripts/agent-bench/run.py                 # every task
    scripts/agent-bench/run.py --task settings-ios-version --model claude-sonnet-5-5
    scripts/agent-bench/run.py --from-jsonl out/…/settings.jsonl   # re-audit a run

Artifacts go to scripts/agent-bench/out/<timestamp>/ (git-ignored).
"""
import argparse
import datetime
import json
import os
import pathlib
import re
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_MCP = pathlib.Path.home() / "Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
TOOL_PREFIX = "mcp__iphone-use__"


def parse_task(path):
    """(header dict, prompt) from a task file; the header is optional.

    Header values are plain text, or quoted ("…" / '…') when they hold a
    `#`; anything after an unquoted ` #` is a comment."""
    text = pathlib.Path(path).read_text(encoding="utf-8")
    header = {"name": pathlib.Path(path).stem}
    if text.startswith("---\n"):
        head, _, text = text[4:].partition("\n---\n")
        for line in head.splitlines():
            key, sep, value = line.partition(":")
            if not sep:
                continue
            key, value = key.strip(), value.strip()
            quoted = re.match(r"""^(["'])(.*)\1(\s+#.*)?$""", value)
            value = quoted.group(2) if quoted else re.sub(r"\s+#.*$", "", value)
            header[key] = int(value) if key == "max_tool_calls" else value
    return header, text.strip()


def audit(lines, returncode=0):
    """Summarize one stream-json run (an iterable of JSONL lines). A run
    without a final `result` event, or whose `claude` exited non-zero, is an
    error."""
    calls, failed, names = [], [], {}
    pending = {}
    result = {}
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            event = json.loads(raw)
        except ValueError:
            continue
        kind = event.get("type")
        content = (event.get("message") or {}).get("content") or []
        if kind == "assistant":
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    name = block.get("name", "")
                    short = name[len(TOOL_PREFIX):] if name.startswith(TOOL_PREFIX) else name
                    calls.append(short)
                    names[short] = names.get(short, 0) + 1
                    pending[block.get("id")] = short
        elif kind == "user":
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("is_error"):
                    failed.append(pending.get(block.get("tool_use_id"), "?"))
        elif kind == "result":
            result = event
    usage = result.get("usage") or {}
    return {
        "tool_calls": len(calls),
        "tools": names,
        "sequence": calls,
        "failed_calls": failed,
        "turns": result.get("num_turns"),
        "duration_ms": result.get("duration_ms"),
        "cost_usd": result.get("total_cost_usd"),
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "answer": result.get("result", ""),
        "is_error": bool(result.get("is_error")) or not result or returncode != 0,
    }


def judge(header, summary):
    """Pass/fail and the reasons, from the task header and the audit."""
    reasons = []
    if summary["is_error"]:
        reasons.append("claude reported an error, exited non-zero or gave no result")
    expect = header.get("expect")
    if expect and not re.search(expect, summary["answer"] or ""):
        reasons.append(f"answer does not match {expect!r}")
    budget = header.get("max_tool_calls")
    if isinstance(budget, int) and summary["tool_calls"] > budget:
        reasons.append(f"{summary['tool_calls']} tool calls > budget {budget}")
    return not reasons, reasons


def run_task(path, out_dir, mcp, model):
    header, prompt = parse_task(path)
    config = out_dir / "mcp.json"
    config.write_text(json.dumps({"mcpServers": {"iphone-use": {"command": str(mcp)}}}))
    jsonl = out_dir / f"{header['name']}.jsonl"
    command = [
        "claude", "-p", prompt,
        "--output-format", "stream-json", "--verbose",
        "--mcp-config", str(config), "--strict-mcp-config",
        "--allowedTools", f"{TOOL_PREFIX}*",
    ]
    if model:
        command += ["--model", model]
    started = time.monotonic()
    with open(jsonl, "w", encoding="utf-8") as sink:
        proc = subprocess.run(command, stdout=sink, stderr=subprocess.STDOUT, cwd=out_dir, check=False)
    with open(jsonl, encoding="utf-8") as source:
        summary = audit(source, proc.returncode)
    summary["wall_ms"] = int((time.monotonic() - started) * 1000)
    passed, reasons = judge(header, summary)
    return {"task": header["name"], "passed": passed, "reasons": reasons, **summary}


def print_report(rows):
    for row in rows:
        mark = "PASS" if row["passed"] else "FAIL"
        secs = (row.get("duration_ms") or row.get("wall_ms") or 0) / 1000
        cost = row.get("cost_usd")
        print(f"{mark} {row['task']}: {row['tool_calls']} tool calls, "
              f"{len(row['failed_calls'])} failed, {row.get('turns')} turns, {secs:.1f} s"
              + (f", ${cost:.4f}" if isinstance(cost, (int, float)) else ""))
        print(f"     sequence: {' → '.join(row['sequence']) or '(none)'}")
        for reason in row["reasons"]:
            print(f"     ✗ {reason}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--task", action="append", help="task name (tasks/<name>.md); repeatable")
    parser.add_argument("--model", help="exact Claude model id for a controlled comparison")
    parser.add_argument("--mcp", default=os.environ.get("IPHONE_USE_MCP", str(DEFAULT_MCP)))
    parser.add_argument("--from-jsonl", help="re-audit a saved run instead of running Claude")
    parser.add_argument("--json", action="store_true", help="print machine-readable results")
    args = parser.parse_args(argv)

    if args.from_jsonl:
        with open(args.from_jsonl, encoding="utf-8") as source:
            summary = audit(source)
        rows = [{"task": pathlib.Path(args.from_jsonl).stem, "passed": not summary["is_error"],
                 "reasons": [], **summary}]
    else:
        tasks = sorted((HERE / "tasks").glob("*.md"))
        if args.task:
            tasks = [HERE / "tasks" / f"{name}.md" for name in args.task]
        out_dir = HERE / "out" / datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        out_dir.mkdir(parents=True, exist_ok=True)
        rows = [run_task(task, out_dir, args.mcp, args.model) for task in tasks]
        (out_dir / "results.json").write_text(json.dumps(rows, ensure_ascii=False, indent=2))
    if args.json:
        print(json.dumps(rows, ensure_ascii=False, indent=2))
    else:
        print_report(rows)
    return 0 if all(row["passed"] for row in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
