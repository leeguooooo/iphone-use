#!/usr/bin/env python3
"""Own a disposable Simulator and bridge real MCP replies into the packaged panel.

The host shell is synthetic; native Runner, daemon, stdio MCP, H.264/PNG and
browser canvas are real. No personal device, client configuration or user tab
is selected. A ready server is not browser acceptance; inspect/capture its live
counter and independent App receipt, then POST /stop from the same origin.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import subprocess
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from runner_auth import authorization

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("owned_live_fixture", ROOT / "scripts/test-input-target-simulator.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
HOST = '''<!doctype html><meta charset="utf-8"><title>Owned Simulator live MCP panel</title>
<p>Owned Simulator · live native/MCP transport · synthetic host</p>
<button id="hide">Private hold</button><button id="show">Resume</button><output id="state">public</output>
<iframe id="panel" width="380" height="820" sandbox="allow-scripts allow-same-origin"></iframe>
<script>
const mode=new URLSearchParams(location.search).get('resources')==='1'?'workbuddy':'generic';
const frame=document.getElementById('panel');frame.src='/screen?mode='+mode;
async function hold(secs){const r=await fetch('/hold',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({secs})});if(!r.ok)throw new Error('hold refused');document.getElementById('state').textContent=secs?'private':'public';}
document.getElementById('hide').onclick=()=>hold(60);document.getElementById('show').onclick=()=>hold(0);
window.addEventListener('message',async event=>{
 if(event.source!==frame.contentWindow||!event.data||event.data.id===undefined)return;
 const request=event.data;let reply;
 try {
  if(request.method==='ui/initialize')reply={result:{hostContext:{}}};
  else {const r=await fetch('/rpc',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({mode,request})});reply=await r.json();}
 }catch(_){reply={error:{code:-32000,message:'owned fixture bridge failed'}};}
 frame.contentWindow.postMessage({jsonrpc:'2.0',id:request.id,...reply},'*');
});
</script>'''


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop_process(process):
    if process and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


class Context:
    def __init__(self, args, out, env, token, daemon_port, documents):
        self.args, self.out, self.env, self.token = args, out, env, token
        self.daemon_url = f"http://127.0.0.1:{daemon_port}"
        self.documents = documents
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.peers, self.peer_locks, self.html, self.stats = {}, {}, {}, {}
        self.stats_lock = threading.Lock()
        self.reason = "deadline"

    def api(self, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        headers = {"Authorization": "Bearer " + self.token, "X-Phone-Owner": "owned-browser-control", "Content-Type": "application/json"}
        if body is not None:
            headers["X-Phone-Control"] = "1"
        request = urllib.request.Request(self.daemon_url + path, data=data, headers=headers)
        with self.opener.open(request, timeout=15) as response:
            return json.load(response)

    def counter(self, number):
        assert type(number) is int and 1 <= number <= 999999
        nonce = secrets.token_hex(8)
        target = self.documents / "live-video-command.json"
        temp = target.with_suffix(".tmp")
        temp.write_text(json.dumps({"counter": number, "nonce": nonce}))
        temp.replace(target)
        until = time.monotonic() + 5
        while time.monotonic() < until:
            receipt = self.oracle()
            if receipt.get("nonce") == nonce and receipt.get("counter") == number:
                return receipt
            time.sleep(0.05)
        raise RuntimeError("App did not acknowledge live counter")

    def oracle(self):
        path = self.documents / "live-video-state.json"
        return json.loads(path.read_text()) if path.exists() else {}

    def start_peers(self):
        for mode in ["generic", "workbuddy"]:
            env = dict(self.env)
            env.update(IPHONE_USE_URL=self.daemon_url, IPHONE_USE_TOKEN=self.token,
                       IPHONE_USE_OWNER="live-browser-" + mode, IPHONE_USE_MCP_HOST=mode,
                       IPHONE_USE_MCP_TOOLS="readonly", IPHONE_USE_MCP_STRUCTURED="full", IPHONE_USE_MCP_PREWARM="0")
            peer = fixture.VideoPeer(self.args.mcp.resolve(), env, self.out / (mode + "-mcp.log"))
            self.peers[mode] = peer
            self.peer_locks[mode] = threading.Lock()
            peer.initialize()
            resources = peer.request("resources/list", {})["result"]["resources"]
            uri = next(item["uri"] for item in resources if item["uri"].startswith("ui://iphone-use/screen-"))
            self.html[mode] = peer.request("resources/read", {"uri": uri})["result"]["contents"][0]["text"]
            self.stats[mode] = {"calls": 0, "tool_calls": 0, "resource_reads": 0, "video_packets": 0,
                                "video_bytes": 0, "png_frames": 0, "clear_frames": 0, "errors": 0,
                                "panel_sha256": hashlib.sha256(self.html[mode].encode()).hexdigest()}

    def rpc(self, mode, request):
        if mode not in self.peers:
            raise ValueError("unknown fixture mode")
        method, params = request.get("method"), request.get("params", {})
        if method == "tools/call":
            if mode != "generic" or params.get("name") != "phone_screen_frame":
                raise ValueError("only the read-only panel frame tool is allowed")
            arguments = params.get("arguments", {})
            if not isinstance(arguments, dict) or not set(arguments) <= {"video", "reset", "fresh"}:
                raise ValueError("unexpected frame arguments")
        elif method == "resources/read":
            uri = params.get("uri", "")
            if mode != "workbuddy" or not isinstance(uri, str) or not uri.startswith(("ui://iphone-use/video-v1/", "ui://iphone-use/frame-v1/")):
                raise ValueError("only the read-only frame resource is allowed")
        else:
            raise ValueError("unsupported host method")
        with self.peer_locks[mode]:
            reply = self.peers[mode].request(method, params)
        body = None
        result = reply.get("result", {})
        if method == "tools/call":
            body = result.get("structuredContent")
        elif result.get("contents"):
            body = json.loads(result["contents"][0]["text"])
        with self.stats_lock:
            stats = self.stats[mode]
            stats["calls"] += 1
            stats["tool_calls" if method == "tools/call" else "resource_reads"] += 1
            if isinstance(body, dict):
                video = body.get("video") or {}
                packets = video.get("packets") or []
                stats["video_packets"] += len(packets)
                stats["video_bytes"] += sum(len(base64.b64decode(packet["data"])) for packet in packets)
                stats["png_frames"] += int(bool(body.get("frame")))
                stats["clear_frames"] += int(bool(body.get("clear_frame")))
            stats["errors"] += int("error" in reply or result.get("isError", False))
        return {key: value for key, value in reply.items() if key in ["result", "error"]}


class Handler(BaseHTTPRequestHandler):
    def respond(self, status, value, mime="application/json"):
        data = value.encode() if isinstance(value, str) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; frame-src 'self'; connect-src 'self'; frame-ancestors 'none'" if not self.path.startswith("/screen") else "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'")
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # A closed browser must not cancel/replay an already issued MCP read.

    def do_GET(self):
        ctx = self.server.context
        path = urllib.parse.urlsplit(self.path)
        if path.path == "/":
            return self.respond(200, HOST, "text/html; charset=utf-8")
        if path.path == "/screen":
            mode = urllib.parse.parse_qs(path.query).get("mode", ["generic"])[0]
            if mode in ctx.html:
                return self.respond(200, ctx.html[mode], "text/html; charset=utf-8")
        if path.path == "/state":
            with ctx.stats_lock:
                stats = json.loads(json.dumps(ctx.stats))
            return self.respond(200, {"oracle": ctx.oracle(), "stats": stats, "owner": ctx.api("/agent/status").get("owner")})
        self.respond(404, {"error": "not found"})

    def do_POST(self):
        ctx = self.server.context
        origin = f"http://127.0.0.1:{self.server.server_port}"
        if self.headers.get("Origin") != origin or self.headers.get("Content-Type", "").split(";")[0] != "application/json":
            return self.respond(403, {"error": "same-origin JSON required"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 65536:
                raise ValueError("invalid body size")
            body = json.loads(self.rfile.read(length))
            if self.path == "/rpc":
                return self.respond(200, ctx.rpc(body.get("mode"), body.get("request", {})))
            if self.path == "/counter":
                return self.respond(200, ctx.counter(body.get("counter")))
            if self.path == "/hold":
                secs = body.get("secs")
                if type(secs) is not int or secs not in [0, 60]:
                    raise ValueError("invalid fixture hold")
                return self.respond(200, ctx.api("/agent/hold", {"secs": secs, "private": True}))
            if self.path == "/stop":
                ctx.reason = "requested"
                self.respond(200, {"stopping": True})
                threading.Thread(target=self.server.shutdown, daemon=True).start()
                return
            self.respond(404, {"error": "not found"})
        except Exception as error:
            self.respond(400 if isinstance(error, (ValueError, AssertionError)) else 502,
                         {"error": {"code": -32000, "message": "owned bridge refused or failed", "type": type(error).__name__}})

    def log_message(self, *_):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--xctestrun", required=True, type=Path)
    parser.add_argument("--daemon", required=True, type=Path)
    parser.add_argument("--mcp", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--max-seconds", type=int, default=900)
    args = parser.parse_args()
    if not 30 <= args.max_seconds <= 1800:
        parser.error("max-seconds must be 30...1800")
    out = args.out.resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("IPHONE_USE_", "PHONE_REMOTE_", "IPU_", "TEST_RUNNER_IPU_", "SIMCTL_CHILD_"))}
    token = secrets.token_hex(32)
    runner_port, daemon_port = free_port(), free_port()
    video_port = free_port()
    while video_port in [runner_port, daemon_port]:
        video_port = free_port()
    runner = daemon = server = timer = ctx = None
    udid = None
    result = {"scope": "owned Simulator -> native Runner -> daemon -> stdio MCP -> browser host bridge; readiness is not browser acceptance"}
    cleanup = []
    def setup_interrupted(*_):
        raise InterruptedError("owned live fixture interrupted during setup")
    signal.signal(signal.SIGINT, setup_interrupted)
    signal.signal(signal.SIGTERM, setup_interrupted)
    try:
        app = fixture.build_fixture(out)
        udid = fixture.command("xcrun", "simctl", "create", "ipu-live-browser-" + secrets.token_hex(4), "com.apple.CoreSimulator.SimDeviceType.iPhone-17", "com.apple.CoreSimulator.SimRuntime.iOS-27-0")
        result["simulator"] = udid
        fixture.command("xcrun", "simctl", "boot", udid)
        fixture.command("xcrun", "simctl", "bootstatus", udid, "-b", timeout=180)
        fixture.command("xcrun", "simctl", "install", udid, app)
        runner_env = dict(env)
        runner_env.update(TEST_RUNNER_IPU_RUNNER_PORT=str(runner_port), TEST_RUNNER_IPU_RUNNER_MJPEG_PORT=str(video_port), TEST_RUNNER_IPU_RUNNER_TOKEN=token, TEST_RUNNER_IPU_DAEMON_PORT="0")
        with (out / "runner.log").open("w") as log:
            runner = subprocess.Popen(["xcodebuild", "test-without-building", "-xctestrun", str(args.xctestrun.resolve()), "-destination", "id=" + udid, "-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe", "-parallel-testing-enabled", "NO"], env=runner_env, stdout=log, stderr=log, start_new_session=True)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        until = time.monotonic() + 120
        while True:
            try:
                request = urllib.request.Request(f"http://127.0.0.1:{runner_port}/status", headers={"Authorization": authorization(token, "GET", "/status", b"")})
                with opener.open(request, timeout=2) as response:
                    native_status = json.load(response)["value"]
                    if native_status.get("ready"):
                        assert isinstance(native_status.get("h264"), dict), "native video listener unavailable"
                        break
            except OSError:
                pass
            assert runner.poll() is None and time.monotonic() < until, "owned runner readiness failed"
            time.sleep(0.2)
        fixture.command("xcrun", "simctl", "launch", udid, fixture.BUNDLE, "--live-video-fixture")
        documents = Path(fixture.command("xcrun", "simctl", "get_app_container", udid, fixture.BUNDLE, "data")) / "Documents"
        documents.mkdir(exist_ok=True)
        state, runtime = out / "daemon-state", out / "daemon-runtime"
        state.mkdir(mode=0o700)
        runtime.mkdir(mode=0o700)
        (state / "runner-token").write_text(token)
        (state / "runner-token").chmod(0o600)
        daemon_env = dict(env)
        daemon_env.update(IPHONE_USE_HOST="127.0.0.1", IPHONE_USE_PORT=str(daemon_port), IPHONE_USE_INSTANCE="live-" + udid[:8].lower(), TMPDIR=str(runtime), IPHONE_USE_STATE_DIR=str(state), IPHONE_USE_SIMULATOR=udid,
                          IPHONE_USE_RUNNER_URL=f"http://127.0.0.1:{runner_port}", IPHONE_USE_RUNNER_MJPEG_URL=f"http://127.0.0.1:{video_port}", IPHONE_USE_RUNNER_MANAGED="0", IPHONE_USE_AGENT_TOKEN=token, IPHONE_USE_PASSWORD="fixture-" + token,
                          IPHONE_USE_AUTO_FOCUS="0", IPHONE_USE_IDLE_RELEASE_SECS="0", IPHONE_USE_FLOWS_NO_AUTO_UPDATE="1")
        with (out / "daemon.log").open("w") as log:
            daemon = subprocess.Popen([str(args.daemon.resolve()), "serve"], env=daemon_env, stdout=log, stderr=log, start_new_session=True)
        ctx = Context(args, out, env, token, daemon_port, documents)
        until = time.monotonic() + 30
        while True:
            try:
                status = ctx.api("/agent/status")
                if status.get("drivable"):
                    assert status.get("target") == "simulator"
                    assert status.get("udid") == udid and status.get("instance") == "live-" + udid[:8].lower(), "daemon identity does not match the owned Simulator"
                    assert daemon.poll() is None, "owned daemon exited before identity confirmation"
                    break
            except OSError:
                pass
            assert daemon.poll() is None and time.monotonic() < until, "owned daemon readiness failed"
            time.sleep(0.1)
        first = ctx.counter(1001)
        result.update(simulator=udid, initial_oracle=first, owner_before=status.get("owner"))
        ctx.start_peers()
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        server.context = ctx
        ready = {"url": f"http://127.0.0.1:{server.server_port}/", "simulator": udid, "initial_oracle": first}
        (out / "ready.json").write_text(json.dumps(ready, indent=2) + "\n")
        print(json.dumps(ready), flush=True)
        timer = threading.Timer(args.max_seconds, server.shutdown)
        timer.daemon = True
        timer.start()
        def interrupted(*_):
            ctx.reason = "signal"
            threading.Thread(target=server.shutdown, daemon=True).start()
        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        server.serve_forever()
        result.update(shutdown_reason=ctx.reason, final_oracle=ctx.oracle(), stats=ctx.stats)
    except Exception as error:
        result.update(error_type=type(error).__name__, error=str(error), traceback=traceback.format_exc(limit=6))
    finally:
        if timer:
            timer.cancel()
        if server:
            server.server_close()
        if ctx:
            for peer in ctx.peers.values():
                try:
                    peer.close()
                except Exception as error:
                    cleanup.append(type(error).__name__)
        for process in [daemon, runner]:
            try:
                stop_process(process)
            except Exception as error:
                cleanup.append(type(error).__name__)
        for path in [out / "daemon-state", out / "daemon-runtime"]:
            if path.exists():
                shutil.rmtree(path)
        if udid:
            for operation in ["shutdown", "delete"]:
                try:
                    fixture.command("xcrun", "simctl", operation, udid)
                except Exception as error:
                    cleanup.append(str(error))
        for log in out.glob("*.log"):
            log.write_text(log.read_text(errors="replace").replace(token, "[fixture token redacted]"))
        result["cleanup_errors"] = cleanup
        result["sources"] = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest() for name in ["scripts/serve-mcp-live-browser-fixture.py", "scripts/test-input-target-simulator.py", "crates/mcp/src/screen.html", "crates/mcp/src/screen.rs", "crates/mcp/src/screen_video.rs", "crates/server/src/video.rs", "runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerRoutes.swift", "runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerH264.swift"]}
        for key, path in [("daemon_sha256", args.daemon), ("mcp_sha256", args.mcp), ("xctestrun_sha256", args.xctestrun), ("fixture_sha256", out / "InputGuard.app/InputGuard")]:
            if path.exists():
                result[key] = hashlib.sha256(path.read_bytes()).hexdigest()
        runner_binary = args.xctestrun.parent / "Debug-iphonesimulator/iPhoneUse-Runner.app/PlugIns/iPhoneUse.xctest/iPhoneUse"
        if runner_binary.exists():
            result["runner_binary_sha256"] = hashlib.sha256(runner_binary.read_bytes()).hexdigest()
        if (out / "runner.log").exists():
            log = (out / "runner.log").read_text()
            result["native_input_requests"] = len(re.findall(r"ipu-runner: POST \S*(?:/wda/keys|/ipu/text|/actions|/clear|/ipu/element/[^/]+/replace) ->", log))
        (out / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    return 0 if "error_type" not in result and not cleanup and result.get("shutdown_reason") == "requested" else 1


if __name__ == "__main__":
    raise SystemExit(main())
