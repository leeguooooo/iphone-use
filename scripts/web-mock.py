#!/usr/bin/env python3
"""Mock daemon for working on the web control panel without a phone.

Serves the real web/ files (re-read on every request, so edits show on reload)
behind a fake daemon: /agent/status from a switchable scenario, a replayed
H.264 (/agent/h264) and MJPEG (/agent/mjpeg) stream, /agent/screenshot, and a
/control endpoint that records every gesture instead of touching a device.

    python3 scripts/web-mock.py [--port 8799] [--frames DIR | --video FILE]
                                [--owner NAME] [--rtt 40]

Needs ffmpeg (Homebrew) to build the replayed stream. Source picture, first
match wins:
  --frames DIR   PNG/JPEG phone screenshots (e.g. 1170x2532), shown as a
                 slideshow with cross-fades;
  --video FILE   any phone screen recording;
  (neither)      ffmpeg's animated test pattern at a phone aspect ratio.

Open http://127.0.0.1:8799/phone (add ?visible=1 when an automation tool
drives the tab in the background: it pins document.visibilityState to
"visible" so the live view renders; set window.__mockHidden = true and
dispatch "visibilitychange" to simulate leaving the tab). There is no password: /login just sets the
cookie. Switch what the page sees with

    curl -s 'http://127.0.0.1:8799/__mock/scenario?name=locked'

Scenarios: ready (default), connecting (status ready, stream withheld),
stall (stream freezes after a few seconds), locked, released, handoff,
reconnecting, usb (setup blocked: cable), passcode (setup blocked: a
person must enter the passcode on the phone), restart-locked (a known phone
off usbmuxd since a restart), offline, unconfigured, owned
(another session holds the owner lease; /control answers 409), will-lock
(ready, lock_readiness says a person must unlock), redacted.

Inspect what the page sent with GET /__mock/controls (JSON list, newest last);
DELETE /__mock/controls clears it. Every control is also printed to stdout.
"""

import argparse
import http.server
import json
import os
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB = ROOT / "web"
HTTP_RS = ROOT / "crates" / "server" / "src" / "http.rs"

LOCK = threading.Lock()
STATE = {
    "scenario": "ready",
    "stream_started": 0.0,
    "stream_id": None,
    "stream_last_frame": 0.0,
    "controls": [],
    "owner": "codex-agent",
    "rtt": 40,
}
MEDIA = {"jpegs": [], "aus": [], "fps": 30}


