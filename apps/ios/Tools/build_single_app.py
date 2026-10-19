#!/usr/bin/env python3
"""Build the iOS app for one signing team. Produces one installable app, never installs it.

Full app (default): the remote app plus local control, assembled with the
runner's PhoneUse.xctest into one application. Needs a paid team (the
packet-tunnel extension), the on-phone daemon and launcher libraries, and the
Apple developer image prepared from this Mac's Xcode.

--remote-only: the app without local control. A free Apple ID can sign it.

`iphone-use setup-app` runs this from the release's app sources with the
user's own team and bundle identifier; a repo checkout can run it directly.
"""
import argparse
import os
import plistlib
import shutil
import subprocess
from pathlib import Path
from package_single_app import DEFAULT_BUNDLE_ID, assemble, sign

ROOT = Path(__file__).resolve().parents[3]
IOS = ROOT / "apps/ios"
DEFAULT_TEAM = "6ZPXG4KVVS"


def run(command, log):
    log.parent.mkdir(parents=True, exist_ok=True)
    with os.fdopen(os.open(log, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as output:
        result = subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        for line in log.read_text(errors="replace").splitlines():
            if "error:" in line:
                print(line)
        raise SystemExit("Build failed; private log: " + str(log))


def asc_auth(team):
    """-allowProvisioningUpdates, plus App Store Connect key flags when configured.

    setup-app passes the phone's key in the environment. Only our own team falls
    back to the default runner's key: another team's build must not carry it."""
    profile = Path.home() / "Library/LaunchAgents/com.leeguoo.iphone-use.runner.plist"
    env = {}
    if team == DEFAULT_TEAM and profile.exists():
        env = plistlib.loads(profile.read_bytes()).get("EnvironmentVariables", {})
    auth = ["-allowProvisioningUpdates"]
    for key, flag in [("IPU_RUNNER_ASC_KEY_PATH", "-authenticationKeyPath"),
                      ("IPU_RUNNER_ASC_KEY_ID", "-authenticationKeyID"),
                      ("IPU_RUNNER_ASC_ISSUER_ID", "-authenticationKeyIssuerID")]:
        value = os.environ.get(key) or env.get(key)
        if value:
            auth.extend([flag, value])
    return auth


def project(spec, generate):
    """The generated project for a spec: regenerated from a checkout, shipped in a release.

    Both live in apps/ios, because their build settings name files relative to it."""
    path = IOS / ("IPhoneUseRemote.xcodeproj" if spec == "project.yml" else "PhoneUseRemoteOnly.xcodeproj")
    if generate:
        subprocess.run(["xcodegen", "generate", "--quiet", "--spec", str(IOS / spec)], cwd=ROOT, check=True)
    if not path.is_dir():
        raise SystemExit("Project missing: " + str(path))
    return path


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--team", default=DEFAULT_TEAM)
    parser.add_argument("--bundle-id", default=DEFAULT_BUNDLE_ID)
    parser.add_argument("--configuration", default="Release", choices=["Release", "Debug"])
    parser.add_argument("--remote-only", action="store_true")
    parser.add_argument("--host-lib", type=Path)
    parser.add_argument("--embed-lib", type=Path)
    parser.add_argument("--runner-project", type=Path, default=ROOT / "runner/IPhoneUseRunner/IPhoneUseRunner.xcodeproj")
    parser.add_argument("--runner-bundle-id", help="the team's runner identifier, as setup signs it")
    parser.add_argument("--build-dir", type=Path, default=IOS / "build-single-app")
    parser.add_argument("--no-generate", action="store_true", help="use the shipped projects, do not run xcodegen")
    args = parser.parse_args()

    output = args.output.resolve()
    if output.exists():
        raise SystemExit("Output already exists; use a new output path")
    build = args.build_dir.resolve()
    generate = not args.no_generate and shutil.which("xcodegen") is not None
    signing = ["DEVELOPMENT_TEAM=" + args.team, "IPU_APP_BUNDLE_ID=" + args.bundle_id]
    common = ["xcodebuild", "-configuration", args.configuration, "-destination", "generic/platform=iOS", "-jobs", "2"]
    products = args.configuration + "-iphoneos"

    if args.remote_only:
        run(common + ["-project", str(project("project.remote.yml", generate)), "-scheme", "IPhoneUseRemote",
                      "-derivedDataPath", str(build / "remote")] + signing + asc_auth(args.team) + ["build"],
            build / "remote-build.log")
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(build / "remote/Build/Products" / products / "Phone Use Remote.app", output, symlinks=True)
        print("Remote app ready (no local control): " + str(output))
        raise SystemExit(0)

    for library in (args.host_lib, args.embed_lib):
        if library is None or not library.is_file():
            raise SystemExit("Required native library missing (--host-lib, --embed-lib)")
    if not (ROOT / "experiments/phone-self-host/Resources/DeveloperImage/BuildManifest.plist").is_file():
        raise SystemExit("Prepare the developer image first: python3 experiments/phone-self-host/prepare-image.py")
    host_flags = '-force_load "' + str(args.host_lib.resolve()) + '" -framework Security'
    host_flags += " " + " ".join("-u _" + name for name in ["ipu_host_start", "ipu_host_stop", "ipu_host_status", "ipu_host_string_free", "ipu_host_copy_pair_record"])
    run(common + ["-project", str(project("project.yml", generate)), "-scheme", "IPhoneUseRemote",
                  "-derivedDataPath", str(build / "main"), "IPU_HOST_LDFLAGS=" + host_flags] + signing + asc_auth(args.team) + ["build"],
        build / "main-build.log")
    embed_flags = '$(inherited) -force_load "' + str(args.embed_lib.resolve()) + '" -framework Security -framework SystemConfiguration'
    embed_flags += " " + " ".join("-u _" + name for name in ["ipu_daemon_start", "ipu_daemon_state", "ipu_daemon_error", "ipu_daemon_pair_code"])
    runner_signing = ["DEVELOPMENT_TEAM=" + args.team]
    if args.runner_bundle_id:
        runner_signing.append("PRODUCT_BUNDLE_IDENTIFIER=" + args.runner_bundle_id)
    run(common + ["-project", str(args.runner_project), "-scheme", "IPhoneUseRunner",
                  "-derivedDataPath", str(build / "control"), "PRODUCT_NAME=PhoneUse",
                  "OTHER_LDFLAGS=" + embed_flags] + runner_signing + asc_auth(args.team) + ["build-for-testing"],
        build / "control-build.log")
    main = build / "main/Build/Products" / products / "Phone Use Remote.app"
    runner = build / "control/Build/Products" / products / "PhoneUse-Runner.app"
    candidate = assemble(main, runner, output, args.bundle_id)
    sign(candidate, main)
    print("Full app ready (local control included): " + str(candidate))
