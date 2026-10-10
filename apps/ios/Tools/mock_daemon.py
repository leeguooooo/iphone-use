#!/usr/bin/env python3
"""A stand-in iphone-use daemon for checking the iOS remote in the Simulator.

It speaks just enough of the daemon's HTTP contract for the app to log in,
stream video and send gestures, without a phone, a Mac service or WDA:

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
    "locked": {"wda_locked": True, "drivable": False, "device_state": "locked",
               "setup_blocked_on": "locked"},
    "handoff": {"human_handoff": True, "drivable": False, "mode": "human"},
    "owned": {"owner": "agent-loop", "owner_lease_remaining_secs": 240},
}

BASE_STATUS = {
    "ok": True, "backend": "direct", "drivable": True, "mode": "agent",
    "device_state": "ready", "released": False, "reconnecting": False,
    "releasing": False, "warming": False, "human_handoff": False,
    "wda_locked": False, "hint": "", "next_step": {}, "setup_blocked_on": "",
    "recovery_owner": "", "version": "0.17.12-mock", "capture_redacted": False,
    "owner": None, "owner_lease_remaining_secs": 0, "transport": "usb",
    "wda_rtt_ms": 18,
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
                print(json.dumps(entry), flush=True)
                if path == "/agent/mode":
                    mock.scenario = "handoff" if action.get("mode") == "human" else "live"
                return self.send(200, b'{"ok":true}')
            self.send(404, b"{}")

        def do_DELETE(self):
            if self.path.split("?")[0] == "/mock/actions":
                with mock.lock:
                    mock.actions.clear()
                return self.send(200, b'{"ok":true}')
            self.send(404, b"{}")

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
                    if mock.scenario in ("released", "starting", "locked", "handoff"):
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
