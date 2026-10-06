#!/usr/bin/env python3
"""Latency census of iphone-use primitives on a real phone — the shared yardstick.

Run before and after a performance change, on the same screen, and paste both
tables into the PR. It goes through the daemon only (never WDA :8100 directly,
which bypasses the owner lease and is not counted as activity), takes the
phone with an owner name and a hold, and releases both at the end.

    PHONE_REMOTE_URL=http://127.0.0.1:45432 PHONE_REMOTE_TOKEN=... \\
        python3 scripts/bench-census.py [--runs 3] [--app com.apple.calculator]

Each row: wall-clock ms per run, then the daemon's own timing split of the last
run (daemon vs WDA, per WDA call). Taps use Calculator's keypad identifiers, so
keep --app at the default for comparable numbers.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

OWNER = "bench-census"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--app", default="com.apple.calculator")
    args = parser.parse_args()
    base = os.environ.get("PHONE_REMOTE_URL", "http://127.0.0.1:44321").rstrip("/")
    token = os.environ.get("PHONE_REMOTE_TOKEN", "")
    headers = {
        "Authorization": "Bearer " + token,
        "X-Phone-Control": "1",
        "X-Phone-Owner": OWNER,
        "Content-Type": "application/json",
    }

    def request(method, path, body=None):
        started = time.time()
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=90) as resp:
                raw = resp.read()
        except urllib.error.HTTPError as error:
            raw = error.read()
        wall = (time.time() - started) * 1000
        try:
            parsed = json.loads(raw)
        except ValueError:
            parsed = {}
        return wall, parsed if isinstance(parsed, dict) else {}

    def act(action):
        return request("POST", "/agent/input", action)

    status = request("GET", "/agent/status")[1]
    if status.get("owner") not in (None, OWNER):
        print(f"phone is in use by {status.get('owner')!r}; not benchmarking", file=sys.stderr)
        return 1
    if status.get("drivable") is not True:
        print(f"phone is not drivable ({status.get('device_state')}): {status.get('hint')}", file=sys.stderr)
        return 1
    print(f"daemon {status.get('version')} · transport {status.get('transport', '?')} · runs {args.runs}")
    request("POST", "/agent/hold", {"secs": 900})
    act({"type": "launch_app", "bundle": args.app})  # warm-up (session, auto DND)
    time.sleep(1)

    ops = [
        ("status", lambda: request("GET", "/agent/status"), False),
        ("launch_app (running)", lambda: act({"type": "launch_app", "bundle": args.app}), False),
        ("tap x,y", lambda: act({"type": "tap", "x": 0.5, "y": 0.95}), True),
        ("tap_locator identifier", lambda: act({"type": "tap_locator", "locator": {"identifier": "Seven"}}), True),
        ("tap label", lambda: act({"type": "tap", "label": "7"}), True),
        ("key return", lambda: act({"type": "key", "name": "return"}), True),
        ("elements (full tree)", lambda: request("GET", "/agent/elements"), True),
        ("screenshot", lambda: request("GET", "/agent/screenshot"), True),
        ("swipe", lambda: act({"type": "swipe", "x1": 0.5, "y1": 0.6, "x2": 0.5, "y2": 0.4}), True),
        ("shortcut home", lambda: act({"type": "shortcut", "name": "home"}), False),
    ]
    try:
        for name, op, needs_app in ops:
            if needs_app:
                act({"type": "launch_app", "bundle": args.app})
            walls, last = [], {}
            for _ in range(args.runs):
                wall, last = op()
                walls.append(wall)
            timing = last.get("timing") or {}
            calls = ", ".join(f"{c['call']}×{c.get('count', 1)}={c['ms']}" for c in timing.get("wda", []))
            error = f" ERR {last.get('error')}" if last.get("ok") is False else ""
            print(
                f"{name:24s} wall ms: {', '.join(f'{w:.0f}' for w in walls):18s} "
                f"| daemon={timing.get('daemon_ms')} wda={timing.get('wda_ms')} [{calls}]{error}",
                flush=True,
            )
    finally:
        request("POST", "/agent/hold", {"secs": 0})
        request("POST", "/agent/owner", {"release": True})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
