#!/usr/bin/env python3
"""Build a separately identified runner, with the on-phone daemon embedded."""
import os
from pathlib import Path
import plistlib
import subprocess

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]
lib = Path(os.environ.get("IPU_EMBED_LIB", str(REPO/"target/aarch64-apple-ios/release/libipu_embed.a"))).resolve()
if not lib.exists():
    raise SystemExit("Build scripts/build-ios-embed.sh first")
profile = Path(os.environ.get("IPU_RUNNER_PLIST", str(Path.home()/"Library/LaunchAgents/com.leeguoo.iphone-use.runner.plist")))
env = plistlib.loads(profile.read_bytes()).get("EnvironmentVariables", {}) if profile.exists() else {}
keys = ["IPU_RUNNER_ASC_KEY_PATH", "IPU_RUNNER_ASC_KEY_ID", "IPU_RUNNER_ASC_ISSUER_ID"]
values = [os.environ.get(key) or env.get(key) for key in keys]
args = ["xcodebuild", "-project", str(REPO/"runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj"),
        "-scheme", "IPhoneUseRunner", "-configuration", "Debug", "-destination", "generic/platform=iOS",
        "-derivedDataPath", str(ROOT/"build/runner"), "-allowProvisioningUpdates",
        "PRODUCT_BUNDLE_IDENTIFIER=com.leeguoo.iphone-use.selfhost-runner",
        f'OTHER_LDFLAGS=$(inherited) -force_load "{lib}" -framework Security -framework SystemConfiguration -u _ipu_daemon_start -u _ipu_daemon_state -u _ipu_daemon_error -u _ipu_daemon_pair_code']
if all(values):
    for flag, value in zip(["-authenticationKeyPath", "-authenticationKeyID", "-authenticationKeyIssuerID"], values):
        args.extend([flag, value])
args.append("build-for-testing")
if os.environ.get("IPU_SELFHOST_DIAGNOSTIC") == "1":
    args.append("SWIFT_ACTIVE_COMPILATION_CONDITIONS=$(inherited) DEBUG IPU_SELFHOST_DIAGNOSTIC")
logs = ROOT/"build/logs"; logs.mkdir(parents=True, exist_ok=True)
log = logs/"runner-build.log"
with os.fdopen(os.open(log, os.O_WRONLY|os.O_CREAT|os.O_TRUNC, 0o600), "w") as output:
    result = subprocess.run(args, cwd=REPO, stdout=output, stderr=subprocess.STDOUT)
if result.returncode:
    for line in log.read_text(errors="replace").splitlines():
        if "error:" in line: print(line)
    raise SystemExit(result.returncode)
app = ROOT/"build/runner/Build/Products/Debug-iphoneos/iPhoneUse-Runner.app"
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
info = plistlib.loads((app/"Info.plist").read_bytes())
if info["CFBundleIdentifier"] != "com.leeguoo.iphone-use.selfhost-runner.xctrunner":
    raise SystemExit("Unexpected runner bundle identity")
print("Signed self-host runner ready: "+str(app))
