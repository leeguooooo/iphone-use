#!/usr/bin/env python3
"""The agent benchmark's audit and judging, against a recorded stream-json run."""
import importlib.util
import contextlib
import hashlib
import io
import json
import pathlib
import tempfile
import os
import stat
import sys
from unittest.mock import patch

HERE = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("agent_bench", HERE / "agent-bench" / "run.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


def forbid_live_task(*args, **kwargs):
    raise AssertionError("offline audit tests must never start a real agent or phone task")


real_run_task = bench.run_task
bench.run_task = forbid_live_task

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

with tempfile.TemporaryDirectory() as tmp:
    trace = pathlib.Path(tmp) / "settings-ios-version.jsonl"
    original = "\n".join(lines) + "\n"
    trace.write_text(original)
    header, _ = bench.parse_task(HERE / "agent-bench/tasks/settings-ios-version.md")
    row = bench.assess(header, trace, returncode=0)
    assert row["checks_passed"] and row["passed"] is None, row
    assert row["verification"]["state"] == "unverified", row
    display = io.StringIO()
    with contextlib.redirect_stdout(display):
        bench.print_report([{**row, "duration_ms": None, "wall_ms": None}])
    assert "duration unknown" in display.getvalue()
    assert "0.0 s" not in display.getvalue()
    assert row["trace_sha256"] == hashlib.sha256(trace.read_bytes()).hexdigest()

    receipt = {"task": header["name"], "trace_sha256": row["trace_sha256"],
               "verified": True, "verifier": "independent-fixture-assertions",
               "evidence": "synthetic UI postconditions matched"}
    proof = pathlib.Path(tmp) / "verification.json"
    proof.write_text(json.dumps(receipt))
    assert bench.assess(header, trace, 0, proof)["passed"] is True
    assert bench.assess(header, trace, 1, proof)["passed"] is False
    assert bench.assess(header, trace, verification=proof)["passed"] is None
    proof.write_text(json.dumps({**receipt, "verified": False}))
    assert bench.assess(header, trace, 0, proof)["passed"] is False

    for overrides in [{"task": "notes-search"}, {"trace_sha256": "0" * 64},
                      {"verified": "true"}, {"verified": 1},
                      {"verifier": ""}, {"evidence": ""}]:
        proof.write_text(json.dumps({**receipt, **overrides}))
        try:
            bench.assess(header, trace, 0, proof)
            raise AssertionError(f"invalid receipt accepted: {overrides}")
        except ValueError:
            pass
    proof.write_text(json.dumps(receipt))
    trace.write_text(original + "\n")
    try:
        bench.assess(header, trace, 0, proof)
        raise AssertionError("a receipt for a different trace was accepted")
    except ValueError:
        pass
    trace.write_text(original)

    def replay(*extra):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = bench.main(["--from-jsonl", str(trace), "--json", *extra])
        return code, json.loads(output.getvalue())[0]

    code, row = replay("--returncode", "0")
    assert code == 3 and row["passed"] is None
    code, row = replay("--returncode", "0", "--verification", str(proof))
    assert code == 0 and row["passed"] is True
    code, row = replay("--verification", str(proof))
    assert code == 3 and row["process_returncode"] is None

    # Offline judging must apply the same task format and call budget.
    trace.write_text(original.replace("Your iPhone runs iOS 18.1.", "Android"))
    code, row = replay("--returncode", "0")
    assert code == 1 and row["passed"] is False and not row["checks_passed"]
    events = [json.loads(line) for line in lines if line != "not json at all"]
    final = events.pop()
    for i in range(6):
        call_id = f"extra-{i}"
        events.extend([
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": call_id,
                 "name": "mcp__iphone-use__phone_status", "input": {}}]}},
            {"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": call_id, "content": "ready"}]}},
        ])
    events.append(final)
    trace.write_text("\n".join(json.dumps(event) for event in events) + "\n")
    code, row = replay("--returncode", "0")
    assert code == 1 and row["tool_calls"] == 9 and not row["checks_passed"]

    # A model's claim of verification is not an independent receipt.
    events = [json.loads(line) for line in lines if line != "not json at all"]
    events[-1]["verified"] = True
    trace.write_text("\n".join(json.dumps(event) for event in events) + "\n")
    code, row = replay("--returncode", "0")
    assert code == 3 and row["passed"] is None

    for args in [["--verification", str(proof)],
                 ["--from-jsonl", ""], ["--from-jsonl", "   "],
                 ["--verification", ""],
                 ["--from-jsonl", str(trace), "--verification", ""],
                 ["--from-jsonl", str(trace), "--task", "notes-search", "--task", "home-screen-apps"],
                 ["--from-jsonl", str(trace), "--task", "missing-task"]]:
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                bench.main(args)
                raise AssertionError(f"invalid replay options accepted: {args}")
            except SystemExit as error:
                assert error.code == 2

    old_here = bench.HERE
    try:
        bench.HERE = pathlib.Path(tmp) / "empty-benchmark"
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                bench.main([])
                raise AssertionError("an empty benchmark passed")
            except SystemExit as error:
                assert error.code == 2
    finally:
        bench.HERE = old_here

