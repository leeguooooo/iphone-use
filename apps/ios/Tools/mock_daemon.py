#!/usr/bin/env python3
"""A stand-in iphone-use daemon for checking the iOS remote in the Simulator.

It speaks just enough of the daemon's HTTP contract for the app to log in,
stream video and send gestures, without a phone, a Mac service or runner:

  POST /login, POST /pair            -> session cookie (any password / code)
  GET  /agent/status                 -> a scripted status (see SCENARIOS)
  GET  /agent/h264                   -> a recorded H.264 stream, replayed in a loop
  POST /agent/h264/keyframe          -> next message sent is a keyframe
  POST /control, POST /agent/mode    -> 200, and the action is printed (JSON line)
  GET  /agent/screenshot             -> one frame as JPEG
  POST /mock/scenario {"name": ...}  -> switch scenario while the app runs
  GET  /mock/actions                 -> every action received so far (JSON list)
  GET  /mock/info                    -> the stream's size and frame rate
  DELETE /mock/actions               -> forget them

Agent tasks, flows and schedules (docs/agent-task.md), scripted in memory:

  POST /agent/task, GET /agent/task/<id|current>?since=n, GET /agent/tasks,
  POST /agent/task/<id>/confirm|cancel -> a task that takes a step a second
      for 5 steps; a goal that pays, sends or deletes ("付", "发", "删",
      "pay", "send", "delete") stops at needs_confirm before step 3
  GET|POST /agent/flow/record, GET /agent/flow/draft, POST /agent/flow/save,
  GET /agent/flows, POST /agent/flow/run
  GET|POST /agent/schedules, PATCH|DELETE /agent/schedules/<id>,
  POST /agent/schedules/<id>/run, GET /agent/schedules/<id>/runs

The stream is made once from any video file (`--video`, default: the demo
screens bundled with the app as a slideshow) with ffmpeg, cached next to
this script as `.mock-stream.h264`. Needs python3 and ffmpeg; stdlib only.

  python3 apps/ios/Tools/mock_daemon.py --port 47001 --name "iPhone 15 Pro"
  xcrun simctl launch booted com.leeguoo.iphone-use.remote \
      -address http://127.0.0.1:47001 -password mock

Every gesture the app sends is printed as one JSON line on stdout, so a
check can assert on the normalized coordinates (e.g. after zooming in).
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs

HERE = Path(__file__).resolve().parent
DEMO = HERE.parent / "IPhoneUseRemote" / "Resources" / "Demo"

# What /agent/status says in each scenario, on top of BASE_STATUS.
SCENARIOS = {
    "live": {},
    # Frames stop while the status still says drivable: the app must show a
    # stall, not pretend the frozen frame is live.
    "stall": {},
    "released": {"released": True, "drivable": False, "device_state": "released"},
    "starting": {"reconnecting": True, "drivable": False, "device_state": "starting"},
    "locked": {"runner_locked": True, "drivable": False, "device_state": "locked",
               "setup_blocked_on": "locked"},
    "handoff": {"human_handoff": True, "drivable": False, "mode": "human"},
    # A start on a passcode phone with nobody there to enter it.
    "passcode": {"drivable": False, "device_state": "blocked", "recovery_owner": "daemon",
                 "setup_blocked_on": "needs_passcode_on_phone",
                 "next_step": {"zh": "手机设了锁屏密码：这次启动需要有人在手机上输入密码，允许 UI 自动化。之后保持连接就不会再问。",
                               "en": "This iPhone has a passcode: this start needs someone at the phone to enter it and allow UI Automation. After that, staying connected means it will not ask again."}},
    "owned": {"owner": "agent-loop", "owner_lease_remaining_secs": 240},
    # A known phone off usbmuxd since a restart (Before First Unlock).
    "restart-locked": {"drivable": False, "device_state": "blocked", "recovery_owner": "daemon",
                       "setup_blocked_on": "locked_after_restart",
                       "next_step": {"zh": "这台 iPhone 重启后还没解锁过：需要有人在手机上输入一次密码，之后会自动连上。",
                                     "en": "This iPhone restarted and has not been unlocked since: someone needs to enter the passcode on the phone once, then it connects on its own."}},
}

BASE_STATUS = {
    "ok": True, "backend": "direct", "drivable": True, "mode": "agent",
    "device_state": "ready", "released": False, "reconnecting": False,
    "releasing": False, "warming": False, "human_handoff": False,
    "runner_locked": False, "hint": "", "next_step": {}, "setup_blocked_on": "",
    "recovery_owner": "", "version": "0.17.12-mock", "capture_redacted": False,
    "owner": None, "owner_lease_remaining_secs": 0, "transport": "usb",
    "runner_rtt_ms": 18,
    "lock_readiness": {"verdict": "ready", "passcode_protected": True, "auto_lock_secs": "never"},
}


def build_stream(video, cache, fps, width):
    """Transcode `video` (or the demo slideshow) to Annex-B H.264 with an AUD
    before every access unit and SPS/PPS repeated on each keyframe."""
    if cache.exists():
        return cache.read_bytes()
    if not shutil.which("ffmpeg"):
        sys.exit("mock_daemon: ffmpeg is needed to build the stream (brew install ffmpeg)")
    enc = ["-c:v", "libx264", "-profile:v", "high", "-pix_fmt", "yuv420p", "-preset", "veryfast",
           "-tune", "zerolatency", "-g", str(fps * 2), "-bf", "0",
           "-x264-params", "aud=1:repeat-headers=1", "-bsf:v", "h264_mp4toannexb", "-an", "-f", "h264"]
    scale = f"scale={width}:-2"
    if video:
        cmd = ["ffmpeg", "-y", "-loglevel", "error", "-i", str(video), "-vf", f"{scale},fps={fps}",
               *enc, str(cache)]
    else:
        # Each demo screen for 3 s, whole (never cropped: the app's layout is
        # checked against the phone's own status bar). The encoder still
        # sends every frame, so fps and stalls show.
        shots = sorted(DEMO.glob("demo-*.jpg"))
        inputs, parts = [], []
        for i, shot in enumerate(shots):
            inputs += ["-loop", "1", "-t", "3", "-i", str(shot)]
            parts.append(
                f"[{i}:v]{scale},fps={fps},setsar=1,format=yuv420p[v{i}]")
        chain = "".join(f"[v{i}]" for i in range(len(shots)))
        graph = ";".join(parts) + f";{chain}concat=n={len(shots)}:v=1:a=0[out]"
        cmd = ["ffmpeg", "-y", "-loglevel", "error", *inputs, "-filter_complex", graph,
               "-map", "[out]", *enc, str(cache)]
    subprocess.run(cmd, check=True)
    return cache.read_bytes()


def probe_size(path):
    """(width, height) of the stream, or None without ffprobe."""
    if not shutil.which("ffprobe"):
        return None
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                          "stream=width,height", "-of", "csv=p=0", str(path)],
                         capture_output=True, text=True).stdout.strip()
    try:
        w, h = (int(v) for v in out.split(",")[:2])
        return w, h
    except ValueError:
        return None


def access_units(stream):
    """Split Annex-B at access unit delimiters; (is_keyframe, bytes) each."""
    marks = []
    i = stream.find(b"\x00\x00\x00\x01\x09")
    while i != -1:
        marks.append(i)
        i = stream.find(b"\x00\x00\x00\x01\x09", i + 5)
    units = []
    for n, start in enumerate(marks):
        end = marks[n + 1] if n + 1 < len(marks) else len(stream)
        au = stream[start:end]
        units.append((b"\x00\x00\x00\x01\x65" in au or b"\x00\x00\x01\x65" in au, au))
    return units


def frame_message(keyframe, pts_us, payload):
    body = bytes([1 if keyframe else 0]) + pts_us.to_bytes(8, "big") + payload
    return len(body).to_bytes(4, "big") + body


RISKY = ("付", "发", "删", "pay", "send", "delete")
NO_SCHEDULE = {"ok": False, "error": "no_such_schedule", "message": "no schedule with that id"}


class Agent:
    """Scripted agent tasks, flows and schedules, all in memory."""

    def __init__(self):
        self.lock = threading.Lock()
        self.tasks = []          # newest last
        self.recording = None    # the steps, while recording
        self.flows = [
            {"id": "settings/battery", "name": "Battery level", "steps": 4, "inputs": [],
             "app": "com.apple.Preferences", "source": "official", "verified": True, "risk": "read_only",
             "outputs": ["level"]},
            {"id": "messages/send-text", "name": "Send a message", "steps": 6, "inputs": ["to", "text"],
             "app": "com.apple.MobileSMS", "source": "official", "verified": True, "risk": "side_effect",
             "example_inputs": {"to": "Leo", "text": "hi"}},
        ]
        self.schedules = []
        self.runs = []
        self.seq = 0

    def next_id(self, prefix):
        self.seq += 1
        return f"{prefix}-{self.seq}"

    def start(self, body):
        with self.lock:
            running = next((t for t in self.tasks if t["status"] in ("running", "needs_confirm")), None)
            if running:
                return 409, {"error": "task_running", "id": running["id"]}
            goal = (body.get("goal") or "").strip()
            if not goal:
                return 400, {"error": "goal_required"}
            if goal == "__mock_refuse_task__":
                return 503, {"error": "service_unavailable", "message": "Mock task service is unavailable"}
            assist = ((body.get("models") or {}).get("assist") or [{}])[0]
            task = {"id": self.next_id("t"), "goal": goal, "status": "running", "phase": "jev", "flow": None,
                    "steps": [], "confirm": None, "summary": None, "error": None,
                    "started_at_ms": int(time.time() * 1000), "elapsed_ms": 0,
                    "models_used": {"assist": assist.get("model")},
                    "risky": any(word in goal.lower() for word in RISKY), "answer": None, "cancel": False}
            self.tasks.append(task)
        # Keys never reach a log, not even the mock's.
        models = json.loads(json.dumps(body.get("models") or {}))
        for entry in [models.get("jev"), models.get("text"), *(models.get("assist") or [])]:
            if isinstance(entry, dict) and entry.get("key"):
                entry["key"] = "…"
        print(json.dumps({"task": task["id"], "goal": goal, "models": models}, ensure_ascii=False), flush=True)
        threading.Thread(target=self.run, args=(task,), daemon=True).start()
        return 202, {"id": task["id"], "status": "running"}

    def run(self, task):
        targets = ["设置", "电池", "电池健康", "确认", "返回"]
        long_progress = task["goal"] in ("__mock_long_progress__", "__mock_task_conflict_existing__")
        for n in range(1, 101 if long_progress else 6):
            time.sleep(3 if long_progress else 1)
            with self.lock:
                if task["cancel"]:
                    task["status"] = "cancelled"
                    break
                if n == 3 and task["risky"]:
                    task["status"] = "needs_confirm"
                    task["confirm"] = {"action": "CLICK", "target": "确认支付", "reason": "pays ¥55.51",
                                       "app": "拼多多"}
            while task["status"] == "needs_confirm" and task["answer"] is None and not task["cancel"]:
                time.sleep(0.2)
            with self.lock:
                if task["status"] == "needs_confirm":
                    task["confirm"] = None
                    if task["cancel"]:
                        task["status"] = "cancelled"
                        break
                    if task["answer"] is False:
                        task["status"] = "blocked"
                        task["error"] = "你没有允许这一步，任务停在这里。"
                        break
                    task["status"] = "running"
                by = "assist" if n == 4 else "jev"
                target = f"进度测试 {n}" if long_progress else targets[n - 1]
                task["phase"] = "acting" if long_progress else by
                task["planned_action"] = {"action": "WAIT", "target": target} if long_progress else None
                task["steps"].append({"n": n, "action": "WAIT" if long_progress else "CLICK", "target": target, "text": None,
                                      "app": "设置", "ok": True, "error": None, "page_changed": True,
                                      "by": by, "ms": 600 + 50 * n})
        with self.lock:
            if task["status"] == "running":
                task["phase"] = "summary"
        time.sleep(0.8)
        with self.lock:
            if task["status"] == "running":
                task["status"] = "done"
                task["summary"] = "进度显示测试完成，未操作手机。" if long_progress else "电池电量 87%，低电量模式关闭。"
            task["elapsed_ms"] = int(time.time() * 1000) - task["started_at_ms"]

    @staticmethod
    def public(task, since=0, steps=True):
        out = {k: v for k, v in task.items() if k not in ("risky", "answer", "cancel")}
        if task["status"] in ("running", "needs_confirm"):
            out["elapsed_ms"] = int(time.time() * 1000) - task["started_at_ms"]
        if steps:
            out["steps"] = [s for s in task["steps"] if s["n"] > since]
        else:
            out.pop("steps")
        return out

    def find(self, tid):
        if tid == "current":
            return next((t for t in reversed(self.tasks) if t["status"] in ("running", "needs_confirm")), None)
        return next((t for t in self.tasks if t["id"] == tid), None)

    def schedule_view(self, s):
        last = next((r for r in reversed(self.runs) if r["schedule_id"] == s["id"]), None)
        return {**s, "last_run": last}


class Mock:
    def __init__(self, args):
        self.args = args
        self.scenario = args.scenario
        self.want_keyframe = threading.Event()
        self.actions = []
        self.lock = threading.Lock()
        source = Path(args.video) if args.video else None
        key = hashlib.sha1(f"{source}|{args.fps}|{args.width}".encode()).hexdigest()[:10]
        cache = HERE / f".mock-stream-{key}.h264"
        stream = build_stream(source, cache, args.fps, args.width)
        self.size = probe_size(cache)
        self.units = access_units(stream)
        if not self.units:
            sys.exit("mock_daemon: the stream has no access units")
        self.screenshot = (DEMO / "demo-general.jpg").read_bytes()
        self.agent = Agent()

    def status(self):
        s = dict(BASE_STATUS)
        s.update(SCENARIOS.get(self.scenario, {}))
        s["device"] = {"name": self.args.name, "model": self.args.model, "ios": "26.1"}
        return s


def handler(mock):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *a):
            if mock.args.verbose:
                sys.stderr.write("mock: " + fmt % a + "\n")

        def delay(self):
            if mock.args.latency_ms:
                time.sleep(mock.args.latency_ms / 1000)

        def body(self):
            n = int(self.headers.get("Content-Length") or 0)
            return self.rfile.read(n) if n else b""

        def send(self, code, payload=b"", ctype="application/json", extra=None):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(payload)))
            for k, v in (extra or {}).items():
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(payload)

        def authed(self):
            return "phone_session=mock" in (self.headers.get("Cookie") or "")

        def do_POST(self):
            self.delay()
            path = self.path.split("?")[0]
            raw = self.body()
            cookie = {"Set-Cookie": "phone_session=mock; Path=/; HttpOnly"}
            if path == "/login":
                return self.send(303, b"", "text/plain", {**cookie, "Location": "/"})
            if path == "/pair":
                return self.send(200, json.dumps({"device_token": "mock-token", "lan_urls": []}).encode(),
                                 extra=cookie)
            if path == "/mock/scenario":
                mock.scenario = json.loads(raw or b"{}").get("name", "live")
                print(json.dumps({"scenario": mock.scenario}), flush=True)
                return self.send(200, b'{"ok":true}')
            if not self.authed():
                return self.send(401, b'{"ok":false,"error":"unauthorized"}')
            if path == "/agent/h264/keyframe":
                mock.want_keyframe.set()
                return self.send(200, b'{"ok":true}')
            if path.startswith(("/agent/task", "/agent/flow", "/agent/schedules")):
                return self.agent_post(path, raw)
            if path in ("/control", "/agent/mode"):
                try:
                    action = json.loads(raw or b"{}")
                except ValueError:
                    return self.send(400, b'{"ok":false,"error":"invalid_control_message"}')
                action.pop("issued_at_ms", None)
                action.pop("ttl_ms", None)
                entry = {"t": round(time.time(), 3), "path": path, **action}
                with mock.lock:
                    mock.actions.append(entry)
                with mock.agent.lock:
                    if path == "/control" and mock.agent.recording is not None:
                        kind = "tap_locator" if action.get("type") == "tap" else action.get("type")
                        mock.agent.recording.append({"kind": kind})
                print(json.dumps(entry), flush=True)
                if path == "/agent/mode":
                    mock.scenario = "handoff" if action.get("mode") == "human" else "live"
                return self.send(200, b'{"ok":true}')
            self.send(404, b"{}")

        def do_DELETE(self):
            path = self.path.split("?")[0]
            if path == "/mock/actions":
                with mock.lock:
                    mock.actions.clear()
                return self.send(200, b'{"ok":true}')
            if not self.authed():
                return self.send(401, b'{"ok":false,"error":"unauthorized"}')
            if path.startswith("/agent/schedules/"):
                sid = path.split("/")[3]
                with mock.agent.lock:
                    before = len(mock.agent.schedules)
                    mock.agent.schedules = [s for s in mock.agent.schedules if s["id"] != sid]
                    if len(mock.agent.schedules) == before:
                        return self.json(404, NO_SCHEDULE)
                return self.json(200, {"ok": True, "deleted": sid})
            self.send(404, b"{}")

        def do_PATCH(self):
            path = self.path.split("?")[0]
            raw = self.body()
            if not self.authed():
                return self.send(401, b'{"ok":false,"error":"unauthorized"}')
            if path.startswith("/agent/schedules/"):
                sid = path.split("/")[3]
                body = json.loads(raw or b"{}")
                with mock.agent.lock:
                    s = next((s for s in mock.agent.schedules if s["id"] == sid), None)
                    if s is None:
                        return self.json(404, NO_SCHEDULE)
                    s["enabled"] = bool(body.get("enabled"))
                    return self.json(200, {"ok": True, "schedule": mock.agent.schedule_view(s)})
            self.send(404, b"{}")

        def json(self, code, obj):
            return self.send(code, json.dumps(obj, ensure_ascii=False).encode())

        def agent_post(self, path, raw):
            a = mock.agent
            try:
                body = json.loads(raw or b"{}")
            except ValueError:
                return self.json(400, {"ok": False, "error": "invalid_json"})
            if self.headers.get("X-Phone-Control") != "1":
                return self.json(403, {"ok": False, "error": "missing_phone_control_header"})
            parts = path.strip("/").split("/")
            if path == "/agent/task":
                code, out = a.start(body)
                return self.json(code, out)
            if len(parts) == 4 and parts[1] == "task" and parts[3] in ("confirm", "cancel"):
                with a.lock:
                    task = a.find(parts[2])
                    if task is None:
                        return self.json(404, {"ok": False, "error": "task_not_found"})
                    if parts[3] == "cancel":
                        task["cancel"] = True
                    elif task["status"] != "needs_confirm":
                        return self.json(409, {"ok": False, "error": "not_waiting", "message": "nothing to answer"})
                    else:
                        task["answer"] = bool(body.get("approve"))
                print(json.dumps({"task": parts[2], parts[3]: body}), flush=True)
                return self.json(200, {"ok": True})
            if path == "/agent/flow/record":
                with a.lock:
                    if body.get("action") == "start":
                        a.recording = []
                        return self.json(200, {"ok": True, "recording": True})
                    if body.get("action") == "stop":
                        steps, a.recording = a.recording or [], None
                        draft = None
                        if steps:
                            draft = {"source": "current",
                                     "app": {"bundle": "com.apple.Preferences", "label": "设置"},
                                     "recorded_steps": len(steps),
                                     "flow": {"version": 1, "name": "设置 task", "app": "com.apple.Preferences",
                                              "steps": [{"kind": "launch_app"}] + steps},
                                     "todo": ["Give it a real name and description.",
                                              "Add a wait_for gate after each tap that opens a new screen."]}
                        return self.json(200, {"ok": True, "recording": False, "draft": draft})
                return self.json(400, {"ok": False, "error": "invalid_action"})
            if path == "/agent/flow/save":
                fid = body.get("id") or ""
                if "/" not in fid:
                    return self.json(400, {"ok": False, "error": "id_required", "hint": "an id like myapp/daily-check"})
                with a.lock:
                    a.flows = [f for f in a.flows if f["id"] != fid]
                    steps = len((body.get("flow") or {}).get("steps") or []) or 3
                    a.flows.append({"id": fid, "name": body.get("name") or fid,
                                    "description": body.get("description"), "steps": steps, "inputs": [],
                                    "source": "local", "verified": False, "risk": "unknown"})
                print(json.dumps({"saved": fid, "with_flow": "flow" in body}), flush=True)
                return self.json(200, {"ok": True, "saved": {"id": fid, "source": "local"}})
            if path == "/agent/flow/run":
                flow = next((f for f in a.flows if f["id"] == body.get("id")), None)
                if flow is None:
                    return self.json(422, {"ok": False, "error": "flow_not_run", "message": "no flow with that id"})
                print(json.dumps({"run": flow["id"], "confirm": body.get("confirm")}), flush=True)
                if flow.get("risk") == "side_effect" and not body.get("confirm"):
                    return self.json(200, {"ok": False, "result": {"ok": False, "error": "confirmation_required"}})
                time.sleep(3)
                return self.json(200, {"ok": True, "result": {"ok": True, "outputs": {"level": "87%"}}})
            if path == "/agent/schedules":
                cron = body.get("cron") or ""
                if len(cron.split()) != 5:
                    return self.json(400, {"ok": False, "error": "invalid_cron", "message": "cron needs 5 fields"})
                with a.lock:
                    now = int(time.time())
                    s = {"id": a.next_id("s"), "name": body.get("name"), "cron": cron,
                         "kind": body.get("kind", "flow"), "target": body.get("target"),
                         "inputs": body.get("inputs") or {},
                         "confirm_side_effects": bool(body.get("confirm_side_effects")), "enabled": True,
                         "window_mins": 30, "created_at": now, "next_run_at": now + 3600}
                    a.schedules.append(s)
                    print(json.dumps({"schedule": s}, ensure_ascii=False), flush=True)
                    return self.json(201, {"ok": True, "schedule": a.schedule_view(s)})
            if len(parts) == 4 and parts[1] == "schedules" and parts[3] == "run":
                with a.lock:
                    s = next((s for s in a.schedules if s["id"] == parts[2]), None)
                    if s is None:
                        return self.json(404, NO_SCHEDULE)
                    now = int(time.time())
                    run = {"id": a.next_id("r"), "schedule_id": s["id"], "scheduled_for": now,
                           "deadline": now + 1800, "state": "passed", "manual": True, "attempts": 1,
                           "finished_at": now + 3, "duration_ms": 3000, "summary": "level=87%"}
                    a.runs.append(run)
                    return self.json(202, {"ok": True, "run": run})
            return self.json(404, {"ok": False, "error": "not_found"})

        def agent_get(self, path, query):
            a = mock.agent
            parts = path.strip("/").split("/")
            with a.lock:
                if path == "/agent/tasks":
                    return self.json(200, {"ok": True, "tasks": [Agent.public(t, steps=False)
                                                                 for t in reversed(a.tasks[-20:])]})
                if len(parts) == 3 and parts[1] == "task":
                    task = a.find(parts[2])
                    if task is None:
                        return self.json(404, {"ok": False, "error": "task_not_found"})
                    if task["goal"] == "__mock_task_conflict_existing__":
                        time.sleep(2)
                    since = int((query.get("since") or ["0"])[0] or 0)
                    return self.json(200, Agent.public(task, since))
                if path == "/agent/flow/record":
                    return self.json(200, {"ok": True, "recording": a.recording is not None,
                                           "steps": len(a.recording or []), "draft": None})
                if path == "/agent/flow/draft":
                    done = next((t for t in reversed(a.tasks) if t["status"] == "done"), None)
                    if done is None:
                        return self.json(404, {"ok": False, "error": "no_trail"})
                    return self.json(200, {"ok": True, "source": "previous",
                                           "app": {"bundle": "com.apple.Preferences", "label": "设置"},
                                           "recorded_steps": len(done["steps"]),
                                           "flow": {"version": 1, "name": "设置 task", "app": "com.apple.Preferences",
                                                    "steps": [{"kind": "tap_locator"}] * len(done["steps"])},
                                           "todo": ["Give it a real name and description."]})
                if path == "/agent/flows":
                    return self.json(200, {"ok": True, "source": "mock", "flows": a.flows})
                if path == "/agent/schedules":
                    return self.json(200, {"ok": True, "schedules": [a.schedule_view(s) for s in a.schedules],
                                           "mcp_available": True})
                if path == "/agent/schedules/runs":
                    return self.json(200, {"ok": True, "runs": list(reversed(a.runs))})
                if len(parts) == 4 and parts[1] == "schedules" and parts[3] == "runs":
                    return self.json(200, {"ok": True, "runs": [r for r in reversed(a.runs)
                                                                if r["schedule_id"] == parts[2]]})
            return self.json(404, {"ok": False, "error": "not_found"})

        def do_GET(self):
            path = self.path.split("?")[0]
            if path == "/pair/probe":
                return self.send(404, b"{}")
            if path == "/mock/info":
                w, h = mock.size or (0, 0)
                return self.send(200, json.dumps({"width": w, "height": h, "fps": mock.args.fps}).encode())
            if path == "/mock/actions":
                with mock.lock:
                    return self.send(200, json.dumps(mock.actions).encode())
            self.delay()
            if not self.authed():
                return self.send(401, b'{"ok":false,"error":"unauthorized"}')
            if path == "/agent/status":
                return self.send(200, json.dumps(mock.status()).encode())
            if path == "/agent/screenshot":
                return self.send(200, mock.screenshot, "image/jpeg")
            if path == "/agent/h264":
                return self.stream()
            if path.startswith(("/agent/task", "/agent/flow", "/agent/schedules")):
                return self.agent_get(path, parse_qs(self.path.partition("?")[2]))
            self.send(404, b"{}")

        def stream(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.close_connection = True
            units = mock.units
            first_key = next(i for i, (k, _) in enumerate(units) if k)
            i = first_key
            period = 1 / mock.args.fps
            started = time.monotonic()
            sent = 0
            try:
                while True:
                    if mock.scenario in ("released", "starting", "locked", "handoff", "passcode", "restart-locked"):
                        time.sleep(0.2)
                        continue
                    if mock.scenario == "stall":
                        time.sleep(0.1)
                        continue
                    if mock.want_keyframe.is_set():
                        mock.want_keyframe.clear()
                        i = first_key
                    key, au = units[i]
                    self.wfile.write(frame_message(key, int(sent * period * 1e6), au))
                    self.wfile.flush()
                    sent += 1
                    i = (i + 1) % len(units)
                    if i == 0:
                        i = first_key
                    wait = started + sent * period - time.monotonic()
                    if wait > 0:
                        time.sleep(wait)
            except (BrokenPipeError, ConnectionResetError):
                return

    return H


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--port", type=int, default=47001)
    p.add_argument("--bind", default="127.0.0.1")
    p.add_argument("--video", help="video file to replay (default: demo screens slideshow)")
    p.add_argument("--fps", type=int, default=30)
    p.add_argument("--width", type=int, default=590, help="stream width in pixels")
    p.add_argument("--scenario", default="live", choices=sorted(SCENARIOS))
    p.add_argument("--name", default="Mock iPhone")
    p.add_argument("--model", default="iPhone 15 Pro")
    p.add_argument("--latency-ms", type=int, default=0, help="added to every request")
    p.add_argument("--verbose", action="store_true")
    args = p.parse_args()
    mock = Mock(args)
    server = ThreadingHTTPServer((args.bind, args.port), handler(mock))
    server.daemon_threads = True
    sys.stderr.write(f"mock daemon on http://{args.bind}:{args.port} ({len(mock.units)} frames, "
                     f"scenario {args.scenario})\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
