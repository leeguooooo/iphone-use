#!/usr/bin/env python3
"""Exercise a real daemon binary against isolated synthetic target scopes.

No Simulator or phone is used. This checks the consumer's public HTTP contract;
Native producer behavior needs separate App evidence.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

CASES = ["partial-source", "unknown-source", "invalid-reference", "covered-point",
         "covered-default", "stale-source-clear-live", "clear-point", "replaced-handle",
         "query-lost-after-reveal"]


def run_case(binary, case, out):
    requests = []
    scrolled = False
    owner = "action-protocol"

    class Provider(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.answer()

        def do_POST(self):
            self.answer()

        def do_DELETE(self):
            self.answer()

        def answer(self):
            nonlocal scrolled
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            path = self.path.split("?", 1)[0]
            entry = {"method": self.command, "path": path}
            if path.endswith("/actions"):
                entry["body"] = json.loads(raw)
            requests.append(entry)
            if path.endswith("/scrollTo"):
                scrolled = True
            value = None
            if path == "/status":
                value = {"ready": True, "message": "synthetic target protocol fixture"}
            elif path == "/session":
                value = {"sessionId": "FIXTURE"}
            elif path.endswith("/wda/locked"):
                value = False
            elif path.endswith("/wda/apps/list"):
                value = []
            elif path.endswith("/source"):
                source_y = 745 if case in ("stale-source-clear-live", "clear-point") else 400
                value = {"type": "XCUIElementTypeApplication", "label": "Fixture",
                         "rect": {"x": 0, "y": 0, "width": 390, "height": 844},
                         "children": [
                             {"type": "XCUIElementTypeButton", "label": "Done",
                              "rect": {"x": 16, "y": source_y, "width": 358, "height": 51}},
                             {"type": "XCUIElementTypeSearchField", "label": "Search",
                              "rect": {"x": 28, "y": 778, "width": 334, "height": 28}}]}
            elif path.endswith("/elements"):
                element = "B" if scrolled and case == "replaced-handle" else "A"
                value = [{"ELEMENT": element}]
                if case == "invalid-reference":
                    value.append({})
            elif path.endswith("/rect"):
                covered = case in ("covered-point", "covered-default", "replaced-handle", "query-lost-after-reveal")
                live_y = 780 if covered and not (scrolled and case == "replaced-handle") else 420
                value = {"x": 16, "y": live_y, "width": 358, "height": 51}
            elif path.endswith("/window/size"):
                value = {"width": 390, "height": 844}
            payload = json.dumps({"value": value}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            if path.endswith(("/source", "/elements")):
                unknown = case == "unknown-source" and path.endswith("/source")
                unknown |= case == "query-lost-after-reveal" and scrolled and path.endswith("/elements")
                if not unknown:
                    partial = case == "partial-source" and path.endswith("/source")
                    self.send_header("X-IPU-Truncated", "1" if partial else "0")
                    self.send_header("X-IPU-Tree-Complete", "0" if partial else "1")
            self.end_headers()
            self.wfile.write(payload)

    provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=lambda: provider.serve_forever(poll_interval=0.05), daemon=True)
    thread.start()
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    token = secrets.token_hex(32)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    process = None
    receipt = {"name": case, "passed": False}
    log = out / (case + ".log")
    with tempfile.TemporaryDirectory(prefix="ipu-target-protocol-") as work:
        prefixes = ("IPHONE_USE_", "PHONE_REMOTE_", "IOS_USE_", "IPU_", "TEST_RUNNER_")
        env = {key: value for key, value in os.environ.items() if not key.startswith(prefixes)
               and key.lower() not in ("http_proxy", "https_proxy", "all_proxy")}
        env.update(IPHONE_USE_INSTANCE="protocol-" + secrets.token_hex(4),
                   IPHONE_USE_STATE_DIR=work + "/state", IPHONE_USE_HOST="127.0.0.1",
                   IPHONE_USE_PORT=str(port), IPHONE_USE_RUNNER_URL=f"http://127.0.0.1:{provider.server_port}",
                   IPHONE_USE_RUNNER_MJPEG_URL=f"http://127.0.0.1:{provider.server_port}",
                   IPHONE_USE_RUNNER_MANAGED="false", IPHONE_USE_AGENT_TOKEN=token,
                   IPHONE_USE_OWNER=owner, IPHONE_USE_FLOWS_NO_AUTO_UPDATE="1", CI="true")
        try:
            with log.open("w") as output:
                process = subprocess.Popen([str(binary), "serve"], env=env, stdout=output,
                                           stderr=subprocess.STDOUT, start_new_session=True)
            auth = {"Authorization": "Bearer " + token}
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                assert process.poll() is None, "daemon stopped before readiness"
                try:
                    req = urllib.request.Request(f"http://127.0.0.1:{port}/agent/status", headers=auth)
                    with opener.open(req, timeout=2) as response:
                        if json.load(response).get("drivable"):
                            break
                except OSError:
                    pass
                time.sleep(0.1)
            else:
                raise AssertionError("synthetic provider did not become drivable")
            via = "point" if case in ("covered-point", "clear-point") else "element"
            body = {"observe": False, "steps": [{"kind": "action", "action": {
                "type": "tap_locator", "locator": {"label": "Done"}, "via": via}}]}
            req = urllib.request.Request(f"http://127.0.0.1:{port}/agent/actions",
                    data=json.dumps(body).encode(), headers=dict(auth, **{
                        "x-phone-owner": owner, "x-phone-control": "1", "Content-Type": "application/json"}))
            try:
                response = opener.open(req, timeout=12)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                receipt.update(status=response.status, response=json.load(response))
        except Exception as error:
            receipt["execution_error"] = str(error)
        finally:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
            provider.shutdown()
            provider.server_close()
            thread.join(timeout=2)
            receipt["owned_process_stopped"] = process is not None and process.poll() is not None
            receipt["requests"] = requests
            log.write_text(log.read_text(errors="replace").replace(token, "REDACTED") if log.exists() else "")
    try:
        assert receipt["owned_process_stopped"], "owned daemon was not stopped"
        assert "execution_error" not in receipt, receipt.get("execution_error")
        body = receipt["response"]
        taps = [r for r in requests if r["method"] == "POST" and r["path"].endswith(("/actions", "/click"))]
        scrolls = [r for r in requests if r["method"] == "POST" and r["path"].endswith("/scrollTo")]
        queries = [r for r in requests if r["method"] == "POST" and r["path"].endswith("/elements")]
        if case in ("partial-source", "unknown-source", "invalid-reference"):
            assert not taps and not scrolls, "unproven scope dispatched a mutation"
            expected = "runner_pre_dispatch_failed" if case == "invalid-reference" else "target_scope_unproven"
            assert body.get("error") == expected and body.get("ok") is False, body
            assert body.get("outcome") == "not_sent" and body.get("retry_safe") is True, body
        elif case in ("covered-point", "query-lost-after-reveal"):
            assert not taps and len(scrolls) == 1 and len(queries) == 2, requests
            assert body.get("ok") is False and body.get("retry_safe") is False, body
            assert body.get("outcome") != "not_sent", body
        else:
            assert body.get("ok") is True and len(taps) == 1, body
            if case == "replaced-handle":
                assert len(scrolls) == 1 and taps[0]["path"].endswith("/B/click"), requests
            elif case == "stale-source-clear-live":
                assert not scrolls and taps[0]["path"].endswith("/A/click"), requests
            else:
                assert taps[0]["path"].endswith("/actions"), requests
                assert len(scrolls) == (1 if case == "covered-default" else 0), requests
                movements = [step for device in taps[0]["body"]["actions"] for step in device["actions"]
                             if step.get("type") == "pointerMove"]
                assert len(movements) == 1, movements
                y = movements[0]["y"]
                assert 820 < y <= 831 if case == "covered-default" else abs(y - 445.5) <= 1, movements
        receipt["passed"] = True
    except AssertionError as error:
        receipt["error"] = str(error)
    (out / (case + ".json")).write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", type=Path, required=True)
    parser.add_argument("--expect-version", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary = args.daemon.resolve()
    version = subprocess.check_output([str(binary), "--version"], text=True, timeout=10).strip()
    assert version == "iphone-use " + args.expect_version, version
    args.out.mkdir(parents=True, exist_ok=False)
    results = [run_case(binary, case, args.out) for case in CASES]
    summary = {"passed": all(case["passed"] for case in results), "version": version,
               "daemon_sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
               "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               "scope": "Real host daemon with isolated synthetic provider;not Native producer or physical proof",
               "cases": [{k: case.get(k) for k in ("name", "passed", "error", "execution_error", "owned_process_stopped")} for case in results]}
    (args.out / "result.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"passed": summary["passed"], "cases": summary["cases"]}))
    return 0 if summary["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
