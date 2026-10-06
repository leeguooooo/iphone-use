#!/usr/bin/env python3
"""Smoke test + micro-benchmark for the native iphone-use runner (runner/).

Talks straight to the runner's HTTP port (forwarded with `iproxy 8100 8100`).
Read-only except for one tap, and only when --tap X Y is given — pick a harmless
point (an empty area of the current screen).

    python3 scripts/runner-smoke.py [--base http://127.0.0.1:8100] [--runs 5] [--tap 200 60]

Reports: /status, /window/size, /apps/active, /alert, then /source timing over
--runs (with node counts and the runner's backend/depth headers), the xcui
fallback once for comparison, the optional tap, and /screenshot timing.
Exit code 1 when any check fails.
"""

import argparse
import base64
import json
import statistics
import sys
import time
import urllib.error
import urllib.request


def call(base, method, path, body=None, timeout=60.0):
    """Returns (status, json_or_None, headers, elapsed_ms)."""
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(base + path, data=data, method=method)
    if data is not None:
        request.add_header("Content-Type", "application/json")
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            status, headers = response.status, dict(response.headers)
    except urllib.error.HTTPError as error:
        raw = error.read()
        status, headers = error.code, dict(error.headers)
    elapsed = (time.perf_counter() - started) * 1000
    try:
        payload = json.loads(raw) if raw else None
    except ValueError:
        payload = None
    return status, payload, headers, elapsed


def count_nodes(node):
    if not isinstance(node, dict):
        return 0
    return 1 + sum(count_nodes(child) for child in node.get("children") or [])


def check_wda_shape(node):
    """Problems with the root node's WDA /source shape (empty list = fine)."""
    problems = []
    for key in ("type", "label", "name", "value", "rawIdentifier", "placeholderValue", "rect", "isEnabled", "isFocused"):
        if key not in node:
            problems.append(f"missing {key}")
    if not str(node.get("type", "")).startswith("XCUIElementType"):
        problems.append(f"type {node.get('type')!r}")
    rect = node.get("rect") or {}
    if not all(k in rect for k in ("x", "y", "width", "height")):
        problems.append("rect lacks x/y/width/height")
    if node.get("isEnabled") not in ("0", "1"):
        problems.append(f"isEnabled {node.get('isEnabled')!r}")
    return problems


def summary(samples):
    if not samples:
        return "-"
    return (f"min {min(samples):.0f}  median {statistics.median(samples):.0f}  "
            f"max {max(samples):.0f} ms  (n={len(samples)})")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="http://127.0.0.1:8100")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--tap", nargs=2, type=float, metavar=("X", "Y"),
                        help="tap this point (screen points) once and time it; skipped when absent")
    parser.add_argument("--max-depth", type=int, help="pass max_depth to /source")
    parser.add_argument("--no-xcui", action="store_true", help="skip the xcui-snapshot comparison run")
    parser.add_argument("--screenshot-out", help="write the last screenshot PNG here")
    args = parser.parse_args()
    base = args.base.rstrip("/")
    failures = []

    def fail(message):
        failures.append(message)
        print(f"  FAIL {message}")

    print(f"runner at {base}")
    try:
        status, payload, _, elapsed = call(base, "GET", "/status", timeout=5)
    except OSError as error:
        print(f"  FAIL /status unreachable: {error}")
        return 1
    value = (payload or {}).get("value") or {}
    print(f"/status        {status}  {elapsed:6.0f} ms  {json.dumps(value)}")
    if status != 200 or value.get("ready") is not True:
        fail("/status not ready")

    for path in ("/window/size", "/apps/active", "/alert"):
        status, payload, _, elapsed = call(base, "GET", path)
        value = (payload or {}).get("value")
        print(f"{path:<14} {status}  {elapsed:6.0f} ms  {json.dumps(value, ensure_ascii=False)[:160]}")
        if path == "/alert":
            if status not in (200, 404):
                fail(f"/alert returned {status}")
        elif status != 200:
            fail(f"{path} returned {status}")

    query = f"?max_depth={args.max_depth}" if args.max_depth else ""
    samples, server_samples = [], []
    for run in range(args.runs):
        status, payload, headers, elapsed = call(base, "GET", "/source" + query)
        root = (payload or {}).get("value")
        if status != 200 or not isinstance(root, dict):
            fail(f"/source run {run + 1}: {status} {json.dumps(root)[:200]}")
            continue
        samples.append(elapsed)
        server = headers.get("Server-Timing", "")
        if "dur=" in server:
            server_samples.append(float(server.split("dur=")[1]))
        nodes = count_nodes(root)
        print(f"/source #{run + 1}     {status}  {elapsed:6.0f} ms  nodes={nodes} "
              f"backend={headers.get('X-IPU-Backend')} depth={headers.get('X-IPU-Depth')} "
              f"ext={headers.get('X-IPU-Extension-Calls')} truncated={headers.get('X-IPU-Truncated')} "
              f"pid={headers.get('X-IPU-Pid')} bytes={headers.get('Content-Length')}")
        if run == 0:
            for problem in check_wda_shape(root):
                fail(f"/source shape: {problem}")
    print(f"/source wall   {summary(samples)}")
    print(f"/source server {summary(server_samples)}")

    if not args.no_xcui:
        status, payload, headers, elapsed = call(base, "GET", "/source?backend=xcui", timeout=120)
        root = (payload or {}).get("value")
        nodes = count_nodes(root) if isinstance(root, dict) else 0
        print(f"/source xcui   {status}  {elapsed:6.0f} ms  nodes={nodes} backend={headers.get('X-IPU-Backend')}")

    if args.tap:
        x, y = args.tap
        status, payload, headers, elapsed = call(base, "POST", "/tap", {"x": x, "y": y})
        print(f"/tap ({x:g},{y:g})  {status}  {elapsed:6.0f} ms  path={headers.get('X-IPU-Gesture')} "
              f"{json.dumps((payload or {}).get('value'))[:120]}")
        if status != 200:
            fail("/tap failed")

    shots = []
    png = None
    for _ in range(max(1, min(3, args.runs))):
        status, payload, _, elapsed = call(base, "GET", "/screenshot")
        encoded = (payload or {}).get("value")
        if status != 200 or not isinstance(encoded, str):
            fail(f"/screenshot returned {status}")
            break
        png = base64.b64decode(encoded)
        if not png.startswith(b"\x89PNG"):
            fail("/screenshot is not a PNG")
            break
        shots.append(elapsed)
    if png:
        print(f"/screenshot    {summary(shots)}  png={len(png)} bytes")
        if args.screenshot_out:
            with open(args.screenshot_out, "wb") as handle:
                handle.write(png)
            print(f"  wrote {args.screenshot_out}")

    status, _, _, _ = call(base, "GET", "/nope")
    if status != 404:
        fail(f"unknown endpoint returned {status}, expected 404")

    print("OK" if not failures else f"{len(failures)} failure(s)")
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())
