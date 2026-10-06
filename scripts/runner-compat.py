#!/usr/bin/env python3
"""WDA-compatibility check for the native iphone-use runner (runner/).

Exercises every WebDriverAgent route the daemon's WdaClient (crates/server/src/wda.rs) uses,
with the same request bodies, and checks the response shapes WdaClient parses. Also reads the
MJPEG stream (WDA's multipart format, port 9100) and reports the achieved frame rate.

    python3 scripts/runner-compat.py [--base http://127.0.0.1:8100] [--mjpeg http://127.0.0.1:9100]

By default it only reads: session setup, settings, tree, screenshot, finds, element reads,
alert reads, lock state, the MJPEG stream. Opt-in flags change the screen:

    --mutate            Home, launch Settings, a W3C swipe there and back, element click on a
                        Settings row, keyboard dismiss (a no-op without a keyboard), back to Home
    --tap X Y           one W3C tap at a harmless point (screen points)
    --keys TEXT         type TEXT into the focused field (/wda/keys and a W3C key action)
    --url URL           POST /session/:sid/url
    --lock              lock the phone (POST /wda/lock) and check /wda/locked turns true

Exit code 1 when any check fails.
"""

import argparse
import base64
import json
import sys
import time
import urllib.error
import urllib.request

W3C_KEY = "element-6066-11e4-a52e-4f735466cecf"


class Checker:
    def __init__(self, base):
        self.base = base.rstrip("/")
        self.failures = 0
        self.sid = None

    def call(self, method, path, body=None, timeout=60.0):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            request.add_header("Content-Type", "application/json")
        started = time.perf_counter()
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                status, raw, headers = response.status, response.read(), dict(response.headers)
        except urllib.error.HTTPError as error:
            status, raw, headers = error.code, error.read(), dict(error.headers)
        elapsed = (time.perf_counter() - started) * 1000
        try:
            payload = json.loads(raw) if raw else None
        except ValueError:
            payload = None
        return status, payload, headers, elapsed

    def s(self, path):
        return f"/session/{self.sid}{path}"

    def report(self, ok, label, elapsed, detail=""):
        if not ok:
            self.failures += 1
        mark = "ok  " if ok else "FAIL"
        print(f"{mark} {label:<48} {elapsed:7.0f} ms  {detail}"[:220])

    def expect(self, label, method, path, body=None, status=200, check=None, timeout=60.0):
        """Calls a route; checks the HTTP status, the WDA envelope, and `check(value)`."""
        code, payload, headers, elapsed = self.call(method, path, body, timeout)
        value = (payload or {}).get("value") if isinstance(payload, dict) else None
        ok = code == status and isinstance(payload, dict) and "value" in payload
        detail = json.dumps(value, ensure_ascii=False)[:120]
        if ok and status >= 400:
            ok = isinstance(value, dict) and "error" in value and "message" in value
        if ok and check is not None:
            try:
                verdict = check(value)
            except Exception as error:  # noqa: BLE001 - a broken shape is a failed check
                verdict = f"check raised {error!r}"
            if verdict is not True:
                ok = False
                detail = f"{verdict} :: {detail}"
        if code != status:
            detail = f"HTTP {code} (want {status}) :: {detail}"
        self.report(ok, label, elapsed, detail)
        return value, headers

    def expect_either(self, label, method, path, body=None, check=None, missing_code="no such element"):
        """200 with `check`, or 404 with WDA's `missing_code` error (both are valid answers)."""
        code, payload, _, elapsed = self.call(method, path, body)
        value = (payload or {}).get("value") if isinstance(payload, dict) else None
        if code == 404:
            ok = isinstance(value, dict) and value.get("error") == missing_code
            self.report(ok, label + " (none)", elapsed, json.dumps(value, ensure_ascii=False)[:120])
            return None
        ok = code == 200 and (check is None or check(value) is True)
        self.report(ok, label, elapsed, json.dumps(value, ensure_ascii=False)[:120])
        return value if ok else None


def is_ref(value):
    if not isinstance(value, dict):
        return "not an object"
    if not isinstance(value.get("ELEMENT"), str) or value.get(W3C_KEY) != value.get("ELEMENT"):
        return "element reference lacks ELEMENT / W3C key"
    return True


def is_rect(value):
    keys = ("x", "y", "width", "height")
    return True if isinstance(value, dict) and all(isinstance(value.get(k), (int, float)) for k in keys) else "bad rect"