with tempfile.TemporaryDirectory(prefix="bench-native-") as tmp:
    root = pathlib.Path(tmp)
    program = root / "other-phone-cli"
    program.write_text("fixture executable, never launched\n")
    program.chmod(0o700)
    reference = root / "usage.md"
    reference.write_text("Use the local phone helpers over standard input.")
    env_file = root / "cli-env.json"
    env_file.write_text(json.dumps({"OTHER_CLI_HOME": "{run_dir}/cli-home", "OTHER_CLI_SESSION": ""}))
    config = root / "other-mcp.json"
    config.write_text(json.dumps({"mcpServers": {"alt": {
        "command": sys.executable, "args": ["fixture-server.py"],
        "env": {"FIXTURE_SECRET": "not-for-the-public-context"}}}}))
    mcp = bench.backend_spec(None, config=config, label="other-mcp")
    cli = bench.backend_spec(None, cli=program, reference=reference, label="other-cli",
                             env=bench.load_env_file(env_file))
    assert mcp["tool_prefix"] == "mcp__alt__" and mcp["tools"] == "ToolSearch,WaitForMcpServers"
    assert cli["tool_prefix"] is None and cli["tools"] == "Bash"
    for bad in ({"lower": "x"}, {"OK": 1}, ["OTHER_CLI_HOME"]):
        bad_env = root / "bad-env.json"; bad_env.write_text(json.dumps(bad))
        try:
            bench.load_env_file(bad_env)
        except ValueError:
            pass
        else:
            raise AssertionError(f"accepted a bad --env-file: {bad}")
    task = HERE / "agent-bench/tasks/settings-ios-version.md"

    for i, invalid in enumerate([
        {"mcpServers": {}},
        {"mcpServers": {"one": {}, "two": {}}},
        {"mcpServers": {"alt*": {"command": sys.executable}}},
        {"mcpServers": {"alt": {"type": "http", "url": "https://example.invalid"}}},
        {"mcpServers": {"alt": {"command": sys.executable, "args": "not-a-list"}}},
        {"mcpServers": {"alt": {"command": sys.executable, "env": {"X": 1}}}},
    ]):
        bad = root / f"invalid-{i}.json"
        bad.write_text(json.dumps(invalid))
        try:
            bench.backend_spec(None, config=bad)
            raise AssertionError("invalid native backend accepted")
        except ValueError:
            pass

    fake_calls = []
    current_trace = {"value": original.replace("mcp__iphone-use__", "mcp__alt__")}

    def fake_claude(command, **kwargs):
        # A real model, MCP server, backend CLI or phone must never run here.
        assert pathlib.Path(command[0]).resolve() == pathlib.Path(sys.executable).resolve() and command[1] == "-p"
        fake_calls.append((command, kwargs))
        kwargs["stdout"].write(json.dumps({"type": "system", "subtype": "init", "model": command[command.index("--model") + 1]}) + "\n")
        for raw in current_trace["value"].splitlines():
            try:
                event = json.loads(raw)
            except ValueError:
                kwargs["stdout"].write(raw + "\n")
                continue
            if event.get("type") == "assistant" and isinstance(event.get("message"), dict):
                event["message"].setdefault("model", command[command.index("--model") + 1])
                if command[command.index("--tools") + 1] == "Bash":
                    launcher = command[command.index("--allowedTools") + 1][5:-1]
                    for block in event["message"].get("content", []):
                        if block.get("type") == "tool_use" and block.get("name") == "Bash":
                            block["input"] = {"command": launcher + " probe"}
            kwargs["stdout"].write(json.dumps(event) + "\n")
        return type("Completed", (), {"returncode": 0})()

    with patch.object(bench, "execute_agent", side_effect=fake_claude), patch.object(bench.shutil, "which", return_value=sys.executable):
        out = root / "mcp-run"
        out.mkdir(mode=0o700)
        row = real_run_task(task, out, None, "fixed-model", mcp)
    assert row["passed"] is None and row["checks_passed"]
    assert row["sequence"] == ["phone_status", "phone_run_steps", "phone_run_steps"]
    command, kwargs = fake_calls[-1]
    assert command[command.index("--tools") + 1] == "ToolSearch,WaitForMcpServers"
    assert command[command.index("--allowedTools") + 1] == "mcp__alt__*"
    assert command[command.index("--permission-mode") + 1] == "dontAsk"
    assert json.loads(command[command.index("--settings") + 1]) == {"disableAllHooks": True}
    assert pathlib.Path(kwargs["cwd"]) != out and not pathlib.Path(kwargs["cwd"]).exists()
    context_path = out / "settings-ios-version.context.json"
    trace = out / "settings-ios-version.jsonl"
    assert "not-for-the-public-context" not in context_path.read_text()
    for artifact in [context_path, trace, out / "settings-ios-version.mcp.json"]:
        assert stat.S_IMODE(artifact.stat().st_mode) == 0o600
    proof = root / "bound-receipt.json"
    receipt = {"task": "settings-ios-version", "trace_sha256": row["trace_sha256"],
               "context_sha256": row["context_sha256"], "verified": True,
               "verifier": "fixture-verifier", "evidence": "synthetic postcondition"}
    proof.write_text(json.dumps(receipt))
    assert bench.assess(bench.parse_task(task)[0], trace, 0, proof, context_path)["passed"] is True
    try:
        bench.assess({**bench.parse_task(task)[0], "max_tool_calls": 100}, trace, 0, proof, context_path)
        raise AssertionError("changed task rules reused the old receipt")
    except ValueError:
        pass
    for changed in [{k: v for k, v in receipt.items() if k != "context_sha256"},
                    {**receipt, "context_sha256": "0" * 64}]:
        proof.write_text(json.dumps(changed))
        try:
            bench.assess(bench.parse_task(task)[0], trace, 0, proof, context_path)
            raise AssertionError("a receipt not bound to the backend context passed")
        except ValueError:
            pass
    proof.write_text(json.dumps(receipt))
    context_data = json.loads(context_path.read_text())
    context_path.write_text(json.dumps({**context_data, "backend_label": "another-backend"}))
    try:
        bench.assess(bench.parse_task(task)[0], trace, 0, proof, context_path)
        raise AssertionError("a relabeled backend inherited the old verification")
    except ValueError:
        pass
    context_path.write_text(json.dumps(context_data))

    discovery = root / "discovery.jsonl"
    discovery.write_text(json.dumps({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "search-1", "name": "ToolSearch", "input": {}}]}}) + "\n" +
                         json.dumps({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "search-1", "content": "ready"}]}}) + "\n" + current_trace["value"])
    discovery_context = root / "discovery.context.json"
    discovery_context.write_text(json.dumps({**context_data, "trace_sha256": hashlib.sha256(discovery.read_bytes()).hexdigest()}))
    result = bench.assess(bench.parse_task(task)[0], discovery, 0, context=discovery_context)
    assert result["checks_passed"] and result["sequence"][0] == "ToolSearch"
    assert result["tool_calls"] == 4, "framework discovery must count toward the task budget"

    # A successful final answer cannot hide tools from another backend.
    foreign = root / "foreign.jsonl"
    foreign.write_text(original.replace("mcp__iphone-use__", "mcp__other__"))
    foreign_context = root / "foreign.context.json"
    foreign_context.write_text(json.dumps({**context_data, "trace_sha256": hashlib.sha256(foreign.read_bytes()).hexdigest()}))
    result = bench.assess(bench.parse_task(task)[0], foreign, 0, context=foreign_context)
    assert result["passed"] is False and not result["checks_passed"]

    events = [json.loads(line) for line in lines if line != "not json at all"]
    for event in events:
        for block in (event.get("message") or {}).get("content", []):
            if block.get("type") == "tool_use":
                block["name"] = "Bash"
    current_trace["value"] = "\n".join(json.dumps(event) for event in events) + "\n"
    with patch.object(bench, "execute_agent", side_effect=fake_claude), patch.object(bench.shutil, "which", return_value=sys.executable), patch.dict(os.environ, {"OTHER_CLI_SESSION": "old-cloud-session"}):
        out = root / "cli-run"
        out.mkdir(mode=0o700)
        row = real_run_task(task, out, None, "fixed-model", cli)
    assert row["passed"] is None and row["checks_passed"]
    command, kwargs = fake_calls[-1]
    assert command[command.index("--tools") + 1] == "Bash"
    # --env-file overrides the inherited session and expands {run_dir}.
    assert kwargs["env"]["OTHER_CLI_SESSION"] == "old-cloud-session"
    assert "OTHER_CLI_HOME" not in kwargs["env"]
    launcher = out / "settings-ios-version.backend"
    assert launcher.is_file() and stat.S_IMODE(launcher.stat().st_mode) == 0o700
    assert str(launcher) in command[2] and str(launcher) in command[command.index("--allowedTools") + 1]
    assert json.loads((out / "settings-ios-version.mcp.json").read_text()) == {"mcpServers": {}}

    # Preparation is useful while no phone is available, but is never a pass.
    old_here = bench.HERE
    try:
        bench.HERE = root / "preparation"
        (bench.HERE / "tasks").mkdir(parents=True)
        (bench.HERE / "tasks/settings-ios-version.md").write_text(task.read_text())
        captured = io.StringIO()
        with patch.object(bench.subprocess, "run", side_effect=AssertionError("preparation executed a process")), contextlib.redirect_stdout(captured):
            assert bench.main(["--task", "settings-ios-version", "--cli", str(program),
                               "--instructions", str(reference), "--model", "fixed-model",
                               "--prepare-only", "--json"]) == 0
        prepared = json.loads(captured.getvalue())
        assert prepared[0]["state"] == "prepared" and prepared[0]["passed"] is None
        outputs = list((bench.HERE / "out").iterdir())
        assert len(outputs) == 1 and stat.S_IMODE(outputs[0].stat().st_mode) == 0o700
        assert not list(outputs[0].glob("*.jsonl")) and not (outputs[0] / "results.json").exists()
    finally:
        bench.HERE = old_here

