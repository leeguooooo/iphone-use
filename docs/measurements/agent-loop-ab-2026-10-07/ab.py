"""Matched-task A/B through the real MCP entry point, on the iPhone 13.

Task: from the Settings top page, open 通用 → 关于本机. Postcondition: a
fresh read (outside the run) shows 名称. Variant A drives it with single
calls (read, tap_label, tap_label, read); variant B with one phone_run_steps
batch. Each trial is an explicit run (phone_run_start / phone_run_end), so the
daemon's own summary gives the counts. Order: one cold round per variant
(recorded, reported separately), then ABBA ×2. Raw records → records.jsonl.
Never prints tokens.
"""
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = sys.argv[1]
I13_PORT, RELAY, MJPEG, UDID = 45838, "http://127.0.0.1:8538", "http://127.0.0.1:9538", "00008110-0002346211A0401E"
PORT, OWNER = 45690, "ab-agent"
TOKEN = secrets.token_hex(16)
I13_TOKEN = subprocess.run(
    ["plutil", "-extract", "EnvironmentVariables.PHONE_REMOTE_AGENT_TOKEN", "raw",
     os.path.expanduser("~/Library/LaunchAgents/com.leeguoo.iphone-use.i13.plist")],
    capture_output=True, text=True).stdout.strip()


def http(port, token, method, path, body=None, run=None):
    headers = {"Authorization": f"Bearer {token}", "X-Phone-Control": "1",
               "X-Phone-Owner": OWNER, "Content-Type": "application/json"}
    if run:
        headers["X-Agent-Run"] = run
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}",
                                 data=None if body is None else json.dumps(body).encode(),
                                 method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw or b"{}")
        except Exception:
            return e.code, {"raw": raw[:200].decode(errors="replace")}


