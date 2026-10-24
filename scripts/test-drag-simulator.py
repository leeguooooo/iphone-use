#!/usr/bin/env python3
"""Verify OS-delivered drag/hover events in a disposable simulator fixture.

Requires an already-running simulator daemon. It refuses physical targets,
never retries a gesture, and reads the fixture's independent UI counters.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import re
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from runner_auth import authorization

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = "com.leeguoo.iphone-use.gesture-fixture"


def command(argv, env=None, check=True, timeout=180):
    result = subprocess.run([str(x) for x in argv], env=env, check=False,
                            capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"{argv[0]} exit {result.returncode}:\n{result.stdout}{result.stderr}")
    return result


def judge_multi_hold(value, nonce):
    if not isinstance(value, dict) or value.get("run_id") != nonce:
        return ["no fresh independent multi-finger result"]
    errors = []
    for key, expected in [("down_count", 3), ("up_count", 3), ("cancel_count", 0), ("max_fingers", 3)]:
        if type(value.get(key)) is not int or value[key] != expected:
            errors.append(f"{key} must be {expected}")
    holds = value.get("holds")
    if not isinstance(holds, list) or len(holds) != 3:
        return errors + ["three independent completed holds are required"]
    remaining = [(0.3, 0.4), (0.5, 0.4), (0.7, 0.4)]
    for hold in holds:
        if not isinstance(hold, dict):
            errors.append("invalid finger record")
            continue
        duration = hold.get("duration_ms")
        if type(duration) is not int or not 1350 <= duration <= 2500:
            errors.append("each finger must hold for the requested duration")
        def close(point, expected):
            return isinstance(point, list) and len(point) == 2 and all(
                type(v) in (int, float) and math.isfinite(v) and abs(v - wanted) <= 0.015
                for v, wanted in zip(point, expected))
        match = next((point for point in remaining if close(hold.get("start"), point)
                      and close(hold.get("end"), point)), None)
        if match is None:
            errors.append("finger moved, repeated a point, or touched the wrong position")
        else:
            remaining.remove(match)
    return errors


def judge(value, nonce, hover, kind="path"):
    """Transport success is insufficient: every assertion comes from UIKit."""
    if kind.startswith("multi-"):
        return judge_multi_hold(value, nonce)
    stationary, native = kind == "longpress", kind == "native"
    errors = []
    if not isinstance(value, dict):
        return ["no independent fixture result"]
    if value.get("run_id") != nonce:
        errors.append("stale fixture launch")
    for key, expected in [("down_count", 1), ("up_count", 1), ("cancel_count", 0), ("max_fingers", 1)]:
        if type(value.get(key)) is not int or value[key] != expected:
            errors.append(f"{key} must be {expected}")
    expected_visits = ["start"] if stationary else (["start", "end"] if native else ["start", "edge", "end"])
    if value.get("visits") != expected_visits:
        errors.append("waypoints were not visited in order")
    dwell = value.get("hover_ms")
    if type(dwell) is not int or dwell < 0 or (dwell >= 800) != hover:
        errors.append("edge hold was not observed" if hover else "slow drift falsely counted as a hold")
    duration = value.get("duration_ms")
    initial = value.get("initial_hold_ms")
    if type(duration) is not int or duration < (550 if stationary else (650 if native else (1750 if hover else 2950))):
        errors.append("touch duration was not observed")
    low, high = (0, 150) if native else (350, 600)
    if not stationary and (type(initial) is not int or not low <= initial <= high):
        errors.append("initial hold was not observed before motion")
    expected_end = [0.3, 0.4] if stationary else [0.5, 0.4]
    for key, expected in [("start", [0.3, 0.4]), ("end", expected_end)]:
        point = value.get(key)
        if not isinstance(point, list) or len(point) != 2 or any(
            isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v)
            or abs(v - wanted) > 0.015 for v, wanted in zip(point or [], expected)
        ):
            errors.append(f"wrong {key} position")
    return errors


def require_matching_viewport(fixture, runner_window):
    size = runner_window.get("value", {})
    for key in ("width", "height"):
        actual, expected = fixture.get(key), size.get(key)
        if (type(actual) not in (int, float) or type(expected) not in (int, float)
                or not math.isfinite(actual) or not math.isfinite(expected)
                or actual <= 0 or abs(actual - expected) > 1):
            raise RuntimeError(f"fixture {key} does not match Runner screen; check app compatibility mode; no gesture sent")


def build_fixture(out):
    # Let Xcode process Info.plist and platform metadata too. A hand-linked
    # app can run in a compatibility-sized viewport on older iOS runtimes.
    derived = out / "fixture-build"
    built = command(["xcodebuild", "-project", ROOT / "runner/gesture-fixture/GestureFixture.xcodeproj",
                     "-scheme", "GestureFixture", "-configuration", "Release", "-sdk", "iphonesimulator",
                     "-destination", "generic/platform=iOS Simulator", "-derivedDataPath", derived,
                     "-jobs", "2", "CODE_SIGNING_ALLOWED=NO", "build"], check=False, timeout=180)
    (out / "fixture-build.log").write_text(built.stdout + built.stderr)
    if built.returncode:
        raise RuntimeError("fixture Xcode build failed; see fixture-build.log")
    app = out / "GestureFixture.app"
    shutil.copytree(derived / "Build/Products/Release-iphonesimulator/GestureFixture.app", app)
    command(["codesign", "--force", "--sign", "-", app])
    return app


class Daemon:
    def __init__(self, base, owner):
        parsed = urllib.parse.urlparse(base)
        if parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "localhost", "::1"):
            raise ValueError("only a loopback simulator daemon may be tested")
        if parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError("unexpected daemon URL components")
        self.base, self.owner = base.rstrip("/"), owner

    def call(self, path, body=None):
        headers = {"X-Phone-Owner": self.owner}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers.update({"Content-Type": "application/json", "X-Phone-Control": "1"})
        request = urllib.request.Request(self.base + path, data=data, headers=headers)
        with urllib.request.urlopen(request, timeout=40) as response:
            return json.load(response)

    def require_simulator(self, udid):
        status = self.call("/agent/status")
        if status.get("target") != "simulator" or str(status.get("udid", "")).lower() != udid.lower():
            raise RuntimeError("daemon does not target the requested simulator; no gesture sent")
        if status.get("drivable") is not True:
            raise RuntimeError("simulator is not drivable")
        if status.get("owner") not in (None, self.owner):
            raise RuntimeError("simulator belongs to another owner")
        return status

    def metrics(self, nonce, identifier="gesture-result"):
        tree = self.call("/agent/elements")
        matches = []
        for row in tree.get("elements", []):
            if row.get("identifier") == identifier:
                try:
                    record = json.loads(row["label"])
                    if record.get("run_id") == nonce:
                        matches.append(record)
                except (ValueError, KeyError, TypeError):
                    continue
        if matches and all(row == matches[0] for row in matches):
            return matches[0]
        return None

    def wait_metrics(self, nonce, ended=False, identifier="gesture-result"):
        until = time.monotonic() + 15
        last = None
        while time.monotonic() < until:
            last = self.metrics(nonce, identifier)
            if last is not None and (not ended or last.get("up_count", 0) > 0):
                return last
            time.sleep(0.2)
        return last


def runner_call(args, target, body=None):
    record = json.loads(args.runner_record.read_text())
    if str(record.get("udid", "")).lower() != args.udid.lower():
        raise RuntimeError("runner record belongs to another simulator")
    port = record.get("port")
    if type(port) is not int or not 1 <= port <= 65535:
        raise RuntimeError("runner record has no valid port")
    # This disposable instance's token only; never runner_auth.token(), whose
    # default may belong to a personal phone. No token or auth header is logged.
    token = (args.runner_record.parent / "runner-token").read_text().strip()
    if not re.fullmatch(r"[0-9a-f]{32,128}", token):
        raise RuntimeError("invalid disposable runner token")
    data = None if body is None else json.dumps(body).encode()
    method = "GET" if body is None else "POST"
    request = urllib.request.Request(f"http://127.0.0.1:{port}" + target, data=data,
        headers={"Content-Type": "application/json", "Authorization": authorization(token, method, target, data or b"")})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response), response.headers


def native_swipe(args, before):
    body = {"x1": before["width"] * 0.3, "y1": before["height"] * 0.4,
            "x2": before["width"] * 0.5, "y2": before["height"] * 0.4, "duration_ms": 700}
    value, headers = runner_call(args, "/swipe", body)
    gesture = headers.get("X-IPU-Gesture")
    return ({"ok": "value" in value and value["value"] is None and gesture == "synthesized",
             "native": True, "gesture": gesture}, body)


def localized_settings_suite(daemon, args):
    daemon.require_simulator(args.udid)
    reply = daemon.call("/agent/input", {"type": "launch_app", "bundle": "com.apple.Preferences", "fresh": True})
    if reply.get("ok") is not True:
        raise RuntimeError("Settings launch was not acknowledged; no retry")
    until = time.monotonic() + 15
    while time.monotonic() < until:
        active, _ = runner_call(args, "/apps/active")
        source, headers = runner_call(args, "/source?format=json&max_depth=1&max_nodes=1&extension_calls=0")
        active, root = active.get("value", {}), source.get("value", {})
        label = root.get("label") or root.get("name")
        if (active.get("bundleId") == "com.apple.Preferences" and active.get("pid")
                and str(active["pid"]) == headers.get("X-IPU-Pid")
                and root.get("type") == "XCUIElementTypeApplication" and isinstance(label, str) and label):
            binding = {"bundle": active["bundleId"], "pid": active["pid"], "label": label, "udid": args.udid}
            args.settings_suite.write_text(json.dumps({"suite": "Simulator smoke", "app": "com.apple.Preferences",
                "cases": [{"name": "Settings opens", "steps": [{"kind": "pause", "ms": 1500}],
                           "assert": [{"application": label, "timeout_ms": 15000}]}]}))
            return binding
        time.sleep(0.2)
    raise RuntimeError("Settings bundle/PID/source label did not agree")


def case(daemon, args, label, carrier, hover):
    out = args.out / label
    out.mkdir()
    nonce = uuid.uuid4().hex
    daemon.require_simulator(args.udid)
    command(["xcrun", "simctl", "terminate", args.udid, BUNDLE], check=False, timeout=30)
    env = {k: v for k, v in os.environ.items() if not k.startswith("SIMCTL_CHILD_")}
    env["SIMCTL_CHILD_GESTURE_RUN_ID"] = nonce
    command(["xcrun", "simctl", "launch", args.udid, BUNDLE], env=env, timeout=60)
    before = daemon.wait_metrics(nonce)
    if not before or any(before.get(k) != 0 for k in ("down_count", "up_count", "cancel_count")):
        raise RuntimeError("fixture is not fresh and ready; no gesture sent")
    runner_window, _ = runner_call(args, "/window/size")
    (out / "result.json").write_text(json.dumps({"case": label, "passed": False,
        "stage": "viewport_preflight", "before": before, "runner_window": runner_window}, indent=2) + "\n")
    require_matching_viewport(before, runner_window)
    command(["xcrun", "simctl", "io", args.udid, "screenshot", out / "before.png"], timeout=30)
    action = {"type": "drag", "hold_ms": 400, "duration_ms": 300 if hover else 900,
              "points": [{"x": 0.3, "y": 0.4}, {"x": 0.13, "y": 0.5, "dwell_ms": 900 if hover else 0}]}
    if not hover:
        action["points"].append({"x": 0.15, "y": 0.5})
    action["points"].append({"x": 0.5, "y": 0.4})
    if carrier == "longpress":
        action = {"type": "longpress", "x": 0.3, "y": 0.4, "duration_ms": 600}
    if carrier.startswith("multi-"):
        action = {"type": "multi_longpress", "duration_ms": 1500,
                  "points": [{"x": x, "y": 0.4} for x in (0.3, 0.5, 0.7)]}
    transport = carrier.removeprefix("multi-")
    daemon.require_simulator(args.udid)
    reply, reply_error = None, None
    # One send only, even on a lost reply. The observation below may still
    # prove that the gesture landed, but no part of it is resent.
    try:
        if carrier == "native":
            reply, action = native_swipe(args, before)
        elif transport == "flow":
            step = {k: v for k, v in action.items() if k != "type"}
            step["kind"] = action["type"]
            flow = out / "drag.json"
            flow.write_text(json.dumps({"version": 1, "name": "Fixture drag", "risk": "navigation", "steps": [step]}))
            child = {k: v for k, v in os.environ.items() if not k.startswith(("IPHONE_USE_", "PHONE_REMOTE_", "IPU_"))}
            child.update({"IPHONE_USE_URL": daemon.base, "IPHONE_USE_OWNER": daemon.owner,
                          "IPHONE_USE_INSTANCE": "fixture-" + nonce[:12],
                          "IPHONE_USE_FLOWS_DIR": str(out / "flows"), "IPHONE_USE_FLOWS_NO_AUTO_UPDATE": "1"})
            ran = command([args.mcp, "flow", "run", flow], env=child, check=False, timeout=90)
            (out / "flow.log").write_text(ran.stdout + ran.stderr)
            reply = json.loads(ran.stdout)
            if ran.returncode:
                reply_error = f"flow exit {ran.returncode}"
        else:
            reply = daemon.call("/agent/input" if transport in ("single", "longpress") else "/agent/actions",
                                action if transport in ("single", "longpress") else {"steps": [{"kind": "action", "action": action}]})
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        reply_error = str(error)
    after = daemon.wait_metrics(nonce, ended=True)
    command(["xcrun", "simctl", "io", args.udid, "screenshot", out / "after.png"], timeout=30)
    errors = judge(after, nonce, hover, kind=carrier)
    if reply_error or not isinstance(reply, dict) or reply.get("ok") is not True:
        errors.append("gesture acknowledgement failed: " + str(reply_error or reply))
    report = {"case": label, "carrier": carrier, "expected_hover": hover, "passed": not errors, "errors": errors, "before": before,
              "after": after, "runner_window": runner_window, "action": action, "reply": reply, "reply_error": reply_error}
    (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"case": label, "passed": not errors, "after": after, "errors": errors}), flush=True)
    return report


def judge_input(record, nonce, kind, row):
    if not isinstance(record, dict) or record.get("run_id") != nonce:
        return ["no independent input receipt for this launch"]
    errors = []
    if kind == "typing":
        if (record.get("text") != "照片" or type(record.get("began_editing")) is not int
                or record["began_editing"] != 1 or record.get("selections") != []):
            errors.append("the fresh search field did not receive exactly the requested text")
    elif record.get("selections") != [row]:
        errors.append("the requested row was not selected exactly once")
    offset = record.get("offset_y")
    if kind == "scroll" and (type(offset) not in (int, float) or not math.isfinite(offset) or offset <= 20):
        errors.append("the UIKit table did not scroll")
    return errors


def input_case(daemon, args, label, kind):
    out = args.out / label
    out.mkdir()
    nonce = uuid.uuid4().hex
    daemon.require_simulator(args.udid)
    command(["xcrun", "simctl", "terminate", args.udid, BUNDLE], check=False, timeout=30)
    env = {k: v for k, v in os.environ.items() if not k.startswith("SIMCTL_CHILD_")}
    env.update(SIMCTL_CHILD_GESTURE_RUN_ID=nonce, SIMCTL_CHILD_GESTURE_FIXTURE_MODE="input")
    command(["xcrun", "simctl", "launch", args.udid, BUNDLE], env=env, timeout=60)
    before = daemon.wait_metrics(nonce, identifier="input-result")
    if not before or before.get("selections") != [] or before.get("text") != "":
        raise RuntimeError("input fixture is not fresh; no input sent")
    row = 8 if kind == "scroll" else 3
    steps = []
    if kind == "scroll":
        steps.extend([
            {"kind": "action", "action": {"type": "scroll", "x": 0.5, "y": 0.65, "dx": 0, "dy": 300}},
            {"kind": "wait_for", "expect": {"present": [{"identifier": f"input-row-{row}", "kind": "Cell"}]}, "timeout_ms": 4000},
        ])
    if kind == "typing":
        steps.extend([
            {"kind": "action", "action": {"type": "tap_locator", "locator": {"kind": "SearchField"}}},
            {"kind": "action", "action": {"type": "text", "text": "照片"}},
        ])
    else:
        steps.append({"kind": "action", "action": {"type": "tap_locator", "locator": {"identifier": f"input-row-{row}", "kind": "Cell"}}})
    daemon.require_simulator(args.udid)
    reply, reply_error = None, None
    try:
        # Exactly one send. Wait only for the receiver's postcondition below;
        # a missing acknowledgement or selection must never replay the batch.
        reply = daemon.call("/agent/actions", {"steps": steps, "observe": True})
    except (OSError, ValueError) as error:
        reply_error = str(error)
    after = None
    until = time.monotonic() + 4
    while time.monotonic() < until:
        after = daemon.metrics(nonce, "input-result")
        if after and (after.get("text") == "照片" if kind == "typing" else after.get("selections")):
            break
        time.sleep(0.1)
    errors = judge_input(after, nonce, kind, row)
    if reply_error or not isinstance(reply, dict) or reply.get("ok") is not True:
        errors.append("input acknowledgement failed: " + str(reply_error or reply))
    report = {"case": label, "carrier": "batch", "passed": not errors, "errors": errors,
              "before": before, "after": after, "steps": steps, "reply": reply, "reply_error": reply_error}
    (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"case": label, "passed": not errors, "after": after, "errors": errors}), flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--udid", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--owner", default="gesture-fixture")
    parser.add_argument("--settings-suite", type=Path)
    parser.add_argument("--input-readiness", action="store_true", help="also verify scroll/tap and first-focus typing against UIKit controls")
    parser.add_argument("--runner-record", required=True, type=Path)
    parser.add_argument("--mcp", type=Path, default=ROOT / "target/release/iphone-use-mcp")
    args = parser.parse_args()
    uuid.UUID(args.udid)
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    if args.settings_suite:
        args.settings_suite = args.settings_suite.resolve()
        args.settings_suite.relative_to(args.out)
        if args.settings_suite.exists():
            raise RuntimeError("settings suite output already exists")
        args.settings_suite.parent.mkdir(parents=True, exist_ok=True)
    daemon = Daemon(args.base, args.owner)
    status = daemon.require_simulator(args.udid)
    if not args.mcp.is_file():
        raise RuntimeError("candidate iphone-use-mcp binary is missing")
    app = build_fixture(args.out)
    command(["xcrun", "simctl", "install", args.udid, app], timeout=120)
    reports, harness_error, cleanup_error, settings_binding = [], None, None, None
    try:
        for label, carrier, hover in [("longpress-regression", "longpress", False),
                                      ("native-swipe-regression", "native", False),
                                      ("multi-single", "multi-single", False),
                                      ("multi-batch", "multi-batch", False),
                                      ("multi-flow", "multi-flow", False),
                                      ("single-hover", "single", True), ("batch-hover", "batch", True),
                                      ("flow-hover", "flow", True), ("slow-drift-control", "batch", False)]:
            reports.append(case(daemon, args, label, carrier, hover))
            if not reports[-1]["passed"]:
                break
        if len(reports) == 9 and all(r["passed"] for r in reports) and args.input_readiness:
            for label, kind in [("settled-row-control", "tap"),
                                *[(f"scroll-then-tap-{i}", "scroll") for i in range(3)],
                                *[(f"first-focus-typing-{i}", "typing") for i in range(3)]]:
                reports.append(input_case(daemon, args, label, kind))
                if not reports[-1]["passed"]:
                    break
        expected_cases = 16 if args.input_readiness else 9
        if len(reports) == expected_cases and all(r["passed"] for r in reports) and args.settings_suite:
            settings_binding = localized_settings_suite(daemon, args)
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
        harness_error = str(error)
    finally:
        if not status.get("owner"):
            try:
                daemon.call("/agent/owner", {"release": True})
            except OSError as error:
                cleanup_error = str(error)
    def digest(path):
        value = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                value.update(chunk)
        return value.hexdigest()
    source_files = ["scripts/test-drag-simulator.py", "runner/gesture-fixture/main.swift", "runner/gesture-fixture/Info.plist", "runner/gesture-fixture/LaunchScreen.storyboard", "runner/gesture-fixture/GestureFixture.xcodeproj/project.pbxproj",
                    "runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerActions.swift",
                    "runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.m",
                    "crates/server/src/runner.rs", "crates/server/src/http.rs"]
    provenance = {"source_commit": command(["git", "-C", ROOT, "rev-parse", "HEAD"], check=False).stdout.strip() if (ROOT/".git").exists() else None,
                  "source_sha256": {name:digest(ROOT/name) for name in source_files},
                  "mcp_sha256":digest(args.mcp), "device":status.get("device"),
                  "scope":"iOS Simulator touch delivery; not physical iPhone or SpringBoard folder acceptance"}
    result = {"provenance":provenance, "passed": len(reports) == (16 if args.input_readiness else 9) and all(r["passed"] for r in reports) and not harness_error and not cleanup_error,
              "udid": args.udid, "cases": reports, "settings_binding": settings_binding, "harness_error": harness_error, "cleanup_error": cleanup_error}
    (args.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in ["passed", "harness_error", "cleanup_error", "settings_binding"]}), flush=True)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
