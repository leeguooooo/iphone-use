#!/usr/bin/env python3
"""Fixture tests for scripts/auto-update.sh: the idle gate decides, the installer is a stub."""
import http.server, json, os, plistlib, subprocess, sys, tempfile, threading
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "auto-update.sh"

class StatusServer:
    """Serves `payload`; once `marker` exists (the stub installer ran for this
    instance) it reports `after_version`, like a restarted daemon would. With
    `bearer` set, any other Authorization is refused."""
    def __init__(self, payload, marker=None, after_version=None, bearer=None):
        self.payload, self.marker, self.after_version, self.bearer = payload, marker, after_version, bearer
        handler = self._handler()
        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
    def _handler(s):
        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if s.bearer is not None and self.headers.get("Authorization") != f"Bearer {s.bearer}":
                    self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers(); return
                payload = dict(s.payload)
                if s.marker is not None and s.after_version and Path(s.marker).exists():
                    payload["version"] = s.after_version
                body = json.dumps(payload).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
            def log_message(self, *a): pass
        return H
    @property
    def port(self): return self.httpd.server_port
    @property
    def url(self): return f"http://127.0.0.1:{self.port}/agent/status"
    def close(self): self.httpd.shutdown()

def write_plist(home, label, port, credential=("IPHONE_USE_AGENT_TOKEN", "t")):
    agents = Path(home) / "Library" / "LaunchAgents"
    agents.mkdir(parents=True, exist_ok=True)
    with open(agents / f"{label}.plist", "wb") as f:
        plistlib.dump({"Label": label, "EnvironmentVariables": {
            "IPHONE_USE_PORT": str(port), credential[0]: credential[1], "IPHONE_USE_HOST": "127.0.0.1"}}, f)

def run(latest, *args, home, installed_log, extra_env=None, status_url=None):
    env = dict(os.environ, HOME=home, IPHONE_USE_STATE_DIR=str(Path(home) / "state"),
               AUTO_UPDATE_LATEST_TAG=latest, AUTO_UPDATE_SETTLE_SECS="0",
               AUTO_UPDATE_INSTALLER_CMD=f"echo \"$AUTO_UPDATE_INSTANCE $IPHONE_USE_AUTO_UPDATE_JOB\" >> '{installed_log}'"
                                         f" && touch '{installed_log}.'\"$AUTO_UPDATE_INSTANCE\"",
               AUTO_UPDATE_SELF_URL=f"file://{Path(home) / 'nonexistent.sh'}",
               AUTO_UPDATE_PREBUILD_CMD=f"echo \"$AUTO_UPDATE_INSTANCE\" >> '{installed_log}.prebuilt'")
    if status_url:
        env.update(AUTO_UPDATE_STATUS_URL=status_url, AUTO_UPDATE_TOKEN="t")
    env.update(extra_env or {})
    p = subprocess.run(["bash", str(SCRIPT), "run", *args], env=env, capture_output=True, text=True, timeout=60)
    return p.returncode, p.stderr

def installed_instances(log):
    return [line.split()[0] for line in Path(log).read_text().splitlines()] if Path(log).exists() else []

def outcome(home):
    p = Path(home) / "state" / "auto-update-last.json"
    return json.loads(p.read_text()) if p.exists() else None

base = {"ok": True, "version": "0.5.4", "owner": None, "owner_lease_remaining_secs": 0,
        "hold_remaining_secs": 0, "releasing": False, "reconnecting": False, "device_state": "released",
        "viewer_count": 0, "mjpeg_viewer_count": 0, "idle_secs": 7200}
failures = 0
total = 0
def report(name, ok, detail):
    global failures, total
    total += 1
    print(("ok  " if ok else "FAIL"), name, "" if ok else detail)
    failures += not ok

def case(name, status_payload, latest, *args, expect_install, expect_decision, extra_env=None, expect_rc=0):
    with tempfile.TemporaryDirectory() as home:
        log = Path(home) / "installed"
        srv = StatusServer(status_payload, marker=f"{log}.default", after_version=latest.lstrip("v"))
        write_plist(home, "com.leeguoo.iphone-use", srv.port)
        try:
            rc, err = run(latest, *args, home=home, installed_log=log, extra_env=extra_env, status_url=srv.url)
        finally:
            srv.close()
        installed = bool(installed_instances(log))
        ok = (installed == expect_install) and (expect_decision in err) and rc == expect_rc
        report(name, ok, f"rc={rc} installed={installed} err={err.strip()[-200:]}")

