#!/usr/bin/env python3
"""The agent benchmark's audit and judging, against a recorded stream-json run."""
import importlib.util
import pathlib
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("agent_bench", HERE / "agent-bench" / "run.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)

with open(HERE / "fixtures" / "agent-bench-run.jsonl", encoding="utf-8") as source:
    summary = bench.audit(source)
assert summary["tool_calls"] == 3, summary
assert summary["sequence"] == ["phone_status", "phone_run_steps", "phone_run_steps"], summary
assert summary["tools"] == {"phone_status": 1, "phone_run_steps": 2}, summary
assert summary["failed_calls"] == ["phone_run_steps"], summary
assert summary["turns"] == 4 and summary["duration_ms"] == 12345, summary
assert summary["input_tokens"] == 1000 and summary["output_tokens"] == 200, summary
assert summary["answer"] == "Your iPhone runs iOS 18.1."

passed, reasons = bench.judge({"expect": r"\d+\.\d+", "max_tool_calls": 8}, summary)
assert passed and not reasons, reasons
passed, reasons = bench.judge({"expect": "Android", "max_tool_calls": 2}, summary)
assert not passed and len(reasons) == 2, reasons

# A run with no result event, or a failed claude, never passes, even for a
# task with no expect rule.
lines = open(HERE / "fixtures" / "agent-bench-run.jsonl", encoding="utf-8").read().splitlines()
cut = bench.audit([l for l in lines if '"type":"result"' not in l.replace(" ", "")])
assert cut["is_error"], cut
assert not bench.judge({}, cut)[0]
crashed = bench.audit(lines, returncode=1)
assert crashed["is_error"] and not bench.judge({}, crashed)[0], crashed
assert not bench.audit(lines)["is_error"]

with tempfile.TemporaryDirectory() as tmp:
    task = pathlib.Path(tmp) / "t.md"
    task.write_text('---\nname: demo\nexpect: "a#b"   # comment\nmax_tool_calls: 5 # budget\n---\nDo it.\n')
    header, prompt = bench.parse_task(task)
    assert header == {"name": "demo", "expect": "a#b", "max_tool_calls": 5}, header
    assert prompt == "Do it."
    plain = pathlib.Path(tmp) / "plain.md"
    plain.write_text("Just a prompt.\n")
    assert bench.parse_task(plain) == ({"name": "plain"}, "Just a prompt.")

for task in sorted((HERE / "agent-bench" / "tasks").glob("*.md")):
    header, prompt = bench.parse_task(task)
    assert header["name"] == task.stem and prompt, task

print("agent-bench: ok")
