#!/usr/bin/env python3
"""Build the iOS app without printing signing credentials or raw command lines."""
import os
from pathlib import Path
import plistlib
import subprocess
import sys

ROOT = Path(__file__).resolve().parent
mode = sys.argv[1] if len(sys.argv) > 1 else "unsigned"
if mode not in ("unsigned", "signed"):
    raise SystemExit("usage: build.sh [unsigned|signed]")
if (Path(os.environ.get("IPU_DDI_RESTORE", "/Library/Developer/DeveloperDiskImages/iOS_DDI/Restore"))/"BuildManifest.plist").exists():
    subprocess.run([sys.executable, str(ROOT/"prepare-image.py")], check=True)
subprocess.run(["xcodegen", "generate", "--spec", "project.yml"], cwd=ROOT, check=True)
args = ["xcodebuild", "-project", "PhoneSelfHostProbe.xcodeproj", "-scheme", "PhoneSelfHostProbe",
        "-configuration", "Debug", "-destination", "generic/platform=iOS", "-derivedDataPath", "build"]
if mode == "unsigned":
    args.append("CODE_SIGNING_ALLOWED=NO")
else:
    args.append("-allowProvisioningUpdates")
    env = {}
    profile = Path(os.environ.get("IPU_RUNNER_PLIST", str(Path.home()/"Library/LaunchAgents/com.leeguoo.iphone-use.runner.plist")))
    if profile.exists():
        env = plistlib.loads(profile.read_bytes()).get("EnvironmentVariables", {})
    keys = ["IPU_RUNNER_ASC_KEY_PATH", "IPU_RUNNER_ASC_KEY_ID", "IPU_RUNNER_ASC_ISSUER_ID"]
    values = [os.environ.get(key) or env.get(key) for key in keys]
    if all(values):
        for flag, value in zip(["-authenticationKeyPath", "-authenticationKeyID", "-authenticationKeyIssuerID"], values):
            args.extend([flag, value])
    print("Signing: existing ASC configuration" if all(values) else "Signing: Xcode account/profile", flush=True)
lib = ROOT/"launcher/target/aarch64-apple-ios/release/libipu_phone_host.a"
linked = lib.exists()
if linked:
    roots = " ".join("-u _"+name for name in ["ipu_host_start", "ipu_host_stop", "ipu_host_status", "ipu_host_string_free", "ipu_host_copy_pair_record"])
    args.append(f'IPU_HOST_LDFLAGS=-force_load "{lib}" -framework Security {roots}')
    print("Native phone host linked", flush=True)
else:
    print("Network probe build; native launcher not linked", flush=True)
if os.environ.get("IPU_DEVELOPMENT_TEAM"):
    args.append("DEVELOPMENT_TEAM="+os.environ["IPU_DEVELOPMENT_TEAM"])
args.append("build")
logs = ROOT/"build/logs"
logs.mkdir(parents=True, exist_ok=True)
log = logs/"xcodebuild.log"
fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as output:
    result = subprocess.run(args, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
if result.returncode:
    for line in log.read_text(errors="replace").splitlines():
        if "error:" in line:
            print(line)
    print("Build failed; private log: build/logs/xcodebuild.log")
    raise SystemExit(result.returncode)
app = ROOT/"build/Build/Products/Debug-iphoneos/PhoneSelfHostProbe.app"
if mode == "signed":
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
if linked:
    binary = app/"PlugIns/PhoneLocalTunnel.appex/PhoneLocalTunnel"
    symbols = subprocess.check_output(["nm", "-gU", str(binary)], text=True)
    if any(name not in symbols for name in ["_ipu_host_start", "_ipu_host_status", "_ipu_host_stop"]):
        raise SystemExit("Native ABI symbols missing")
print("BUILD SUCCEEDED; "+str(app))