case("up to date → skip", base, "v0.5.4", expect_install=False, expect_decision="skip:up_to_date")
case("newer + idle → upgrade", base, "v0.6.0", expect_install=True, expect_decision="upgrade current=0.5.4 latest=0.6.0")
case("newer + owner → skip", {**base, "owner": "bank-flow", "owner_lease_remaining_secs": 120}, "v0.6.0", expect_install=False, expect_decision="skip:phone_owned owner=bank-flow")
case("newer + hold → skip", {**base, "hold_remaining_secs": 300}, "v0.6.0", expect_install=False, expect_decision="skip:held")
case("newer + handed to a person → skip", {**base, "human_handoff": True}, "v0.6.0", expect_install=False, expect_decision="skip:handed_to_person")
# A connected, drivable phone is not "in use": since v0.6.3 runner stays up, and
# this rule kept a connected phone from ever auto-upgrading.
case("newer + ready (runner up), idle → upgrade", {**base, "device_state": "ready"}, "v0.6.0", expect_install=True, expect_decision="upgrade current=0.5.4 latest=0.6.0")
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

# Every daemon on the Mac: the default and each named instance, each gated on
# its own phone. Runner supervisors and reserved labels are not daemons.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    servers = {
        "default": StatusServer(base, marker=f"{log}.default", after_version="0.6.0"),
        "ix": StatusServer(base, marker=f"{log}.ix", after_version="0.6.0"),
        "busy": StatusServer({**base, "owner": "agent"}, marker=f"{log}.busy", after_version="0.6.0"),
    }
    write_plist(home, "com.leeguoo.iphone-use", servers["default"].port)
    write_plist(home, "com.leeguoo.iphone-use.ix", servers["ix"].port)
    write_plist(home, "com.leeguoo.iphone-use.busy", servers["busy"].port)
    write_plist(home, "com.leeguoo.iphone-use.runner", 1)
    write_plist(home, "com.leeguoo.iphone-use.runner.ix", 1)
    write_plist(home, "com.leeguoo.iphone-use.autoupdate", 1)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        for s in servers.values(): s.close()
    got = sorted(installed_instances(log))
    out = outcome(home) or {}
    report("upgrades each idle instance, skips a busy one and non-daemon labels",
           rc == 0 and got == ["default", "ix"] and "decision[busy]: skip:phone_owned" in err
           and out.get("ok") is True and sorted(r["instance"] for r in out.get("instances", [])) == ["default", "ix"],
           f"rc={rc} installed={got} outcome={out} err={err.strip()[-300:]}")

# install.sh must not reload the job it runs under (that would kill the
# upgrade): the job says so in IPHONE_USE_AUTO_UPDATE_JOB. This stub installer
# refuses to run without it, and pins the release it was asked for.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0")
    write_plist(home, "com.leeguoo.iphone-use", srv.port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log, extra_env={
            "AUTO_UPDATE_INSTALLER_CMD":
                f"[ \"$IPHONE_USE_AUTO_UPDATE_JOB\" = 1 ] && touch '{log}.default'"})
    finally:
        srv.close()
    row = ((outcome(home) or {}).get("instances") or [{}])[0]
    report("the installer is told it runs under the job", rc == 0 and row.get("ok") is True,
           f"rc={rc} row={row} err={err.strip()[-200:]}")

with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    servers = [StatusServer(base, marker=f"{log}.{n}", after_version="0.6.0") for n in ("default", "ix")]
    write_plist(home, "com.leeguoo.iphone-use", servers[0].port)
    write_plist(home, "com.leeguoo.iphone-use.ix", servers[1].port)
    try:
        rc, err = run("v0.6.0", "--instance", "ix", home=home, installed_log=log)
    finally:
        for s in servers: s.close()
    report("--instance upgrades only that instance", rc == 0 and installed_instances(log) == ["ix"],
           f"rc={rc} installed={installed_instances(log)} err={err.strip()[-200:]}")

# A default install has no agent token: the daemon takes its password instead.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0", bearer="pw")
    write_plist(home, "com.leeguoo.iphone-use", srv.port, credential=("IPHONE_USE_PASSWORD", "pw"))
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        srv.close()
    report("a daemon with only a password is reached with it", rc == 0 and installed_instances(log) == ["default"],
           f"rc={rc} err={err.strip()[-200:]}")