# ---------------------------------------------------------------- media build
def build_media(args, workdir: Path):
    if not shutil.which("ffmpeg"):
        sys.exit("web-mock: ffmpeg not found (brew install ffmpeg)")
    w, h = 586, 1268  # even numbers near a 1170x2532 phone at half size
    src = workdir / "src.mp4"
    if args.frames:
        pics = sorted(
            p for p in Path(args.frames).iterdir()
            if p.suffix.lower() in (".png", ".jpg", ".jpeg")
        )
        if not pics:
            sys.exit(f"web-mock: no PNG/JPEG files in {args.frames}")
        inputs, filters, hold, fade = [], [], 3.0, 0.5
        for i, pic in enumerate(pics):
            inputs += ["-loop", "1", "-t", str(hold + fade), "-i", str(pic)]
            filters.append(
                f"[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,"
                f"pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30,format=yuv420p[v{i}]"
            )
        chain, last = "", "v0"
        for i in range(1, len(pics)):
            offset = i * hold
            chain += f";[{last}][v{i}]xfade=transition=slideleft:duration={fade}:offset={offset}[x{i}]"
            last = f"x{i}"
        graph = ";".join(filters) + chain
        cmd = ["ffmpeg", "-v", "error", "-y", *inputs, "-filter_complex", graph,
               "-map", f"[{last}]", "-t", str(len(pics) * hold), str(src)]
    elif args.video:
        cmd = ["ffmpeg", "-v", "error", "-y", "-i", args.video, "-t", "40",
               "-vf", f"scale={w}:{h}:force_original_aspect_ratio=decrease,"
                      f"pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps=30", "-an", str(src)]
    else:
        cmd = ["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i",
               f"testsrc2=size={w}x{h}:rate=30", "-t", "20", str(src)]
    subprocess.run(cmd, check=True)

    # H.264 baseline, Annex-B, SPS/PPS on every keyframe, an AUD per frame so
    # access units split cleanly.
    annexb = workdir / "stream.h264"
    subprocess.run([
        "ffmpeg", "-v", "error", "-y", "-i", str(src), "-an",
        "-c:v", "libx264", "-profile:v", "baseline", "-pix_fmt", "yuv420p",
        "-tune", "zerolatency", "-g", "60", "-bf", "0",
        "-x264-params", "aud=1:repeat-headers=1", "-b:v", "1500k",
        "-f", "h264", str(annexb),
    ], check=True)
    MEDIA["aus"] = split_access_units(annexb.read_bytes())

    jpeg_dir = workdir / "jpeg"
    jpeg_dir.mkdir()
    subprocess.run([
        "ffmpeg", "-v", "error", "-y", "-i", str(src), "-vf", "fps=15",
        "-q:v", "5", str(jpeg_dir / "f%05d.jpg"),
    ], check=True)
    MEDIA["jpegs"] = [p.read_bytes() for p in sorted(jpeg_dir.iterdir())]
    print(f"web-mock: {len(MEDIA['aus'])} H.264 access units, "
          f"{len(MEDIA['jpegs'])} JPEG frames", flush=True)


def split_access_units(data: bytes):
    """Split an Annex-B stream at access-unit delimiters (NAL type 9)."""
    starts = [m.start() for m in re.finditer(b"\x00\x00\x00\x01\x09", data)]
    if not starts:
        sys.exit("web-mock: encoder produced no access-unit delimiters")
    units = []
    for i, start in enumerate(starts):
        end = starts[i + 1] if i + 1 < len(starts) else len(data)
        au = data[start:end]
        nal_types = {au[m.end()] & 0x1F for m in re.finditer(b"\x00\x00\x01", au)
                     if m.end() < len(au)}
        units.append((5 in nal_types, au))
    return units


# ---------------------------------------------------------------- status
def device_json():
    return {"name": "Leo 的 iPhone 13", "model": "iPhone 13", "product_type": "iPhone14,5", "ios": "26.1"}


