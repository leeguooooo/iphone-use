#!/usr/bin/env python3
"""Fixture tests for scripts/auto-update.sh: the idle gate decides, the installer is a stub."""
import http.server, json, os, subprocess, sys, tempfile, threading
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "auto-update.sh"

class StatusServer:
    def __init__(self, payload):
        self.payload = payload
        handler = self._handler()
        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
    def _handler(s):
        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps(s.payload).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
            def log_message(self, *a): pass
        return H
    @property
    def url(self): return f"http://127.0.0.1:{self.httpd.server_port}/agent/status"
    def close(self): self.httpd.shutdown()

def run(status, latest, *args, marker=None, home=None, extra_env=None):
    env = dict(os.environ, HOME=home, PHONE_REMOTE_STATE_DIR=str(Path(home) / "state"),
               AUTO_UPDATE_STATUS_URL=status.url, AUTO_UPDATE_TOKEN="t", AUTO_UPDATE_LATEST_TAG=latest,
               AUTO_UPDATE_INSTALLER_CMD=f"touch '{marker}'",
               AUTO_UPDATE_SELF_URL=f"file://{Path(home) / 'nonexistent.sh'}")
    env.update(extra_env or {})
    p = subprocess.run(["bash", str(SCRIPT), "run", *args], env=env, capture_output=True, text=True, timeout=30)
    return p.returncode, p.stderr

base = {"ok": True, "version": "0.5.4", "owner": None, "owner_lease_remaining_secs": 0,
        "hold_remaining_secs": 0, "releasing": False, "reconnecting": False, "device_state": "released",
        "viewer_count": 0, "mjpeg_viewer_count": 0, "idle_secs": 7200}
failures = 0
total = 0
def case(name, status_payload, latest, *args, expect_install, expect_decision, extra_env=None):
    global failures, total
    total += 1
    with tempfile.TemporaryDirectory() as home:
        marker = Path(home) / "installed"
        srv = StatusServer(status_payload)
        try:
            rc, err = run(srv, latest, *args, marker=marker, home=home, extra_env=extra_env)
        finally:
            srv.close()
        ok = (marker.exists() == expect_install) and (expect_decision in err) and rc == 0
        print(("ok  " if ok else "FAIL"), name, "" if ok else f"rc={rc} installed={marker.exists()} err={err.strip()[-160:]}")
        failures += not ok

case("up to date → skip", base, "v0.5.4", expect_install=False, expect_decision="skip:up_to_date")
case("newer + idle → upgrade", base, "v0.6.0", expect_install=True, expect_decision="upgrade current=0.5.4 latest=0.6.0")
case("newer + owner → skip", {**base, "owner": "bank-flow", "owner_lease_remaining_secs": 120}, "v0.6.0", expect_install=False, expect_decision="skip:phone_owned owner=bank-flow")
case("newer + hold → skip", {**base, "hold_remaining_secs": 300}, "v0.6.0", expect_install=False, expect_decision="skip:held")
# A connected, drivable phone is not "in use": since v0.6.3 WDA stays up, and
# this rule kept a connected phone from ever auto-upgrading.
case("newer + ready (WDA up), idle → upgrade", {**base, "device_state": "ready"}, "v0.6.0", expect_install=True, expect_decision="upgrade current=0.5.4 latest=0.6.0")
case("newer + agent request 2 min ago → skip", {**base, "device_state": "ready", "idle_secs": 120}, "v0.6.0", expect_install=False, expect_decision="skip:in_use idle_secs=120 (<900)")
case("idle window is configurable", {**base, "device_state": "ready", "idle_secs": 120}, "v0.6.0", expect_install=True, expect_decision="upgrade", extra_env={"AUTO_UPDATE_IDLE_SECS": "60"})
case("newer + live viewer → skip", {**base, "device_state": "ready", "mjpeg_viewer_count": 1}, "v0.6.0", expect_install=False, expect_decision="skip:watched viewers=1")
old_daemon = {k: v for k, v in base.items() if k != "idle_secs"}
case("older daemon without idle_secs, ready, no owner → upgrade", {**old_daemon, "device_state": "ready"}, "v0.6.0", expect_install=True, expect_decision="upgrade")
case("older daemon, named agent's lease still live → skip", {**old_daemon, "device_state": "ready", "owner": "mcp-1", "owner_lease_remaining_secs": 200}, "v0.6.0", expect_install=False, expect_decision="skip:phone_owned owner=mcp-1")
case("a non-integer idle override falls back to 900", {**base, "device_state": "ready", "idle_secs": 120}, "v0.6.0", expect_install=False, expect_decision="skip:in_use idle_secs=120 (<900)", extra_env={"AUTO_UPDATE_IDLE_SECS": "15m"})
case("--force ignores recent activity", {**base, "device_state": "ready", "idle_secs": 5}, "v0.6.0", "--force", expect_install=True, expect_decision="upgrade")
case("newer + reconnecting → skip", {**base, "reconnecting": True}, "v0.6.0", expect_install=False, expect_decision="skip:transitioning")
case("newer + blocked (crash loop) → upgrade", {**base, "device_state": "blocked"}, "v0.6.0", expect_install=True, expect_decision="upgrade")
case("--dry-run never installs", base, "v0.6.0", "--dry-run", expect_install=False, expect_decision="dry-run: would run")
case("--force ignores the owner", {**base, "owner": "bank-flow"}, "v0.6.0", "--force", expect_install=True, expect_decision="upgrade")
case("--force still needs a newer release", base, "v0.5.4", "--force", expect_install=False, expect_decision="skip:up_to_date")
case("--reinstall upgrades even when current", base, "v0.5.4", "--reinstall", expect_install=True, expect_decision="upgrade")
case("older 'latest' (rollback on GitHub) → skip", base, "v0.5.3", expect_install=False, expect_decision="skip:up_to_date")
case("daemon not ok → skip", {**base, "ok": False}, "v0.6.0", expect_install=False, expect_decision="skip:daemon_not_ok")

# After an upgrade the installed copy of this script refreshes itself from the
# new tag (the installer does not ship it), so a fixed idle gate reaches the
# machine it runs on.
def self_refresh_case(name, new_body, expect_body_is_new):
    global failures, total
    total += 1
    with tempfile.TemporaryDirectory() as home:
        state = Path(home) / "state"; state.mkdir()
        installed = state / "auto-update.sh"
        installed.write_text("#!/bin/bash\n# old copy\n")
        new = Path(home) / "new.sh"; new.write_text(new_body)
        marker = Path(home) / "installed"
        srv = StatusServer(base)
        try:
            rc, err = run(srv, "v0.6.0", marker=marker, home=home,
                          extra_env={"AUTO_UPDATE_SELF_URL": f"file://{new}"})
        finally:
            srv.close()
        body = installed.read_text()
        ok = rc == 0 and marker.exists() and ((body == new_body) == expect_body_is_new) \
            and (body == new_body or body == "#!/bin/bash\n# old copy\n")
        print(("ok  " if ok else "FAIL"), name, "" if ok else f"rc={rc} body={body!r} err={err.strip()[-200:]}")
        failures += not ok

self_refresh_case("upgrade refreshes the installed script", "#!/bin/bash\necho new\n", True)
self_refresh_case("a broken download keeps the current copy", "#!/bin/bash\nif then fi (\n", False)
self_refresh_case("a non-script download keeps the current copy", "<html>404</html>\n", False)
print("FAILED" if failures else "OK", f"({total - failures}/{total})")
sys.exit(1 if failures else 0)