record = bench.audit([
    json.dumps({"type": "system", "subtype": "init", "model": "model-A"}),
    json.dumps({"type": "result", "result": "done", "modelUsage": {"model-B": {}},
                "usage": {"cache_read_input_tokens": 30, "cache_creation_input_tokens": 20}}),
])
assert record["models_observed"] == ["model-A", "model-B"]
assert record["cache_read_input_tokens"] == 30 and record["cache_creation_input_tokens"] == 20

# Model identity is an execution condition, separate from device verification.
with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    header, _ = bench.parse_task(HERE / "agent-bench/tasks/settings-ios-version.md")
    baseline = (HERE / "fixtures/agent-bench-run.jsonl").read_text()
    for observed, requested, wanted, state in [
        (["pinned-model"], "pinned-model", True, "matched"),
        (["other-model"], "pinned-model", False, "mismatch"),
        (["pinned-model", "other-model"], "pinned-model", False, "mismatch"),
        ([], "pinned-model", None, "missing"),
        (["actual-default"], None, True, "not_requested"),
    ]:
        trace = root / "task.jsonl"
        prefix = "".join(json.dumps({"type": "assistant", "message": {"model": model, "content": []}}) + "\n" for model in observed)
        trace.write_text(prefix + baseline)
        context = root / "context.json"
        context.write_text(json.dumps({"task": header["name"], "task_header": header,
            "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(), "interface": "mcp",
            "tool_prefix": bench.TOOL_PREFIX, "framework_tools": [], "entrypoint_unchanged": True,
            "model_requested": requested}))
        receipt = root / "proof.json"
        receipt.write_text(json.dumps({"task": header["name"], "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(),
            "context_sha256": hashlib.sha256(context.read_bytes()).hexdigest(), "verified": True,
            "verifier": "synthetic-independent-postconditions", "evidence": "offline model identity control"}))
        checked = bench.assess(header, trace, 0, receipt, context)
        assert checked["passed"] is wanted, (observed, checked)
        assert checked["model_check"]["state"] == state
        assert checked["verification"]["state"] == "passed", "model checks must not rewrite independent device evidence"
        assert checked["comparison_eligible"] is False, "old contexts without runtime controls are not comparable"
    # A result-usage model also matters; it cannot be hidden by a matching init.
    trace.write_text(json.dumps({"type": "system", "subtype": "init", "model": "pinned-model"}) + "\n" +
                     json.dumps({"type": "result", "result": "iOS18.1", "modelUsage": {"other-model": {}}, "is_error": False}) + "\n")
    data = json.loads(context.read_text());data.update(trace_sha256=hashlib.sha256(trace.read_bytes()).hexdigest(), model_requested="pinned-model")
    context.write_text(json.dumps(data))
    checked = bench.assess(header, trace, 0, context=context)
    assert checked["passed"] is False and checked["model_check"]["state"] == "mismatch"

