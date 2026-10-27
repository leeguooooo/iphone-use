#!/usr/bin/env python3
"""Actual daemon + stdio MCP delivery contracts against an owned loopback Runner.

No device, model, signing, reconnect or native XCTest execution. The Runner
simulates an effect before an error/lost reply; its independent request/effect
ledger detects client/daemon replay and batch continuation.
"""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("gesture_protocol", ROOT / "scripts/owned-mcp-fixture.py")
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)
OWNER = "gesture-delivery-entry"
PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="


class OwnedPeer(protocol.Peer):
    def __init__(self, *args):
        try:
            super().__init__(*args)
        except BaseException:
            if hasattr(self, "process"):
                protocol.stop(self.process)
            raise


class Native(http.server.ThreadingHTTPServer):
    daemon_threads = False
    block_on_close = True

    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(5)
        return connection, address

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.mode, self.fail_at, self.requests, self.effects = "success", 1, [], 0
        self.errors = []

    def arm(self, mode, fail_at=1):
        with self.lock:
            self.mode, self.fail_at, self.requests, self.effects = mode, fail_at, [], 0

    def receipt(self):
        with self.lock:
            return {"requests":list(self.requests), "simulated_effects":self.effects, "errors":list(self.errors)}


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def respond(self, value, status=200):
        raw = json.dumps({"value":value}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(raw)
        except (BrokenPipeError, ConnectionResetError):
            # The daemon can cancel a readonly recovery request after failure.
            self.close_connection = True

    def do_GET(self):
        self.connection.settimeout(5)
        path = self.path.split("?", 1)[0]
        if path == "/status":
            value = {"ready":True, "busy":False}
        elif path.endswith("/locked"):
            value = False
        elif path.endswith("/window/size"):
            value = {"width":390, "height":844}
        elif path.endswith("/wda/activeAppInfo"):
            value = {"bundleId":"com.leeguoo.iphone-use.scripted-gesture", "pid":99999, "name":"Scripted gesture"}
        elif path.endswith("/source"):
            value = {"type":"XCUIElementTypeApplication", "name":"Scripted gesture", "pid":99999, "rect":{"x":0,"y":0,"width":390,"height":844}, "children":[]}
        elif path.endswith("/screenshot"):
            value = PNG
        elif path.endswith("/wda/apps/list"):
            value = []
        elif path.startswith("/session/") and path.count("/") == 2:
            value = {"capabilities":{"ipu:timedDragPath":True}}
        else:
            value = None
        self.respond(value)

    def do_POST(self):
        self.connection.settimeout(5)
        length = int(self.headers.get("Content-Length", "0"))
        if not 0 <= length <= 65536:
            self.respond({"error":"invalid argument"}, 400)
            return
        body = json.loads(self.rfile.read(length) or b"{}")
        if self.path == "/session":
            self.respond({"sessionId":"S", "capabilities":{"ipu:timedDragPath":True}})
            return
        if self.path.endswith("/appium/settings"):
            self.respond(None)
            return
        if self.path != "/session/S/actions":
            with self.server.lock:
                self.server.errors.append("unexpected mutation: " + self.path)
            self.respond({"error":"unknown command"}, 404)
            return
        with self.server.lock:
            self.server.requests.append({"path":self.path, "body":body})
            mode = self.server.mode if len(self.server.requests) == self.server.fail_at else "success"
            if mode != "refused":
                self.server.effects += 1
        if mode == "lost":
            self.close_connection = True
            self.connection.shutdown(socket.SHUT_RDWR)
            return
        if mode in ("error", "error-in-200"):
            self.respond({"error":"unknown error", "message":"actions tap synthesis outcome unknown: injected after simulated delivery; no coordinate fallback attempted"}, 200 if mode == "error-in-200" else 500)
        elif mode == "refused":
            self.respond({"error":"invalid argument", "message":"test point outside current viewport; nothing was sent"}, 400)
        else:
            self.respond(None)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--daemon", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--structured", choices=["default", "full"], default="default")
    args = parser.parse_args()
    args.daemon, args.mcp = args.daemon.resolve(strict=True), args.mcp.resolve(strict=True)
    args.out = args.out.resolve()
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    token = secrets.token_hex(32)
    state = args.out / "private-state"
    runtime = args.out / "private-runtime"
    state.mkdir(mode=0o700); runtime.mkdir(mode=0o700)
    (state / "runner-token").write_text(token); (state / "runner-token").chmod(0o600)
    environment = {k:v for k,v in os.environ.items() if not k.startswith(("IPHONE_USE_", "PHONE_REMOTE_", "IPU_", "TEST_RUNNER_"))}
    native = Native()
    native_thread = threading.Thread(target=native.serve_forever, kwargs={"poll_interval":.05})
    native_thread.start()
    port = protocol.port()
    base = "http://127.0.0.1:" + str(port)
    runner = "http://127.0.0.1:" + str(native.server_port)
    environment.update(IPHONE_USE_HOST="127.0.0.1", IPHONE_USE_PORT=str(port), IPHONE_USE_UDID="00000000-0000000000000000", IPHONE_USE_INSTANCE="gesture-entry", IPHONE_USE_STATE_DIR=str(state), TMPDIR=str(runtime), IPHONE_USE_RUNNER_URL=runner, IPHONE_USE_RUNNER_MJPEG_URL=runner, IPHONE_USE_RUNNER_MANAGED="0", IPHONE_USE_AGENT_TOKEN=token, IPHONE_USE_PASSWORD="test-"+token, IPHONE_USE_AUTO_FOCUS="0", IPHONE_USE_KEEP_AWAKE_SECS="0", IPHONE_USE_IDLE_RELEASE_SECS="0", IPHONE_USE_FLOWS_NO_AUTO_UPDATE="1", IPHONE_USE_NO_UPDATE_CHECK="1")
    daemon = peer = None
    reports, cleanup_errors = [], []
    error = None

    def api(path, body=None):
        raw = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(base+path, data=raw, headers={"Authorization":"Bearer "+token, "X-Phone-Control":"1", "X-Phone-Owner":OWNER, "Content-Type":"application/json"})
        try:
            response = urllib.request.urlopen(request, timeout=15)
        except urllib.error.HTTPError as failed:
            response = failed
        with response:
            return response.status, json.load(response)

    def ready():
        until = time.monotonic()+30
        while True:
            if daemon.poll() is not None:
                raise RuntimeError("owned daemon exited; inspect daemon.log")
            try:
                _, status = api("/agent/status")
                if status.get("owner") not in (None, OWNER) or status.get("screen_private_secs", 0) != 0:
                    raise RuntimeError("unexpected owner/privacy state")
                if status.get("drivable") is True:
                    return
            except (urllib.error.URLError, ConnectionError):
                pass
            if time.monotonic() > until:
                raise RuntimeError("scripted runner did not become drivable")
            time.sleep(.1)

    def trial(entry, action, mode, prefix=False):
        ready()
        if reports:
            assert native.receipt() == reports[-1]["runner_receipt"], "native input arrived after the previous response"
        native.arm(mode, 2 if prefix else 1)
        steps = ([{"type":"tap", "x":.2, "y":.3}] if prefix else []) + [action] + ([{"type":"key", "name":"return"}] if prefix else [])
        if entry == "http-single":
            status, body = api("/agent/input", action)
            reply = {"status":status, "body":body}
        elif entry == "http-batch":
            status, body = api("/agent/actions", {"steps":[{"kind":"action", "action":x} for x in steps]})
            reply = {"status":status, "body":body}
        else:
            if entry == "mcp-single":
                name, arguments = "phone_tap", {"x":.5,"y":.5,"observe":False}
            else:
                name, arguments = "phone_run_steps", {"steps":[dict(kind=x["type"], **{k:v for k,v in x.items() if k!="type"}) for x in steps], "observe":False}
            reply = peer.request("tools/call", {"name":name, "arguments":arguments}, timeout=30)
            body = protocol.payload(reply)[2]
        receipt = native.receipt()
        reports.append({"entry":entry,"action":action,"mode":mode,"prefix":prefix,"reply":reply,"runner_receipt":receipt,"passed":False})
        expected = 2 if prefix else 1
        assert not receipt["errors"], receipt
        assert len(receipt["requests"]) == expected, (entry, mode, body, receipt)
        assert receipt["simulated_effects"] == expected - (mode == "refused"), receipt
        if mode in ("error", "error-in-200", "lost"):
            assert body.get("outcome") == "unknown" and body.get("retry_safe") is False, (entry,body)
        elif mode == "refused":
            assert body.get("outcome") == "not_sent", body
            assert body.get("retry_safe") is (not prefix), body
        else:
            assert body.get("ok") is True, body
        if prefix:
            assert body.get("failed_step") == 1 and body.get("applied_actions") == 1, body
            assert body.get("batch_outcome") == ("partially_applied" if mode == "refused" else "unknown"), body
        if entry.startswith("mcp"):
            assert reply.get("result",{}).get("isError",False) is (mode != "success"), reply
        reports[-1]["passed"] = True
        print(json.dumps({"entry":entry,"action":action["type"],"mode":mode,"prefix":prefix,"passed":True}), flush=True)

    try:
        with (args.out/"daemon.log").open("w") as log:
            daemon = subprocess.Popen([str(args.daemon),"serve"], env=environment, stdout=log, stderr=log, start_new_session=True)
        ready()
        child_env = dict(environment, IPHONE_USE_URL=base, IPHONE_USE_TOKEN=token, IPHONE_USE_OWNER=OWNER, IPHONE_USE_MCP_PREWARM="0")
        if args.structured == "full": child_env["IPHONE_USE_MCP_STRUCTURED"] = "full"
        peer = OwnedPeer([str(args.mcp)], child_env, args.out/"mcp.log")
        actions = [
            {"type":"tap", "x":.5,"y":.5},
            {"type":"longpress", "x":.5,"y":.5,"duration_ms":500},
            {"type":"swipe", "x1":.3,"y1":.5,"x2":.7,"y2":.5,"duration_ms":300},
            {"type":"multi_longpress", "points":[{"x":.3,"y":.5},{"x":.7,"y":.5}],"duration_ms":500},
        ]
        for action in actions:
            for mode in ["success","error","error-in-200","lost","refused"]:
                trial("http-single", action, mode)
        for mode in ["success","error","error-in-200","lost","refused"]:
            trial("mcp-single", actions[0], mode)
        for action in actions[1:]:
            for mode in ["error","error-in-200","lost"]:
                trial("mcp-batch", action, mode)
        for entry in ["http-batch","mcp-batch"]:
            for mode in ["error","error-in-200","lost","refused"]:
                trial(entry, actions[0], mode, prefix=True)
    except Exception as caught:
        error = repr(caught)
    finally:
        if peer:
            try:
                if peer.pending is None: peer.close()
                else: protocol.stop(peer.process)
            except Exception as caught: cleanup_errors.append("MCP: "+str(caught))
        if daemon:
            try: protocol.stop(daemon)
            except Exception as caught: cleanup_errors.append("daemon: "+str(caught))
        final_receipt = native.receipt()
        if reports and final_receipt != reports[-1]["runner_receipt"]:
            cleanup_errors.append("native request/effect changed after the last recorded response")
        native.shutdown(); native.server_close(); native_thread.join(timeout=5)
        if native_thread.is_alive(): cleanup_errors.append("Runner thread still alive")
        children_stopped = (daemon is None or daemon.poll() is not None) and (peer is None or peer.process.poll() is not None)
        if children_stopped:
            for directory in [state, runtime]:
                try: shutil.rmtree(directory)
                except OSError as caught: cleanup_errors.append("private state cleanup: "+str(caught))
        else:
            cleanup_errors.append("private state retained: a child is still live")
    cleanup = {"daemon_stopped":daemon is None or daemon.poll() is not None, "mcp_stopped":peer is None or peer.process.poll() is not None, "runner_thread_stopped":not native_thread.is_alive(), "runner_socket_closed":native.socket.fileno() == -1, "private_state_removed":not state.exists(), "private_runtime_removed":not runtime.exists()}
    result = {"scope":"Real CLI daemon HTTP and stdio MCP with simulated native effects/errors; no XCTest/device/model acceptance", "mcp_structure":args.structured,"cases":reports,"harness_error":error,"cleanup_errors":cleanup_errors,"cleanup":cleanup,"final_runner_receipt":final_receipt,"binary_sha256":{k:hashlib.sha256(getattr(args,k).read_bytes()).hexdigest() for k in ["daemon","mcp"]},"passed":len(reports)==42 and error is None and not cleanup_errors and all(cleanup.values())}
    (args.out/"result.json").write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n")
    print(json.dumps({k:result[k] for k in ["passed","harness_error","cleanup_errors"]}), flush=True)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