# An earlier failure stays reported until that instance is upgraded or found current.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    state = Path(home) / "state"; state.mkdir()
    (state / "auto-update-last.json").write_text(json.dumps({"at": 1, "latest": "0.6.0", "ok": False, "instances": [
        {"instance": "ix", "from": "0.5.4", "to": "0.5.4", "ok": False, "error": "still 0.5.4 after the install"}]}))
    servers = {"default": StatusServer(base, marker=f"{log}.default", after_version="0.6.0"),
               "ix": StatusServer({**base, "owner": "agent"})}
    write_plist(home, "com.leeguoo.iphone-use", servers["default"].port)
    write_plist(home, "com.leeguoo.iphone-use.ix", servers["ix"].port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        for srv in servers.values(): srv.close()
    out = outcome(home) or {}
    rows = {r["instance"]: r["ok"] for r in out.get("instances", [])}
    report("a run that skipped a failed instance keeps its failure", out.get("ok") is False and rows == {"default": True, "ix": False},
           f"outcome={out} err={err.strip()[-200:]}")
    srv = StatusServer({**base, "version": "0.6.0"})
    write_plist(home, "com.leeguoo.iphone-use.ix", srv.port)
    (Path(home) / "Library/LaunchAgents/com.leeguoo.iphone-use.plist").unlink()
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        srv.close()
    out = outcome(home) or {}
    report("a failed instance found current clears the failure", out.get("ok") is True
           and all(r["instance"] != "ix" or r["ok"] for r in out.get("instances", [])),
           f"outcome={out} err={err.strip()[-200:]}")

# Success is the version the restarted daemon reports, not the installer's exit.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    srv = StatusServer(base)  # never reports the new version
    write_plist(home, "com.leeguoo.iphone-use", srv.port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        srv.close()
    out = outcome(home) or {}
    row = (out.get("instances") or [{}])[0]
    report("a daemon still on the old version is a failure, recorded",
           rc == 1 and out.get("ok") is False and row.get("from") == "0.5.4" and row.get("to") == "0.5.4"
           and "still 0.5.4" in (row.get("error") or ""),
           f"rc={rc} outcome={out} err={err.strip()[-200:]}")

with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0")
    write_plist(home, "com.leeguoo.iphone-use", srv.port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log,
                      extra_env={"AUTO_UPDATE_INSTALLER_CMD": "exit 3"})
    finally:
        srv.close()
    row = ((outcome(home) or {}).get("instances") or [{}])[0]
    report("a failed installer is recorded with its reason",
           rc == 1 and row.get("ok") is False and "installer failed" in (row.get("error") or ""),
           f"rc={rc} row={row} err={err.strip()[-200:]}")

with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    (Path(home) / "state").mkdir()
    (Path(home) / "state" / "auto-update.disabled").touch()
    srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0")
    write_plist(home, "com.leeguoo.iphone-use", srv.port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
        disabled_ok = rc == 0 and not installed_instances(log) and "skip: disabled" in err
        rc2, err2 = run("v0.6.0", "--force", home=home, installed_log=log)
    finally:
        srv.close()
    report("the disabled marker stops scheduled runs", disabled_ok, f"rc={rc} err={err.strip()[-200:]}")
    report("--force still runs when disabled", rc2 == 0 and installed_instances(log) == ["default"],
           f"rc={rc2} err={err2.strip()[-200:]}")

with tempfile.TemporaryDirectory() as home:
    rc, err = run("v0.6.0", home=home, installed_log=Path(home) / "installed")
    report("nothing installed → skip", rc == 0 and "skip:not_installed" in err, f"rc={rc} err={err.strip()[-200:]}")

def prebuilt(log):
    p = Path(f"{log}.prebuilt")
    return p.read_text().split() if p.exists() else []

# The upgraded instances' runner products are prebuilt (no phone), so the next
# connect does not build while a person waits; a skipped or failed upgrade is not.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    servers = {"default": StatusServer(base, marker=f"{log}.default", after_version="0.6.0"),
               "ix": StatusServer(base, marker=f"{log}.ix", after_version="0.6.0"),
               "busy": StatusServer({**base, "owner": "agent"})}
    write_plist(home, "com.leeguoo.iphone-use", servers["default"].port)
    write_plist(home, "com.leeguoo.iphone-use.ix", servers["ix"].port)
    write_plist(home, "com.leeguoo.iphone-use.busy", servers["busy"].port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log)
    finally:
        for srv in servers.values(): srv.close()
    report("each upgraded instance is prebuilt, a skipped one is not",
           rc == 0 and sorted(prebuilt(log)) == ["default", "ix"] and "prebuild[ix]: finished (exit 0)" in err,
           f"rc={rc} prebuilt={prebuilt(log)} err={err.strip()[-300:]}")

for name, env, payload, expect in [
    ("a failed upgrade is not prebuilt", {"AUTO_UPDATE_INSTALLER_CMD": "exit 3"}, base, []),
    ("up to date is not prebuilt", {}, {**base, "version": "0.6.0"}, []),
    ("AUTO_UPDATE_PREBUILD=0 turns the prebuild off", {"AUTO_UPDATE_PREBUILD": "0"}, base, []),
    ("a failing prebuild does not fail the run", {"AUTO_UPDATE_PREBUILD_CMD": "exit 4"}, base, None),
]:
    with tempfile.TemporaryDirectory() as home:
        log = Path(home) / "installed"
        srv = StatusServer(payload, marker=f"{log}.default", after_version="0.6.0")
        write_plist(home, "com.leeguoo.iphone-use", srv.port)
        try:
            rc, err = run("v0.6.0", home=home, installed_log=log, extra_env=env)
        finally:
            srv.close()
        if expect is None:
            ok = rc == 0 and "prebuild[default]: finished (exit 4)" in err
        else:
            ok = prebuilt(log) == expect and "prebuild[default]: finished" not in err
        report(name, ok, f"rc={rc} prebuilt={prebuilt(log)} err={err.strip()[-300:]}")

# A prebuild that never ends is stopped, and the run still finishes.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0")
    write_plist(home, "com.leeguoo.iphone-use", srv.port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log,
                      extra_env={"AUTO_UPDATE_PREBUILD_CMD": "exec sleep 30", "AUTO_UPDATE_PREBUILD_TIMEOUT": "2"})
    finally:
        srv.close()
    report("a stuck prebuild is stopped at the timeout", rc == 0 and "stopped after 2s" in err,
           f"rc={rc} err={err.strip()[-300:]}")

# Without the hook, the instance's installed setup-runner.sh is asked to prebuild.
with tempfile.TemporaryDirectory() as home:
    log = Path(home) / "installed"
    state = Path(home) / "state"; (state / "instances" / "ix").mkdir(parents=True)
    args_log = Path(home) / "setup-args"
    for script in (state / "setup-runner.sh", state / "instances" / "ix" / "setup-runner.sh"):
        script.write_text(f"#!/bin/bash\necho \"$IPHONE_USE_INSTANCE $1\" >> '{args_log}'\n")
    servers = [StatusServer(base, marker=f"{log}.{n}", after_version="0.6.0") for n in ("default", "ix")]
    write_plist(home, "com.leeguoo.iphone-use", servers[0].port)
    write_plist(home, "com.leeguoo.iphone-use.ix", servers[1].port)
    try:
        rc, err = run("v0.6.0", home=home, installed_log=log, extra_env={"AUTO_UPDATE_PREBUILD_CMD": ""})
    finally:
        for srv in servers: srv.close()
    got = sorted(args_log.read_text().splitlines()) if args_log.exists() else []
    report("the instance's own setup-runner.sh runs `prebuild`", rc == 0 and got == ["default prebuild", "ix prebuild"],
           f"rc={rc} got={got} err={err.strip()[-300:]}")

# After an upgrade the installed copy of this script refreshes itself from the
# new tag, so a fixed idle gate reaches the machine it runs on.
def self_refresh_case(name, new_body, expect_body_is_new):
    with tempfile.TemporaryDirectory() as home:
        state = Path(home) / "state"; state.mkdir()
        installed = state / "auto-update.sh"
        installed.write_text("#!/bin/bash\n# old copy\n")
        new = Path(home) / "new.sh"; new.write_text(new_body)
        log = Path(home) / "installed"
        srv = StatusServer(base, marker=f"{log}.default", after_version="0.6.0")
        write_plist(home, "com.leeguoo.iphone-use", srv.port)
        try:
            rc, err = run("v0.6.0", home=home, installed_log=log,
                          extra_env={"AUTO_UPDATE_SELF_URL": f"file://{new}"})
        finally:
            srv.close()
        body = installed.read_text()
        ok = rc == 0 and installed_instances(log) and ((body == new_body) == expect_body_is_new) \
            and (body == new_body or body == "#!/bin/bash\n# old copy\n")
        report(name, ok, f"rc={rc} body={body!r} err={err.strip()[-200:]}")

self_refresh_case("upgrade refreshes the installed script", "#!/bin/bash\necho new\n", True)
self_refresh_case("a broken download keeps the current copy", "#!/bin/bash\nif then fi (\n", False)
self_refresh_case("a non-script download keeps the current copy", "<html>404</html>\n", False)
print("FAILED" if failures else "OK", f"({total - failures}/{total})")
sys.exit(1 if failures else 0)