def tree_shape(root):
    if not isinstance(root, dict) or not str(root.get("type", "")).startswith("XCUIElementType"):
        return "root is not a WDA node"
    for key in ("label", "name", "value", "rawIdentifier", "placeholderValue", "rect", "isEnabled", "isFocused"):
        if key not in root:
            return f"root lacks {key}"
    return True


def count(node):
    return 1 + sum(count(child) for child in node.get("children") or []) if isinstance(node, dict) else 0


def check_mjpeg(url, seconds):
    """Reads the stream for `seconds`; returns (frames, bytes per frame, header ok, first error)."""
    request = urllib.request.Request(url.rstrip("/") + "/")
    frames, sizes = 0, []
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            content_type = response.headers.get("Content-Type", "")
            header_ok = "multipart/x-mixed-replace" in content_type and "BoundaryString" in content_type
            buffer = b""
            while time.perf_counter() - started < seconds:
                chunk = response.read1(65536) if hasattr(response, "read1") else response.read(65536)
                if not chunk:
                    break
                buffer += chunk
                while True:
                    soi = buffer.find(b"\xff\xd8")
                    if soi < 0:
                        break
                    head = buffer[:soi].decode("latin-1").rsplit("--", 1)[-1]
                    length = None
                    for line in head.splitlines():
                        name, _, value = line.partition(":")
                        if name.strip().lower() == "content-length":
                            length = int(value.strip())
                    if length is None:
                        return frames, sizes, header_ok, "part without Content-Length"
                    if len(buffer) < soi + length:
                        break
                    jpeg = buffer[soi:soi + length]
                    if not jpeg.endswith(b"\xff\xd9"):
                        return frames, sizes, header_ok, "frame does not end with FFD9"
                    frames += 1
                    sizes.append(length)
                    buffer = buffer[soi + length:]
    except OSError as error:
        return frames, sizes, False, str(error)
    return frames, sizes, header_ok, None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default="http://127.0.0.1:8100")
    parser.add_argument("--mjpeg", default="http://127.0.0.1:9100", help="MJPEG URL; 'off' skips it")
    parser.add_argument("--mjpeg-seconds", type=float, default=5)
    parser.add_argument("--mutate", action="store_true")
    parser.add_argument("--tap", nargs=2, type=float, metavar=("X", "Y"))
    parser.add_argument("--keys")
    parser.add_argument("--url")
    parser.add_argument("--lock", action="store_true")
    args = parser.parse_args()
    c = Checker(args.base)
    print(f"runner at {c.base}")

    # --- health, session, settings -------------------------------------------------------------
    c.expect("GET /status", "GET", "/status",
             check=lambda v: True if v.get("ready") is True and v.get("sessionId") else "ready/sessionId missing")
    c.expect("GET /wda/locked (sessionless)", "GET", "/wda/locked",
             check=lambda v: True if isinstance(v, bool) else "not a bool")
    code, payload, _, elapsed = c.call("POST", "/session", {
        "capabilities": {"alwaysMatch": {"shouldWaitForQuiescence": False}, "firstMatch": [{}]}})
    c.sid = (payload or {}).get("sessionId") or ((payload or {}).get("value") or {}).get("sessionId")
    c.report(code == 200 and bool(c.sid), "POST /session", elapsed, f"sessionId={c.sid}")
    if not c.sid:
        print("no session; stopping")
        return 1
    c.expect("POST /appium/settings (idle)", "POST", c.s("/appium/settings"),
             {"settings": {"waitForIdleTimeout": 0, "animationCoolOffTimeout": 0}},
             check=lambda v: True if isinstance(v, dict) else "settings not echoed")
    c.expect("GET /session/:sid/wda/locked", "GET", c.s("/wda/locked"),
             check=lambda v: True if isinstance(v, bool) else "not a bool")
    c.expect("GET /session/:sid/wda/apps/list", "GET", c.s("/wda/apps/list"),
             check=lambda v: True if isinstance(v, list) and v and all("bundleId" in a and "pid" in a for a in v)
             else "need [{bundleId, pid}]")

    # --- tree, screen ----------------------------------------------------------------------------
    for path in ("/source?format=json", "/source?format=json&excluded_attributes=visible,accessible"):
        root, headers = c.expect(f"GET {path[:40]}", "GET", path, check=tree_shape)
        if isinstance(root, dict):
            print(f"     nodes={count(root)} backend={headers.get('X-IPU-Backend')} "
                  f"server={headers.get('Server-Timing')}")
    c.expect("GET /screenshot", "GET", "/screenshot",
             check=lambda v: True if base64.b64decode(v)[:4] == b"\x89PNG" else "not a base64 PNG")
    c.expect("GET /session/:sid/window/size", "GET", c.s("/window/size"),
             check=lambda v: True if v.get("width", 0) > 0 and v.get("height", 0) > 0 else "bad size")

    # --- finds and element reads -----------------------------------------------------------------
    refs, _ = c.expect("POST /elements (class chain)", "POST", c.s("/elements"),
                       {"using": "class chain", "value": "**/XCUIElementTypeStaticText"},
                       check=lambda v: True if isinstance(v, list) and all(is_ref(r) is True for r in v) else "bad refs")
    c.expect("POST /elements (predicate, none)", "POST", c.s("/elements"),
             {"using": "predicate string", "value": "label == '__ipu_no_such_label__'"},
             check=lambda v: True if v == [] else "expected []")
    c.expect("POST /element (missing -> 404)", "POST", c.s("/element"),
             {"using": "accessibility id", "value": "__ipu_no_such_id__"}, status=404,
             check=lambda v: True if v.get("error") == "no such element" else "wrong error code")
    c.expect("POST /elements (bad predicate -> 400)", "POST", c.s("/elements"),
             {"using": "predicate string", "value": "label =="}, status=400)
    element = refs[0]["ELEMENT"] if isinstance(refs, list) and refs else None
    if element:
        c.expect("GET /element/:id/rect", "GET", c.s(f"/element/{element}/rect"), check=is_rect)
        c.expect("GET /element/:id/attribute/value", "GET", c.s(f"/element/{element}/attribute/value"),
                 check=lambda v: True if v is None or isinstance(v, (str, int, float, bool)) else "bad value")
        c.expect("GET /element/:id/attribute/visible", "GET", c.s(f"/element/{element}/attribute/visible"),
                 check=lambda v: True if isinstance(v, bool) else "not a bool")
        c.expect("POST /element/:id/elements", "POST", c.s(f"/element/{element}/elements"),
                 {"using": "class chain", "value": "**/*"},
                 check=lambda v: True if isinstance(v, list) else "not a list")
        label, _ = c.expect("GET /element/:id/attribute/label", "GET", c.s(f"/element/{element}/attribute/label"))
        if isinstance(label, str) and label:
            c.expect("POST /element (accessibility id = label)", "POST", c.s("/element"),
                     {"using": "accessibility id", "value": label}, check=is_ref)
    else:
        print("SKIP element reads: no StaticText on screen")
    c.expect("GET /element/stale/rect -> 404", "GET", c.s("/element/00000000-STALE/rect"), status=404)
    c.expect_either("GET /element/active", "GET", c.s("/element/active"), check=is_ref)

    # --- alerts ----------------------------------------------------------------------------------
    text = c.expect_either("GET /alert/text", "GET", c.s("/alert/text"),
                           check=lambda v: isinstance(v, str), missing_code="no such alert")
    c.expect_either("GET /wda/alert/buttons", "GET", c.s("/wda/alert/buttons"),
                    check=lambda v: isinstance(v, list), missing_code="no such alert")
    if text is None:
        c.expect("POST /alert/accept (none -> 404)", "POST", c.s("/alert/accept"), {}, status=404,
                 check=lambda v: True if v.get("error") == "no such alert" else "wrong error code")
        c.expect("POST /alert/dismiss (none -> 404)", "POST", c.s("/alert/dismiss"), {}, status=404,
                 check=lambda v: True if v.get("error") == "no such alert" else "wrong error code")
    else:
        print("SKIP alert accept/dismiss: an alert is open and accepting it would change the phone")
    c.expect("unknown route -> 404", "GET", c.s("/no/such/route"), status=404)

    # --- opt-in: screen-changing routes ----------------------------------------------------------
    if args.tap:
        x, y = args.tap
        c.expect("POST /actions (tap)", "POST", c.s("/actions"), {"actions": [{
            "type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"},
            "actions": [{"type": "pointerMove", "duration": 0, "x": x, "y": y},
                        {"type": "pointerDown", "button": 0}, {"type": "pointerUp", "button": 0}]}]})
    if args.mutate:
        c.expect("POST /wda/pressButton home", "POST", c.s("/wda/pressButton"), {"name": "home"})
        c.expect("POST /wda/apps/launch Settings", "POST", c.s("/wda/apps/launch"), {"bundleId": "com.apple.Preferences"})
        time.sleep(1.0)
        c.expect("GET /wda/apps/list (Settings first)", "GET", c.s("/wda/apps/list"),
                 check=lambda v: True if v and v[0].get("bundleId") == "com.apple.Preferences" else "Settings not first")
        size, _ = c.expect("GET /window/size", "GET", c.s("/window/size"))
        w, h = (size or {}).get("width", 390), (size or {}).get("height", 844)
        for name, (y1, y2) in (("up", (0.7, 0.4)), ("down", (0.4, 0.7))):
            c.expect(f"POST /actions (swipe {name})", "POST", c.s("/actions"), {"actions": [{
                "type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"},
                "actions": [{"type": "pointerMove", "duration": 0, "x": w / 2, "y": h * y1},
                            {"type": "pointerDown", "button": 0}, {"type": "pause", "duration": 80},
                            {"type": "pointerMove", "duration": 250, "x": w / 2, "y": h * y2},
                            {"type": "pointerUp", "button": 0}]}]})
            time.sleep(0.6)
        cells, _ = c.expect("POST /elements (Settings cells)", "POST", c.s("/elements"),
                            {"using": "class chain", "value": "**/XCUIElementTypeCell"})
        if isinstance(cells, list) and cells:
            cell = cells[0]["ELEMENT"]
            c.expect("POST /element/:id/click (first cell)", "POST", c.s(f"/element/{cell}/click"), {})
            time.sleep(0.8)
            c.expect("POST /actions (edge swipe back)", "POST", c.s("/actions"), {"actions": [{
                "type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"},
                "actions": [{"type": "pointerMove", "duration": 0, "x": 1, "y": h / 2},
                            {"type": "pointerDown", "button": 0}, {"type": "pause", "duration": 80},
                            {"type": "pointerMove", "duration": 250, "x": w * 0.55, "y": h / 2},
                            {"type": "pointerUp", "button": 0}]}]})
        c.expect("POST /wda/keyboard/dismiss", "POST", c.s("/wda/keyboard/dismiss"),
                 {"keyNames": ["Done", "完了", "return", "前往", "search"]})
        c.expect("POST /wda/pressButton home (back)", "POST", c.s("/wda/pressButton"), {"name": "home"})
    if args.keys:
        c.expect("POST /wda/keys", "POST", c.s("/wda/keys"), {"value": [args.keys]})
        c.expect("POST /actions (key: return)", "POST", c.s("/actions"), {"actions": [{
            "type": "key", "id": "keyboard",
            "actions": [{"type": "keyDown", "value": ""}, {"type": "keyUp", "value": ""}]}]})
    if args.url:
        c.expect("POST /session/:sid/url", "POST", c.s("/url"), {"url": args.url})
    if args.lock:
        c.expect("POST /wda/lock", "POST", c.s("/wda/lock"), {})
        time.sleep(1.0)
        c.expect("GET /wda/locked (after lock)", "GET", "/wda/locked",
                 check=lambda v: True if v is True else "phone did not report locked")

    # --- MJPEG -----------------------------------------------------------------------------------
    if args.mjpeg != "off":
        c.expect("POST /appium/settings (mjpeg 30/50/60)", "POST", c.s("/appium/settings"),
                 {"settings": {"mjpegServerFramerate": 30, "mjpegScalingFactor": 50,
                               "mjpegServerScreenshotQuality": 60}})
        frames, sizes, header_ok, error = check_mjpeg(args.mjpeg, args.mjpeg_seconds)
        fps = frames / args.mjpeg_seconds
        average = sum(sizes) / len(sizes) if sizes else 0
        ok = header_ok and frames > 0 and error is None
        c.report(ok, f"MJPEG {args.mjpeg}", args.mjpeg_seconds * 1000,
                 f"{frames} frames, {fps:.1f} fps, avg {average / 1024:.0f} KiB"
                 + ("" if header_ok else ", bad Content-Type") + (f", {error}" if error else ""))
        status, payload, _, _ = c.call("GET", "/status")
        print(f"     runner mjpeg status: {json.dumps(((payload or {}).get('value') or {}).get('mjpeg'))}")

    print("OK" if c.failures == 0 else f"{c.failures} failure(s)")
    return 0 if c.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
