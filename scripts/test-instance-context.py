#!/usr/bin/env python3
"""Shell side of the instance derivation (#67), against the shared fixture.

scripts/fixtures/instance-derivation.json pins what crates/server/src/instance.rs
derives; this checks that setup-wda.sh `instance-context` and uninstall.sh
--instance agree with it, and that the script-only rules hold: per-name port
slots, skipping ports another instance claims, refusing to bind one phone to
two daemons, and an installed copy refusing to run as another instance.

Everything runs against a throwaway HOME; no launchd job, port or phone is
touched.
"""
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SETUP = ROOT / "scripts" / "setup-wda.sh"
UNINSTALL = ROOT / "uninstall.sh"
FIXTURE = json.loads((ROOT / "scripts" / "fixtures" / "instance-derivation.json").read_text())
FIXTURE_HOME = "/Users/leo"
# setup-wda.sh hands instance-context to the setup engine; use this
# workspace's build of it.
BIN = ROOT / "target" / "debug" / "iphone-use"
if not BIN.exists():
    subprocess.run(["cargo", "build", "-q", "-p", "server", "--bin", "iphone-use"],
                   cwd=ROOT, check=True)

failures = []


def check(cond, message):
    if not cond:
        failures.append(message)
        print(f"FAIL: {message}")


def run(script, args, home, env_extra=None, unset=()):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("IPHONE_USE_", "PHONE_REMOTE_", "WDA_", "MJPEG_"))}
    env["HOME"] = str(home)
    env["IPHONE_USE_SETUP_BIN"] = str(BIN)
    for key in unset:
        env.pop(key, None)
    env.update(env_extra or {})
    return subprocess.run(["/bin/bash", str(script), *args], env=env,
                          capture_output=True, text=True, timeout=60)


def context(home, name, env_extra=None, script=SETUP):
    extra = dict(env_extra or {})
    if name is not None:
        extra["IPHONE_USE_INSTANCE"] = name
    result = run(script, ["instance-context"], home, extra)
    values = {}
    for line in result.stdout.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            values[key] = value
    return result, values


def write_plist(home, label, env, program=None):
    agents = home / "Library" / "LaunchAgents"
    agents.mkdir(parents=True, exist_ok=True)
    data = {"Label": label, "ProgramArguments": program or ["/bin/true"],
            "EnvironmentVariables": env}
    (agents / f"{label}.plist").write_bytes(plistlib.dumps(data))


def fresh_home(tmp, name):
    home = pathlib.Path(tmp) / name
    (home / "Library" / "LaunchAgents").mkdir(parents=True)
    return home