def status_json():
    sc = STATE["scenario"]
    now = time.monotonic()
    fresh_age = int((now - STATE["stream_last_frame"]) * 1000) if STATE["stream_last_frame"] else None
    s = {
        "ok": True, "backend": "direct", "instance": "default", "udid": "00008110-MOCK",
        "owner": None, "owner_lease_remaining_secs": None,
        "target_configured": True, "managed_wda": True, "managed_wda_pending": False,
        "recovery_owner": "daemon", "wda": True, "wda_actionable": True, "wda_locked": False,
        "drivable": True, "mode": "agent", "device_state": "ready", "screen_state": "unlocked",
        "releasing": False, "reconnecting": False, "warming": False, "released": False,
        "human_handoff": False, "hold_remaining_secs": None, "idle_secs": 3,
        "hint": None, "next_step": None, "setup_blocked_on": "", "setup_phase": None,
        "setup_message": None, "wda_build": None, "wda_died_reason": "", "wda_died_at": None,
        "viewer_count": 1, "mjpeg_viewer_count": 1,
        "mjpeg_stream_fresh": fresh_age is not None and fresh_age < 8000,
        "mjpeg_stream_age_ms": fresh_age,
        "capture_redacted": sc == "redacted", "version": "0.17.10", "latest": None,
        "update_available": False, "transport": "usb", "wda_rtt_ms": STATE["rtt"],
        "transport_hint": None, "wifi_start_refused": False, "keep_runner_alive": None, "legacy_ios": None,
        "lock_readiness": {
            "passcode_protected": True, "auto_lock_secs": 0,
            "keep_awake": {"enabled": True, "supported": True, "active": True},
            "verdict": "ready", "hint": {"zh": "", "en": ""}, "checked_at": None,
        },
        "device": device_json(),
    }
    if sc == "will-lock":
        s["lock_readiness"].update({
            "auto_lock_secs": 30, "verdict": "will_lock_needs_person",
            "hint": {"zh": "这台 iPhone 有锁屏密码，30 秒没人操作就会锁屏；锁屏后需要有人解锁。把 设置 › 显示与亮度 › 自动锁定 改成「永不」可以避免。",
                     "en": "This iPhone has a passcode and auto-locks after 30 seconds; a person must unlock it."},
        })
    elif sc == "locked":
        s.update(drivable=False, wda_actionable=False, wda_locked=True, screen_state="locked",
                 device_state="locked", next_step={"zh": "请解锁 iPhone 并保持亮屏，连接会自动恢复", "en": "Unlock the iPhone."})
    elif sc == "released":
        s.update(released=True, drivable=False, device_state="released", idle_secs=900)
    elif sc == "handoff":
        s.update(released=True, human_handoff=True, drivable=False, device_state="released")
    elif sc == "reconnecting":
        s.update(reconnecting=True, drivable=False, device_state="reconnecting")
    elif sc == "usb":
        s.update(reconnecting=True, drivable=False, wda=False, setup_blocked_on="usb")
    elif sc == "passcode":
        s.update(reconnecting=True, drivable=False, wda=False, wda_actionable=False,
                 device_state="blocked", setup_blocked_on="needs_passcode_on_phone",
                 next_step={"zh": "手机设了锁屏密码：这次启动需要有人在手机上输入密码，允许 UI 自动化。之后保持连接就不会再问。",
                            "en": "This iPhone has a passcode: this start needs someone at the phone to enter it."})
    elif sc == "restart-locked":
        s.update(reconnecting=True, drivable=False, wda=False, wda_actionable=False,
                 device_state="blocked", setup_blocked_on="locked_after_restart",
                 next_step={"zh": "这台 iPhone 重启后还没解锁过：需要有人在手机上输入一次密码，之后会自动连上。",
                            "en": "This iPhone restarted and has not been unlocked since."})
    elif sc == "offline":
        s.update(drivable=False, wda=False, wda_actionable=False, mode="agent", device_state="offline",
                 next_step={"zh": "设备服务没有响应；正在自动重启，若一直这样请在 Mac 上运行 iphone-use doctor", "en": "Device service unreachable."})
    elif sc == "unconfigured":
        s.update(drivable=False, wda=False, recovery_owner="unconfigured", target_configured=False, device=None)
    elif sc == "owned":
        s.update(owner=STATE["owner"], owner_lease_remaining_secs=212)
    return s


def elements_json():
    labels = [("Button", "设置"), ("Button", "照片"), ("Button", "相机"), ("SearchField", "搜索"),
              ("Button", "信息"), ("Button", "Safari 浏览器"), ("Button", "音乐"), ("Cell", "通用")]
    return {
        "ok": True, "snapshot": "mock-snap-1",
        "elements": [{"kind": k, "label": l, "rect": [20 + i * 10, 80 + i * 60, 80, 44]}
                     for i, (k, l) in enumerate(labels)],
    }


QR_SVG = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 21 21" shape-rendering="crispEdges">'
          + "".join(f'<rect x="{x}" y="{y}" width="1" height="1"/>'
                    for y in range(21) for x in range(21)
                    if (x * 7 + y * 3 + x * y) % 5 < 2 or (x < 7 and y < 7 and (x in (0, 6) or y in (0, 6) or (1 < x < 5 and 1 < y < 5))))
          + "</svg>")