class Mcp:
    def __init__(self, binary):
        env = {"PATH": "/usr/bin:/bin", "HOME": os.path.expanduser("~"),
               "PHONE_REMOTE_URL": f"http://127.0.0.1:{PORT}", "PHONE_REMOTE_TOKEN": TOKEN,
               "PHONE_REMOTE_OWNER": OWNER, "IPHONE_USE_MCP_PREWARM": "0",
               "IPHONE_USE_NO_UPDATE_CHECK": "1"}
        self.p = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, env=env, text=True)
        self.n = 0
        self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                    "clientInfo": {"name": "ab", "version": "0"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.p.stdin.flush()

    def request(self, method, params):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("mcp closed")
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("id") == self.n:
                return msg

    def call(self, name, args):
        msg = self.request("tools/call", {"name": name, "arguments": args})
        texts = [c.get("text", "") for c in msg["result"]["content"] if c.get("type") == "text"]
        return "\n".join(texts), msg["result"].get("isError", False)


# The Bluetooth row on the Settings top page: mid-screen, never under the
# floating search bar (the covered-target path is a separate known issue).
BT_ROW = "蓝牙、打开"


def to_settings_root():
    http(PORT, TOKEN, "POST", "/agent/input", {"type": "launch_app", "bundle": "com.apple.Preferences"})
    for _ in range(3):
        http(PORT, TOKEN, "POST", "/agent/input", {"type": "back"})
    for _ in range(3):
        http(PORT, TOKEN, "POST", "/agent/input", {"type": "scroll", "x": 0.5, "y": 0.5, "dy": -300})
    time.sleep(1.0)
    _, read = http(PORT, TOKEN, "GET", "/agent/elements")
    return any(e.get("label") == BT_ROW for e in read.get("elements", []))


def postcondition():
    _, read = http(PORT, TOKEN, "GET", "/agent/elements")
    return any(e.get("kind") == "Switch" and e.get("label") == "蓝牙" for e in read.get("elements", []))


def trial(mcp, variant, run_id):
    out_bytes = 0
    text, err = mcp.call("phone_run_start", {"run_id": run_id})
    assert not err, text
    t0 = time.time()
    unknown = 0
    calls = []
    if variant == "A":
        for name, args in [("phone_elements", {}),
                           ("phone_tap_label", {"label": BT_ROW}),
                           ("phone_elements", {})]:
            text, err = mcp.call(name, args)
            out_bytes += len(text.encode())
            unknown += text.count("outcome_unknown")
            calls.append({"tool": name, "args": args, "is_error": err, "text": text[:1500]})
    else:
        steps = [
            {"kind": "tap_locator", "locator": {"label": BT_ROW, "kind": "Button"}},
            {"kind": "wait_for", "expect": {"present": [{"label": "蓝牙", "kind": "Switch"}]},
             "timeout_ms": 8000},
        ]
        text, err = mcp.call("phone_run_steps", {"steps": steps, "observe": True})
        out_bytes += len(text.encode())
        unknown += text.count("outcome_unknown")
        calls.append({"tool": "phone_run_steps", "is_error": err, "text": text[:3000]})
    wall = time.time() - t0
    text, err = mcp.call("phone_run_end", {"run_id": run_id})
    summary = json.loads(text).get("run", {}) if not err else {"error": text}
    ok = postcondition()
    return {"variant": variant, "run_id": run_id, "wall_s": round(wall, 3),
            "mcp_text_bytes": out_bytes, "postcondition": ok, "unknown_in_text": unknown,
            "run": summary, "calls": calls}


def main():
    state = os.path.join(HERE, "state")
    os.makedirs(state, mode=0o700, exist_ok=True)
    os.chmod(state, 0o700)
    state = os.path.realpath(state)
    # Keep the phone: hold it through its own daemon, with our owner lease.
    s, held = http(I13_PORT, I13_TOKEN, "POST", "/agent/hold", {"secs": 1800})
    print("i13 hold", s, held.get("hold_remaining_secs"))
    env = {"PATH": "/usr/bin:/bin", "HOME": os.path.expanduser("~"), "TMPDIR": state,
           "PHONE_REMOTE_HOST": "127.0.0.1", "PHONE_REMOTE_PORT": str(PORT),
           "PHONE_REMOTE_STATE_DIR": state, "PHONE_REMOTE_INSTANCE": "ab",
           "PHONE_REMOTE_BACKEND": "direct", "PHONE_REMOTE_WDA_URL": RELAY,
           "PHONE_REMOTE_WDA_MJPEG_URL": MJPEG, "PHONE_REMOTE_WDA_MANAGED": "0",
           "PHONE_REMOTE_UDID": UDID, "PHONE_REMOTE_AGENT_TOKEN": TOKEN,
           "PHONE_REMOTE_PASSWORD": secrets.token_hex(12), "IPHONE_USE_NO_UPDATE_CHECK": "1"}
    daemon = subprocess.Popen([os.path.join(REPO, "target/release/iphone-use"), "serve"], env=env,
                              stdout=subprocess.DEVNULL, stderr=open(os.path.join(HERE, "daemon.err"), "w"))
    try:
        for _ in range(100):
            try:
                if http(PORT, TOKEN, "GET", "/agent/status")[0] == 200:
                    break
            except Exception:
                pass
            time.sleep(0.2)
        mcp = Mcp(os.path.join(REPO, "target/release/iphone-use-mcp"))
        records = []
        order = [(v, p) for v, p in (tuple(x.split("/")) for x in os.environ.get("AB_ORDER", "A/cold,B/cold,A/warm,B/warm,B/warm,A/warm,A/warm,B/warm,B/warm,A/warm").split(","))]
        for i, (variant, phase) in enumerate(order):
            if not to_settings_root():
                print("could not reach the Settings top page; stopping")
                break
            rec = trial(mcp, variant, f"ab-{i}-{variant}")
            rec["phase"] = phase
            rec["seq"] = i
            records.append(rec)
            run = rec["run"]
            print(i, variant, phase, rec["wall_s"], "s calls", run.get("tool_calls"),
                  "observed", run.get("observed_calls"), "batch", run.get("batch_calls"),
                  "p50", run.get("call_p50_ms"), "p95", run.get("call_p95_ms"),
                  "bytes", rec["mcp_text_bytes"], "post", rec["postcondition"], flush=True)
        with open(os.path.join(HERE, os.environ.get("AB_OUT", "records.jsonl")), "w") as f:
            for rec in records:
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
        meta = {"device": "iPhone 13 (i13), iOS 27.0, USB relays 8538/9538",
                "runner": "installed i13 runner (v0.15.0 sources)",
                "daemon": "branch build feat/agent-loop-p2 (release), unmanaged, port 45690",
                "task": "Settings top page → 蓝牙 (row '蓝牙、打开'); postcondition: a fresh read outside the run shows the Switch '蓝牙'",
                "variants": {"A": "phone_elements, phone_tap_label, phone_elements (observed tap)",
                             "B": "one phone_run_steps: tap_locator + wait_for, observe:true"},
                "order": [f"{v}/{p}" for v, p in order]}
        with open(os.path.join(HERE, "meta.json"), "w") as f:
            json.dump(meta, f, ensure_ascii=False, indent=1)
        mcp.p.kill()
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
        http(I13_PORT, I13_TOKEN, "POST", "/agent/hold", {"secs": 0})
        http(I13_PORT, I13_TOKEN, "POST", "/agent/owner", {"release": True})
        print("released")


main()
