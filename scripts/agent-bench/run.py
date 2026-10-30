#!/usr/bin/env python3
"""Run real agent tasks against iphone-use with `claude -p` and audit them.

Each task is a Markdown file in tasks/ with a small header:

    ---
    name: settings-ios-version
    expect: "iOS \\d+"          # answer format only (optional)
    max_tool_calls: 8            # budget the run is judged against (optional)
    ---
    Open Settings on my iPhone and tell me which iOS version it runs.

The driver gives Claude only the iphone-use MCP server (the installed one by
default), records Claude's stream JSONL, and reports per task: checks, verification, tool
calls by name, failed tool calls, turns, wall time, tokens and cost. This tells
whether the tool descriptions lead a model to the short path, the way
XcodeBuildMCP's and mobile-mcp's agent benchmarks do.

It drives a real phone and spends model tokens, so it is manual, not CI:

    scripts/agent-bench/run.py                 # every task
    scripts/agent-bench/run.py --task settings-ios-version --model claude-sonnet-5-5
    scripts/agent-bench/run.py --from-jsonl out/…/settings-ios-version.jsonl   # re-audit a run

Artifacts go to scripts/agent-bench/out/<timestamp>/ (git-ignored).
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import shutil
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_MCP = pathlib.Path.home() / "Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
TOOL_PREFIX = "mcp__iphone-use__"
MCP_FRAMEWORK_TOOLS = ["ToolSearch", "WaitForMcpServers"]


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


def audit(lines, returncode=0, tool_prefix=TOOL_PREFIX):
    """Summarize one stream-json run (an iterable of JSONL lines). A run
    without a final `result` event, or whose known exit code is non-zero, is
    an error. None means the original exit code was not supplied."""
    calls, failed, names, full_names = [], [], {}, []
    pending = {}
    result = {}
    models = set()
    executed_models = set()
    model_sources = {"init": set(), "assistant": set(), "usage": set()}
    result_events = 0
    for raw in lines:
        raw = raw.strip()
        if not raw:
            continue
        try:
            event = json.loads(raw)
        except ValueError:
            continue
        kind = event.get("type")
        if kind == "system" and event.get("subtype") == "init" and isinstance(event.get("model"), str):
            model_sources["init"].add(event["model"])
        content = (event.get("message") or {}).get("content") or []
        if kind == "assistant":
            message_model = (event.get("message") or {}).get("model")
            if isinstance(message_model, str) and message_model and message_model != "<synthetic>":
                model_sources["assistant"].add(message_model)
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    name = block.get("name", "")
                    full_names.append(name)
                    short = name[len(tool_prefix):] if tool_prefix and name.startswith(tool_prefix) else name
                    calls.append(short)
                    names[short] = names.get(short, 0) + 1
                    pending[block.get("id")] = short
        elif kind == "user":
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("is_error"):
                    failed.append(pending.get(block.get("tool_use_id"), "?"))
        elif kind == "result":
            result_events += 1
            result = event
            if isinstance(event.get("modelUsage"), dict):
                model_sources["usage"].update(name for name in event["modelUsage"] if name != "<synthetic>")
    usage = result.get("usage") or {}
    models.update(*(model_sources.values()))
    executed_models.update(model_sources["assistant"], model_sources["usage"])
    return {
        "tool_calls": len(calls),
        "tools": names,
        "sequence": calls,
        "full_tool_names": full_names,
        "failed_calls": failed,
        "turns": result.get("num_turns"),
        "duration_ms": result.get("duration_ms"),
        "cost_usd": result.get("total_cost_usd"),
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "cache_read_input_tokens": usage.get("cache_read_input_tokens"),
        "cache_creation_input_tokens": usage.get("cache_creation_input_tokens"),
        "models_observed": sorted(models),
        "models_executed": sorted(executed_models),
        "model_identity_sources": {key: sorted(values) for key, values in model_sources.items()},
        "result_events": result_events,
        "answer": result.get("result", ""),
        "is_error": bool(result.get("is_error")) or result_events != 1 or returncode not in (None, 0),
        "process_returncode": returncode,
    }


def judge(header, summary):
    """Check execution, answer format and call budget, not physical completion."""
    reasons = []
    if summary["is_error"]:
        reasons.append("claude reported an error, exited non-zero or did not give exactly one final result")
    expect = header.get("expect")
    if expect and not re.search(expect, summary["answer"] or ""):
        reasons.append(f"answer does not match {expect!r}")
    budget = header.get("max_tool_calls")
    if isinstance(budget, int) and summary["tool_calls"] > budget:
        reasons.append(f"{summary['tool_calls']} tool calls > budget {budget}")
    return not reasons, reasons


def assess(header, trace, returncode=None, verification=None, context=None):
    """Only an independent, trace-bound receipt can verify task postconditions.

    The model's answer, regex match and tool acknowledgements are not receipts.
    Receipt loading never executes code or takes instructions from the trace.
    """
    captured_context, context_digest = None, None
    if context:
        data = pathlib.Path(context).read_bytes()
        captured_context = json.loads(data)
        if not isinstance(captured_context, dict) or captured_context.get("task") != header["name"]:
            raise ValueError("run context belongs to another task")
        if captured_context.get("task_header") != header:
            raise ValueError("task rules differ from the captured run")
        interface, prefix = captured_context.get("interface"), captured_context.get("tool_prefix")
        if interface not in ("mcp", "cli") or (interface == "mcp" and
                (not isinstance(prefix, str) or not re.fullmatch(r"mcp__[A-Za-z0-9_-]+__", prefix))):
            raise ValueError("invalid run context interface or tool prefix")
        framework = captured_context.get("framework_tools", [])
        if not isinstance(framework, list) or any(v not in MCP_FRAMEWORK_TOOLS for v in framework):
            raise ValueError("invalid run context framework tools")
        context_digest = hashlib.sha256(data).hexdigest()
    digest = hashlib.sha256()
    with open(trace, "rb") as source:
        def captured_lines():
            for line in source:
                digest.update(line)
                yield line.decode("utf-8")
        summary = audit(captured_lines(), returncode,
                        captured_context.get("tool_prefix") if captured_context else TOOL_PREFIX)
    fingerprint = digest.hexdigest()
    if captured_context and captured_context.get("trace_sha256") != fingerprint:
        raise ValueError("run context does not match this trace")
    checks_passed, reasons = judge(header, summary)
    if captured_context:
        prefix = captured_context.get("tool_prefix")
        interface = captured_context.get("interface")
        unexpected = [name for name in summary["full_tool_names"]
                      if not ((name.startswith(prefix) or name in framework) if interface == "mcp" else name == "Bash")]
        if unexpected:
            checks_passed = False
            reasons.append("trace used tools outside the captured backend")
        if captured_context.get("entrypoint_unchanged") is not True:
            checks_passed = False
            reasons.append("backend entrypoint changed during the run")
    requested_model = captured_context.get("model_requested") if captured_context else None
    if requested_model is not None and (not isinstance(requested_model, str) or not requested_model.strip()):
        raise ValueError("invalid requested model in run context")
    observed_models = summary["models_observed"]
    model_check = {"requested": requested_model, "observed": observed_models,
                   "executed": summary["models_executed"], "state": "not_requested"}
    if requested_model is not None:
        if not observed_models:
            model_check["state"] = "missing"
        elif observed_models == [requested_model]:
            model_check["state"] = "matched" if summary["models_executed"] else "missing"
        else:
            model_check["state"] = "mismatch"
            checks_passed = False
            reasons.append("reported model IDs differ from the requested exact model pin")
    model_confirmed = model_check["state"] in ("matched", "not_requested")
    proof = {"state": "unverified"}
    if verification:
        with open(verification, encoding="utf-8") as source:
            receipt = json.load(source)
        if not isinstance(receipt, dict):
            raise ValueError("verification receipt must be an object")
        if receipt.get("task") != header["name"]:
            raise ValueError("verification receipt belongs to another task")
        if receipt.get("trace_sha256") != fingerprint:
            raise ValueError("verification receipt does not match this trace")
        if context_digest and receipt.get("context_sha256") != context_digest:
            raise ValueError("verification receipt does not match this run context")
        if type(receipt.get("verified")) is not bool:
            raise ValueError("receipt verified must be a boolean")
        for field in ("verifier", "evidence"):
            if not isinstance(receipt.get(field), str) or not receipt[field].strip():
                raise ValueError(f"receipt {field} must name the independent check")
        proof = {key: receipt[key] for key in
                 ("task", "trace_sha256", "verifier", "evidence")}
        proof["state"] = "passed" if receipt["verified"] else "failed"
    if not checks_passed or proof["state"] == "failed":
        passed = False
        if proof["state"] == "failed":
            reasons.append("independent task verification failed")
    elif proof["state"] == "passed" and returncode == 0 and model_confirmed:
        passed = True
    else:
        passed = None
        if proof["state"] != "passed":
            reasons.append("task postconditions have not been independently verified")
        if returncode is None:
            reasons.append("the original process exit status was not supplied")
        if not model_confirmed:
            reasons.append("requested model identity was not observed; controlled comparison is unverified")
    return {"schema_version": 2, "task": header["name"], **summary, "passed": passed,
            "trace_sha256": fingerprint,
            "context": captured_context, "context_sha256": context_digest,
            "checks_passed": checks_passed, "verification": proof, "reasons": reasons,
            "model_check": model_check,
            "comparison_eligible": passed is True and model_check["state"] == "matched"}


def private_json(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as sink:
        json.dump(value, sink, ensure_ascii=False, indent=2)


def load_env_file(path):
    """A backend's own environment for a trial: a JSON object of strings.

    Kept outside the repository with the backend's other private settings;
    `{run_dir}` in a value becomes the run's private directory, so a CLI gets a
    fresh home per run instead of an existing profile or session.
    """
    if not path:
        return {}
    data = json.loads(pathlib.Path(path).read_text())
    if not isinstance(data, dict) or not all(
            isinstance(k, str) and re.fullmatch(r"[A-Z_][A-Z0-9_]*", k) and isinstance(v, str)
            for k, v in data.items()):
        raise ValueError("--env-file must be a JSON object of UPPER_CASE names to strings")
    return data


def backend_spec(mcp, config=None, cli=None, reference=None, label=None, env=None):
    """Prepare native interfaces without starting an MCP server or a phone CLI."""
    instructions = pathlib.Path(reference).read_text() if reference else ""
    env = dict(env or {})
    if cli:
        executable = pathlib.Path(cli).expanduser().resolve()
        # A literal Bash allow rule must not contain glob or shell syntax.
        if not re.fullmatch(r"[/A-Za-z0-9_.-]+", str(executable)):
            raise ValueError("CLI path must be absolute without whitespace or shell/glob characters")
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise ValueError("CLI executable is missing or not executable")
        return {"interface": "cli", "label": label or executable.name,
                "config": {"mcpServers": {}}, "tool_prefix": None,
                "tools": "Bash", "framework_tools": [],
                "allowed": [f"Bash({executable})", f"Bash({executable} *)"],
                "entrypoint": str(executable), "reference": instructions, "env": env}
    if config:
        data = json.loads(pathlib.Path(config).read_text())
        servers = data.get("mcpServers") if isinstance(data, dict) else None
        if not isinstance(servers, dict) or len(servers) != 1:
            raise ValueError("benchmark MCP config must contain exactly one server")
        name, server = next(iter(servers.items()))
        if not re.fullmatch(r"[A-Za-z0-9_-]+", name) or not isinstance(server, dict):
            raise ValueError("invalid MCP server name or configuration")
        if server.get("type", "stdio") != "stdio" or not isinstance(server.get("command"), str) or not server["command"]:
            raise ValueError("benchmark MCP config must name a local stdio command")
        if not isinstance(server.get("args", []), list) or not all(isinstance(v, str) for v in server.get("args", [])):
            raise ValueError("MCP args must be strings")
        environment = server.get("env", {})
        if not isinstance(environment, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in environment.items()):
            raise ValueError("MCP environment must contain string values")
        executable = shutil.which(server["command"], path=environment.get("PATH"))
        if not executable:
            raise ValueError("MCP executable is missing")
        # Preserve a virtualenv's symlink path, while removing PATH ambiguity.
        executable = os.path.abspath(executable)
        server["command"] = executable
    else:
        name = "iphone-use"
        executable = str(pathlib.Path(mcp).expanduser().resolve())
        if not pathlib.Path(executable).is_file() or not os.access(executable, os.X_OK):
            raise ValueError("MCP executable is missing or not executable")
        data = {"mcpServers": {name: {"command": executable}}}
    server = data["mcpServers"][name]
    server["env"] = {**server.get("env", {}), "DO_NOT_TRACK": "1",
                     "IPHONE_USE_ANALYTICS": "0", "IPHONE_USE_TELEMETRY": "0"}
    return {"interface": "mcp", "label": label or name, "config": data,
            "tool_prefix": f"mcp__{name}__", "tools": ",".join(MCP_FRAMEWORK_TOOLS),
            "framework_tools": MCP_FRAMEWORK_TOOLS,
            "allowed": [f"mcp__{name}__*", *MCP_FRAMEWORK_TOOLS],
            "entrypoint": executable, "reference": instructions, "env": env}


def task_invocation(path, out_dir, mcp, model, backend=None):
    header, prompt = parse_task(path)
    if not re.fullmatch(r"[A-Za-z0-9_-]+", header["name"]):
        raise ValueError("task name must be a safe filename")
    backend = backend or backend_spec(mcp)
    config = out_dir / f"{header['name']}.mcp.json"
    private_json(config, backend["config"])
    jsonl = out_dir / f"{header['name']}.jsonl"
    task_prompt = prompt
    if backend["reference"]:
        prompt += "\n\nBackend API reference:\n" + backend["reference"]
    if backend["interface"] == "cli":
        prompt += f"\nUse this native executable for shell calls: {backend['entrypoint']}"
    prompt += ("\n\nUse only the already configured, authorized local test phone. "
               "Do not install or reconfigure software, rent cloud devices, send messages, pay, "
               "or delete user data. Stop if the configured phone is unavailable.")
    command = [
        "claude", "-p", prompt,
        "--output-format", "stream-json", "--verbose",
        "--mcp-config", str(config), "--strict-mcp-config",
        "--tools", backend["tools"], "--allowedTools", *backend["allowed"],
        "--permission-mode", "dontAsk", "--setting-sources", "user",
        "--settings", '{"disableAllHooks":true}',
    ]
    if model:
        command += ["--model", model]
    return header, backend, config, jsonl, task_prompt, command


def run_task(path, out_dir, mcp, model, backend=None):
    header, backend, config, jsonl, task_prompt, command = task_invocation(path, out_dir, mcp, model, backend)
    entrypoint_before = hashlib.sha256(pathlib.Path(backend["entrypoint"]).read_bytes()).hexdigest()
    started = time.monotonic()
    child = os.environ.copy()
    # The backend's own settings (--env-file), e.g. a fresh home under this
    # run's directory, so a trial never reuses a profile or a cloud session.
    run_env = {k: v.replace("{run_dir}", str(out_dir)) for k, v in backend.get("env", {}).items()}
    child.update(run_env)
    fd = os.open(jsonl, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as sink, tempfile.TemporaryDirectory(prefix="iphone-bench-work-") as workspace:
        proc = subprocess.run(command, stdout=sink, stderr=subprocess.STDOUT, cwd=workspace, env=child, check=False)
    wall_ms = int((time.monotonic() - started) * 1000)
    context = {"task": header["name"], "trace_sha256": hashlib.sha256(jsonl.read_bytes()).hexdigest(),
               "task_header": header,
               "driver_sha256": hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
               "backend_label": backend["label"], "interface": backend["interface"],
               "tool_prefix": backend["tool_prefix"], "allowed_tools": backend["allowed"],
               "framework_tools": backend["framework_tools"],
               "builtin_tools": backend["tools"], "model_requested": model,
               "task_prompt_sha256": hashlib.sha256(task_prompt.encode()).hexdigest(),
               "reference_sha256": hashlib.sha256(backend["reference"].encode()).hexdigest(),
               "env_sha256": hashlib.sha256(json.dumps(backend.get("env", {}), sort_keys=True).encode()).hexdigest(),
               "mcp_config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
               "entrypoint_sha256": entrypoint_before,
               "entrypoint_unchanged": entrypoint_before == hashlib.sha256(pathlib.Path(backend["entrypoint"]).read_bytes()).hexdigest(),
               "cli_config_and_state_isolated": backend["interface"] == "cli",
               "agent_policy_requested": {"permission_mode": "dontAsk", "settings_sources": "user",
                                          "disable_all_hooks": True},
               "clipboard_restore_requested": backend["interface"] == "cli"}
    context_path = out_dir / f"{header['name']}.context.json"
    private_json(context_path, context)
    result = assess(header, jsonl, proc.returncode, context=context_path)
    result["wall_ms"] = wall_ms
    return result


def print_report(rows):
    for row in rows:
        mark = "UNVERIFIED" if row["passed"] is None else "PASS" if row["passed"] else "FAIL"
        elapsed = row.get("duration_ms")
        if elapsed is None:
            elapsed = row.get("wall_ms")
        timing = f"{elapsed / 1000:.1f} s" if elapsed is not None else "duration unknown"
        cost = row.get("cost_usd")
        print(f"{mark} {row['task']}: {row['tool_calls']} tool calls, "
              f"{len(row['failed_calls'])} failed, {row.get('turns')} turns, {timing}"
              + (f", ${cost:.4f}" if isinstance(cost, (int, float)) else ""))
        print(f"     sequence: {' → '.join(row['sequence']) or '(none)'}")
        print(f"     model: {row.get('model_check', {}).get('state', 'unknown')}; "
              f"controlled comparison: {'eligible' if row.get('comparison_eligible') else 'not eligible'}")
        for reason in row["reasons"]:
            print(f"     ✗ {reason}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--task", action="append", help="task name (tasks/<name>.md); repeatable")
    parser.add_argument("--model", help="exact Claude model id for a controlled comparison")
    interface = parser.add_mutually_exclusive_group()
    interface.add_argument("--mcp", help="iphone-use MCP executable (default: installed binary)")
    interface.add_argument("--mcp-config", help="explicit single-server stdio MCP configuration")
    interface.add_argument("--cli", help="native local phone CLI executable; exposes Bash only")
    parser.add_argument("--instructions", help="backend API reference to include in the task prompt")
    parser.add_argument("--label", help="backend label recorded with the trace")
    parser.add_argument("--env-file", help="private JSON of the backend's environment ({run_dir} expands per run)")
    parser.add_argument("--prepare-only", action="store_true", help="write private launch configuration without starting an agent or phone tool")
    parser.add_argument("--from-jsonl", help="re-audit a saved run instead of running Claude")
    parser.add_argument("--verification", help="independent JSON receipt bound to --from-jsonl")
    parser.add_argument("--returncode", type=int, help="original process exit code for --from-jsonl")
    parser.add_argument("--context", help="captured run context for --from-jsonl (otherwise infer sibling file)")
    parser.add_argument("--json", action="store_true", help="print machine-readable results")
    args = parser.parse_args(argv)
    if args.model is not None and not args.model.strip():
        parser.error("--model must name an exact reported model ID")
    if args.task and any(not re.fullmatch(r"[A-Za-z0-9_-]+", name) for name in args.task):
        parser.error("task names must be safe filenames")
    replay = args.from_jsonl is not None
    if replay and not args.from_jsonl.strip():
        parser.error("--from-jsonl must name a trace file")
    if args.verification is not None and not args.verification.strip():
        parser.error("--verification must name a receipt file")
    if args.verification is not None and not replay:
        parser.error("--verification requires --from-jsonl")
    if args.returncode is not None and not replay:
        parser.error("--returncode requires --from-jsonl")
    if args.context is not None and not replay:
        parser.error("--context requires --from-jsonl")
    if replay and any([args.mcp, args.mcp_config, args.cli, args.instructions, args.label, args.env_file, args.model, args.prepare_only]):
        parser.error("replay uses the captured context, not new backend or model settings")

    if replay:
        if args.task and len(args.task) != 1:
            parser.error("re-auditing needs exactly one --task")
        name = args.task[0] if args.task else pathlib.Path(args.from_jsonl).stem
        task = HERE / "tasks" / f"{name}.md"
        if not task.is_file():
            parser.error("no matching task header; provide --task NAME")
        header, prompt = parse_task(task)
        try:
            context = args.context
            sibling = pathlib.Path(args.from_jsonl).with_suffix(".context.json")
            if context is None and sibling.is_file():
                context = sibling
            if context and json.loads(pathlib.Path(context).read_text()).get("task_prompt_sha256") != hashlib.sha256(prompt.encode()).hexdigest():
                raise ValueError("task prompt differs from the captured run")
            rows = [assess(header, args.from_jsonl, args.returncode, args.verification, context)]
        except (OSError, ValueError) as error:
            parser.error(str(error))
    else:
        tasks = sorted((HERE / "tasks").glob("*.md"))
        if args.task:
            tasks = [HERE / "tasks" / f"{name}.md" for name in args.task]
        if not tasks:
            parser.error("no benchmark tasks were selected")
        if len(set(tasks)) != len(tasks) or any(not task.is_file() for task in tasks):
            parser.error("tasks must exist and must not be selected twice")
        if (args.mcp_config or args.cli) and not args.model:
            parser.error("alternative backends require an explicit --model for controlled comparisons")
        if args.cli and not args.instructions:
            parser.error("native CLI trials require --instructions with the backend API reference")
        mcp = args.mcp or os.environ.get("IPHONE_USE_MCP", str(DEFAULT_MCP))
        try:
            backend = backend_spec(mcp, args.mcp_config, args.cli, args.instructions, args.label,
                                   load_env_file(args.env_file))
            parent = HERE / "out"
            parent.mkdir(parents=True, exist_ok=True)
            out_dir = pathlib.Path(tempfile.mkdtemp(prefix=datetime.datetime.now().strftime("%Y%m%d-%H%M%S-"), dir=parent))
            if args.prepare_only:
                prepared = []
                for task in tasks:
                    header, _, config, _, _, command = task_invocation(task, out_dir, mcp, args.model, backend)
                    prepared.append({"task": header["name"], "state": "prepared", "passed": None,
                                     "backend_label": backend["label"], "interface": backend["interface"],
                                     "mcp_config_sha256": hashlib.sha256(config.read_bytes()).hexdigest(),
                                     "command": command})
                private_json(out_dir / "prepared.json", prepared)
                if args.json:
                    print(json.dumps(prepared, ensure_ascii=False, indent=2))
                else:
                    print(f"PREPARED {len(prepared)} task(s): {out_dir}; no agent or phone tool started")
                return 0
            rows = [run_task(task, out_dir, mcp, args.model, backend) for task in tasks]
            private_json(out_dir / "results.json", rows)
        except (OSError, ValueError) as error:
            parser.error(str(error))
    if args.json:
        print(json.dumps(rows, ensure_ascii=False, indent=2))
    else:
        print_report(rows)
    if any(row["passed"] is False for row in rows):
        return 1
    return 0 if all(row["passed"] is True for row in rows) else 3


if __name__ == "__main__":
    sys.exit(main())