# Init is configured identity only; assistant and every usage event must be audited.
with tempfile.TemporaryDirectory() as tmp:
    root = pathlib.Path(tmp)
    header, _ = bench.parse_task(HERE / "agent-bench/tasks/settings-ios-version.md")
    for name, identities, result_usage, wanted in [
        ("assistant-drift", ["other-model"], {"pinned-model": {}}, False),
        ("init-only", [], {}, None),
        ("synthetic-only", ["<synthetic>"], {}, None),
        ("real-and-synthetic", ["pinned-model", "<synthetic>"], {}, True),
    ]:
        events = [{"type": "system", "subtype": "init", "model": "pinned-model"}]
        events += [{"type": "assistant", "message": {"model": model, "content": []}} for model in identities]
        events += [{"type": "result", "result": "iOS18.1", "modelUsage": result_usage, "is_error": False}]
        trace = root / "trace.jsonl"; trace.write_text("\n".join(map(json.dumps, events)) + "\n")
        context = root / "context.json"; context.write_text(json.dumps({"task": header["name"], "task_header": header,
            "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(), "interface": "mcp", "tool_prefix": bench.TOOL_PREFIX,
            "framework_tools": [], "entrypoint_unchanged": True, "model_requested": "pinned-model"}))
        receipt = root / "proof.json"; receipt.write_text(json.dumps({"task": header["name"],
            "trace_sha256": hashlib.sha256(trace.read_bytes()).hexdigest(), "context_sha256": hashlib.sha256(context.read_bytes()).hexdigest(),
            "verified": True, "verifier": "synthetic-postconditions", "evidence": "offline event identity test"}))
        checked = bench.assess(header, trace, 0, receipt, context)
        assert checked["passed"] is wanted, (name, checked)
        assert checked["comparison_eligible"] is False, "old contexts preserve task results only"
    earlier = bench.audit([json.dumps({"type": "result", "modelUsage": {"other-model": {}}}),
                           json.dumps({"type": "result", "modelUsage": {"pinned-model": {}}, "result": "iOS18.1"})])
    assert earlier["models_executed"] == ["other-model", "pinned-model"]
    assert earlier["result_events"] == 2 and earlier["is_error"], "concatenated results must not use only the last metrics"

print("agent-bench: audit, interfaces, model identity, context binding and private artifacts ok")