def main():
    with tempfile.TemporaryDirectory() as raw_tmp:
        tmp = os.path.realpath(raw_tmp)

        # 1. The fixture's derivation rows.
        home = fresh_home(tmp, "derive")
        for case in FIXTURE["cases"]:
            extra = {}
            if "state_dir_override" in case:
                extra["IPHONE_USE_STATE_DIR"] = case["state_dir_override"]
            result, got = context(home, case["name"], extra)
            label = case["name"] or "<empty>"
            check(result.returncode == 0, f"{label}: instance-context failed: {result.stderr.strip()}")
            for key in ("state_dir", "daemon_label", "wda_label", "wda_plist"):
                want = case[key].replace(FIXTURE_HOME, str(home), 1)
                check(got.get(key) == want, f"{label}: {key} = {got.get(key)!r}, want {want!r}")

        for bad in FIXTURE["invalid_names"]:
            result, _ = context(home, bad)
            check(result.returncode == 2, f"setup-wda.sh accepted invalid name {bad!r}")
            result = run(UNINSTALL, ["--instance", bad, "--dry-run"], home)
            check(result.returncode == 2, f"uninstall.sh accepted invalid name {bad!r}")
        for reserved in FIXTURE["reserved_names"]:
            check(reserved in FIXTURE["invalid_names"], f"reserved {reserved!r} missing from invalid_names")

        for bad in FIXTURE["invalid_state_dir_overrides"]:
            if FIXTURE_HOME.startswith(bad.rstrip("/") + "/"):
                bad = str(home.parent)  # an ancestor of HOME stays an ancestor
            else:
                bad = bad.replace(FIXTURE_HOME, str(home), 1)
            result, _ = context(home, "lab", {"IPHONE_USE_STATE_DIR": bad})
            check(result.returncode == 2, f"state dir override {bad!r} was accepted")

        # 2. Ports: default keeps its own; named instances take their slot.
        ports = FIXTURE["port_derivation"]
        _, got = context(home, "default")
        for key, want in ports["default"].items():
            check(got.get(key) == str(want), f"default {key} = {got.get(key)}, want {want}")
        for name, want in ports["first_slot"].items():
            _, got = context(home, name)
            first = " ".join(str(want[k]) for k in ("daemon_port", "wda_port", "mjpeg_port"))
            check(got.get("first_slot_ports") == first,
                  f"{name}: first slot {got.get('first_slot_ports')!r}, want {first!r}")
            # Nothing else is configured, so the first slot is the one taken
            # unless this machine happens to listen on one of its ports.
            if got.get("daemon_port") != str(want["daemon_port"]):
                print(f"note: {name} skipped its first slot (a local listener holds one of {first})")

        # 3. Another instance's ports are skipped, and its phone is refused.
        home = fresh_home(tmp, "claimed")
        lab = ports["first_slot"]["lab"]
        write_plist(home, "com.leeguoo.iphone-use", {
            "IPHONE_USE_PORT": "45432", "IPHONE_USE_UDID": "00008150-AAAA"})
        write_plist(home, "com.leeguoo.iphone-use.wda", {
            "WDA_UDID": "00008150-AAAA", "WDA_PORT": "8100", "MJPEG_PORT": "9100"})
        write_plist(home, "com.leeguoo.iphone-use.other", {
            "IPHONE_USE_INSTANCE": "other", "IPHONE_USE_PORT": str(lab["daemon_port"]),
            "IPHONE_USE_UDID": "00008110-BBBB"})
        # Maintenance agents share the prefix but bind nothing.
        write_plist(home, "com.leeguoo.iphone-use.autoupdate", {"IPHONE_USE_PORT": "1"})
        _, got = context(home, "lab")
        check(got.get("daemon_port") == str(lab["daemon_port"] + 1),
              f"lab did not skip the slot instance 'other' claims: {got.get('daemon_port')}")
        result, _ = context(home, "lab", {"WDA_UDID": "00008150-aaaa"})
        check(result.returncode == 1 and 'instance "default"' in result.stderr,
              f"binding the default phone to lab was not refused: {result.returncode} {result.stderr.strip()}")
        result, _ = context(home, "lab", {"WDA_UDID": "00008110-BBBB"})
        check(result.returncode == 1 and 'instance "other"' in result.stderr,
              "binding other's phone to lab was not refused")
        result, _ = context(home, "lab", {"WDA_UDID": "00008110-CCCC", "WDA_PORT": "8100"})
        check(result.returncode == 1 and "TCP 8100" in result.stderr,
              "a named instance was allowed the default instance's WDA port")
        result, _ = context(home, "default", {"IPHONE_USE_PORT": str(lab["daemon_port"])})
        check(result.returncode == 1, "the default instance was allowed a port 'other' owns")
        result, got = context(home, "default")
        check(result.returncode == 0 and got.get("daemon_port") == "45432",
              f"the default instance's own config was disturbed: {result.stderr.strip()}")

        # 4. An installed copy is its instance's, whatever the environment says.
        installed = home / ".iphone-use" / "instances" / "lab" / "setup-wda.sh"
        installed.parent.mkdir(parents=True)
        shutil.copy(SETUP, installed)
        result, got = context(home, None, script=installed)
        check(result.returncode == 0 and got.get("name") == "lab",
              f"installed lab copy did not resolve as lab: {got.get('name')} {result.stderr.strip()}")
        result, _ = context(home, "default", script=installed)
        check(result.returncode == 2, "installed lab copy ran as the default instance")
        default_copy = home / ".iphone-use" / "setup-wda.sh"
        shutil.copy(SETUP, default_copy)
        result, _ = context(home, "lab", script=default_copy)
        check(result.returncode == 2, "the default installed copy ran as instance lab")

        # 5. uninstall.sh: the default refuses while named instances remain;
        # a named dry run plans only that instance's paths.
        result = run(UNINSTALL, ["--dry-run"], home)
        check(result.returncode == 1 and "--instance other" in result.stderr,
              f"default uninstall did not refuse with named instances present: {result.returncode} {result.stderr.strip()}")
        home = fresh_home(tmp, "uninstall")
        (home / ".iphone-use" / "instances" / "lab").mkdir(parents=True, mode=0o700)
        os.chmod(home / ".iphone-use", 0o700)
        os.chmod(home / ".iphone-use" / "instances", 0o700)
        result = run(UNINSTALL, ["--instance", "lab", "--dry-run"], home)
        check(result.returncode == 0, f"named dry-run uninstall failed: {result.stdout}{result.stderr}")
        planned = [line for line in result.stdout.splitlines() if line.startswith("DRY-RUN: remove")]
        stray = [line for line in planned if "/instances/lab" not in line]
        check(not stray, f"named uninstall planned paths outside its instance: {stray}")

    if failures:
        print(f"\n{len(failures)} failure(s)")
        return 1
    print("instance context: all checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