# window.__mockHidden = true + a dispatched "visibilitychange" simulates the
# tab going to the background (and back) for testing the grace period.
FORCE_VISIBLE = (b"Object.defineProperty(Document.prototype,'visibilityState',"
                 b"{get(){return window.__mockHidden?'hidden':'visible'}});"
                 b"Object.defineProperty(Document.prototype,'hidden',{get(){return !!window.__mockHidden}});")


def login_html():
    try:
        src = HTTP_RS.read_text()
        m = re.search(r'const LOGIN_HTML: &str = r#"(.*?)"#;', src, re.S)
        if m:
            return (m.group(1).replace("__NEXT_INPUT__", "").replace("__ERR__", "")
                    .replace("__INVALID__", "false"))
    except OSError:
        pass
    return "<form method=POST action=/login><button>login</button></form>"


# ---------------------------------------------------------------- handler
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet; controls are printed instead
        pass

    def send(self, code, body, ctype="application/json", headers=None):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False)
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def redirect(self, to, cookie=None):
        self.send_response(303)
        self.send_header("Location", to)
        self.send_header("Content-Length", "0")
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        q = dict(urllib.parse.parse_qsl(url.query))
        p = url.path
        if p == "/":
            return self.redirect("/phone")
        if p in ("/phone", "/setup", "/schedules"):
            name = {"/phone": "index.html", "/setup": "setup.html", "/schedules": "schedules.html"}[p]
            page = (WEB / name).read_bytes()
            if q.get("visible") == "1":
                # Automation drives tabs in the background, where the page sees
                # visibilityState "hidden"; ?visible=1 pins it to "visible" so a
                # background tab renders the live view for screenshots.
                page = page.replace(b"<head>", b"<head><script>" + FORCE_VISIBLE + b"</script>", 1)
            return self.send(200, page, "text/html; charset=utf-8")
        if p == "/login":
            return self.send(200, login_html(), "text/html; charset=utf-8")
        if p == "/logout":
            return self.redirect("/login", "phone_session=; Max-Age=0; Path=/")
        if p == "/agent/status":
            return self.send(200, status_json())
        if p == "/agent/screenshot":
            frames = MEDIA["jpegs"]
            idx = int((time.monotonic() * 15)) % len(frames)
            return self.send(200, frames[idx], "image/jpeg",
                             {"X-Capture-Redacted": "1"} if STATE["scenario"] == "redacted" else None)
        if p == "/agent/mjpeg":
            return self.stream_mjpeg(q)
        if p == "/agent/h264":
            if os.environ.get("WEB_MOCK_NO_H264"):
                return self.send(501, {"ok": False, "error": "h264_unavailable"})
            return self.stream_h264(q)
        if p == "/agent/elements":
            time.sleep(0.4)
            return self.send(200, elements_json())
        if p == "/__mock/scenario":
            if "name" in q:
                with LOCK:
                    STATE["scenario"] = q["name"]
                print(f"web-mock: scenario -> {q['name']}", flush=True)
            return self.send(200, {"scenario": STATE["scenario"]})
        if p == "/__mock/controls":
            with LOCK:
                return self.send(200, STATE["controls"])
        return self.send(404, {"ok": False, "error": "not_found"})

    def do_DELETE(self):
        if self.path.startswith("/__mock/controls"):
            with LOCK:
                STATE["controls"].clear()
            return self.send(200, {"ok": True})
        return self.send(404, {"ok": False})

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        p = url.path
        raw = self.body()
        if p == "/login":
            return self.redirect("/phone", "phone_session=mock; Path=/; HttpOnly; SameSite=Lax")
        if p == "/control" or p == "/agent/actions":
            try:
                action = json.loads(raw or b"{}")
            except ValueError:
                return self.send(400, {"ok": False, "error": "invalid_control_message"})
            if self.headers.get("X-Phone-Control") != "1":
                return self.send(403, {"ok": False, "error": "missing_control_header"})
            with LOCK:
                STATE["controls"].append({"at": round(time.time(), 3), "path": p, "action": action})
            print(f"web-mock: {p} {json.dumps(action, ensure_ascii=False)}", flush=True)
            if STATE["scenario"] == "owned":
                return self.send(409, {"ok": False, "error": "phone_owned", "owner": STATE["owner"],
                                       "owner_lease_remaining_secs": 212, "outcome": "not_sent"})
            time.sleep(STATE["rtt"] / 1000.0)
            return self.send(200, {"ok": True, "outcome": "acknowledged"})
        if p == "/agent/mode":
            try:
                mode = json.loads(raw or b"{}").get("mode")
            except ValueError:
                mode = None
            with LOCK:
                if mode == "human":
                    STATE["scenario"] = "handoff"
                elif mode == "agent":
                    STATE["scenario"] = "ready"
            return self.send(200, {"ok": True})
        if p == "/agent/h264/keyframe":
            return self.send(200, {"ok": True})
        if p == "/pair/new":
            return self.send(200, {"ok": True, "svg": QR_SVG, "url": "http://192.168.1.20:8787/pair?c=MOCK",
                                   "host": "192.168.1.20", "hosts": ["192.168.1.20", "leo-mac.local"],
                                   "expires_in_secs": 300})
        return self.send(404, {"ok": False, "error": "not_found"})

    # -- streams
    def stream_allowed(self):
        return STATE["scenario"] not in ("connecting", "locked", "released", "handoff",
                                         "reconnecting", "usb", "passcode", "restart-locked", "offline",
                                         "unconfigured")

    def note_frame(self, sid):
        with LOCK:
            STATE["stream_id"] = sid
            STATE["stream_last_frame"] = time.monotonic()

    def stalled(self, started):
        return STATE["scenario"] == "stall" and time.monotonic() - started > 4

    def stream_mjpeg(self, q):
        if not self.stream_allowed():
            return self.send(503, {"ok": False, "error": "unavailable"})
        boundary = "mockframe"
        self.send_response(200)
        self.send_header("Content-Type", f"multipart/x-mixed-replace; boundary={boundary}")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        started, i = time.monotonic(), 0
        try:
            while self.stream_allowed():
                if not self.stalled(started):
                    frame = MEDIA["jpegs"][i % len(MEDIA["jpegs"])]
                    self.wfile.write(f"--{boundary}\r\nContent-Type: image/jpeg\r\nContent-Length: {len(frame)}\r\n\r\n".encode())
                    self.wfile.write(frame + b"\r\n")
                    self.wfile.flush()
                    self.note_frame(q.get("stream_id"))
                    i += 1
                time.sleep(1 / 15)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True

    def stream_h264(self, q):
        if not self.stream_allowed():
            return self.send(503, {"ok": False, "error": "unavailable"})
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()
        aus = MEDIA["aus"]
        start = next(i for i, (key, _) in enumerate(aus) if key)
        started = time.monotonic()
        i, pts = start, 0
        try:
            while self.stream_allowed():
                if self.stalled(started):
                    time.sleep(0.1)
                    continue
                key, au = aus[i % len(aus)]
                msg = bytes([1 if key else 0]) + pts.to_bytes(8, "big") + au
                self.wfile.write(len(msg).to_bytes(4, "big") + msg)
                self.wfile.flush()
                self.note_frame(q.get("stream_id"))
                i += 1
                if i >= len(aus):
                    i = start
                pts += 33_333
                time.sleep(1 / 30)
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8799)
    ap.add_argument("--frames")
    ap.add_argument("--video")
    ap.add_argument("--owner", default="codex-agent")
    ap.add_argument("--rtt", type=int, default=40, help="simulated /control latency, ms")
    ap.add_argument("--scenario", default="ready")
    args = ap.parse_args()
    STATE.update(owner=args.owner, rtt=args.rtt, scenario=args.scenario)
    with tempfile.TemporaryDirectory(prefix="web-mock-") as tmp:
        build_media(args, Path(tmp))
        srv = Server(("127.0.0.1", args.port), Handler)
        print(f"web-mock: http://127.0.0.1:{args.port}/phone", flush=True)
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
