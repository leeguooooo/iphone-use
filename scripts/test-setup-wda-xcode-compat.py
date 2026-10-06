#!/usr/bin/env python3
"""Xcode-upgrade regressions in setup-wda.sh, with a fake xcodebuild/xcrun and a
throwaway state dir — no Xcode, no device, nothing under the real $HOME.

1. A newer Xcode can reject the runner project's IPHONEOS_DEPLOYMENT_TARGET (as
   Xcode 27 rejected WebDriverAgent's 12.0). Setup must raise it through an
   exported XCODE_XCCONFIG_FILE, never through the xcodebuild argv (that argv
   is the runner's PID identity).
2. After an SDK upgrade, Build/Products holds one .xctestrun per SDK. The
   resolver must pick the one for the SDK just built, and still refuse when the
   choice is genuinely ambiguous.
3. `doctor` reports both conditions.
"""
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()


def section(name):
    return SOURCE.split(f'# BEGIN {name}.', 1)[1].split(f'# END {name}.', 1)[0]


ASC_HELPERS = section('ASC signing helpers')
COMPAT_HELPERS = section('Xcode compatibility helpers')

FAKE_XCODEBUILD = r'''#!/bin/bash
if [ "${1:-}" = "-version" ]; then
    printf 'Xcode %s\nBuild version 99Z999\n' "$FAKE_XCODE"
    exit 0
fi
{
    printf 'argv:'
    printf ' %s' "$@"
    printf '\nxcconfig:%s\n' "${XCODE_XCCONFIG_FILE:-<unset>}"
} >> "$FAKE_XCODEBUILD_LOG"
'''

FAKE_XCRUN = r'''#!/bin/bash
case "$*" in
    "--sdk iphoneos --show-sdk-path") printf '%s\n' "$FAKE_SDK_PATH" ;;
    "--sdk iphoneos --show-sdk-version")
        [ -n "${FAKE_SDK_VERSION:-}" ] || exit 1
        printf '%s\n' "$FAKE_SDK_VERSION" ;;
    *) exit 1 ;;
esac
'''


class XcodeCompatTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='wda-xcode-compat-test-')
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / 'home'
        self.state = self.home / '.iphone-use'
        self.state.mkdir(parents=True)
        self.project = self.state / 'runner' / 'IPhoneUseRunner' / 'IPhoneUseRunner.xcodeproj'
        self.project.mkdir(parents=True)
        self.set_project_targets('12.0', '12.0')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        for name, body in (('xcodebuild', FAKE_XCODEBUILD), ('xcrun', FAKE_XCRUN)):
            path = self.bin / name
            path.write_text(body)
            path.chmod(0o755)
        self.sdk = self.root / 'iPhoneOS27.0.sdk'
        self.sdk.mkdir()
        self.set_sdk_minimum('15.0')
        self.log = self.root / 'xcodebuild.log'
        self.xcconfig = self.state / 'wda-xcode-compat.xcconfig'
        self.derived = self.state / 'runner-build'
        self.products = self.derived / 'Build/Products'
        (self.products / 'Debug-iphoneos').mkdir(parents=True)

    def tearDown(self):
        self.temp.cleanup()

    def set_project_targets(self, *values):
        lines = ''.join(f'\t\t\t\tIPHONEOS_DEPLOYMENT_TARGET = {v};\n' for v in values)
        (self.project / 'project.pbxproj').write_text(
            '// !$*UTF8*$!\n{\n' + lines + '\t\t\t\tTVOS_DEPLOYMENT_TARGET = 10.0;\n}\n')

    def set_sdk_minimum(self, minimum):
        settings = self.sdk / 'SDKSettings.plist'
        if minimum is None:
            settings.unlink(missing_ok=True)
            return
        with settings.open('wb') as handle:
            plistlib.dump({'SupportedTargets': {'iphoneos': {
                'MinimumDeploymentTarget': minimum}}}, handle)

    def xctestrun(self, *sdk_arch):
        paths = []
        for name in sdk_arch:
            path = self.products / f'IPhoneUseRunner_iphoneos{name}.xctestrun'
            path.write_text('plist')
            paths.append(path)
        return paths

    def run_bash(self, body, *, xcode='27.0', sdk_version='27.0', env=None,
                 self_install=None):
        script = self.root / 'run.sh'
        script.write_text(
            'set -eu\n'
            f'STATE_DIR="{self.state}"\nRUNNER_PROJECT="{self.project}"\n'
            f'RUNNER_DERIVED_DATA="{self.derived}"\nRUNNER_SCHEME=IPhoneUseRunner\n'
            f'SELF_INSTALL="{self_install or self.state / "setup-wda.sh"}"\n'
            'ok() { printf "ok: %s\\n" "$*"; }\n'
            'warn() { printf "warn: %s\\n" "$*"; }\n'
            '_safe_expected() { return 0; }\n'
            + ASC_HELPERS + '\n' + COMPAT_HELPERS + '\n' + body + '\n')
        base = {k: v for k, v in os.environ.items() if k != 'XCODE_XCCONFIG_FILE'}
        base.update({
            'HOME': str(self.home),
            'PATH': f'{self.bin}:/usr/bin:/bin',
            'FAKE_XCODE': xcode,
            'FAKE_SDK_PATH': str(self.sdk),
            'FAKE_SDK_VERSION': sdk_version,
            'FAKE_XCODEBUILD_LOG': str(self.log),
        })
        base.update(env or {})
        proc = subprocess.run(['bash', str(script)], capture_output=True, text=True, env=base)
        return proc.returncode, proc.stdout + proc.stderr

    # ── 1. deployment target override ─────────────────────────────────────
    def test_xcode27_writes_override_and_exports_it_without_touching_argv(self):
        code, out = self.run_bash(
            '_prepare_wda_xcconfig\n'
            'echo "override=$WDA_DEPLOYMENT_TARGET_OVERRIDE sdk=$WDA_IOS_SDK_VERSION"\n'
            'XCODEBUILD_BIN="$(command -v xcodebuild)"\n'
            '_runner_xcodebuild -destination platform=iOS,id=0000 -allowProvisioningUpdates '
            'DEVELOPMENT_TEAM=TEAM000000 PRODUCT_BUNDLE_IDENTIFIER=com.example.wda build-for-testing\n')
        self.assertEqual(code, 0, out)
        self.assertIn('override=15.0 sdk=27.0', out)
        content = self.xcconfig.read_text()
        self.assertIn('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n', content)
        self.assertNotIn('#include', content)
        self.assertEqual(self.xcconfig.stat().st_mode & 0o777, 0o600)
        log = self.log.read_text().splitlines()
        self.assertEqual(log[0], f'argv: -project {self.project} -scheme IPhoneUseRunner '
                                 f'-derivedDataPath {self.derived} '
                                 '-destination platform=iOS,id=0000 -allowProvisioningUpdates '
                                 'DEVELOPMENT_TEAM=TEAM000000 PRODUCT_BUNDLE_IDENTIFIER=com.example.wda '
                                 'build-for-testing')
        self.assertEqual(log[1], f'xcconfig:{self.xcconfig}')
        # The runner sources are never edited.
        self.assertIn('IPHONEOS_DEPLOYMENT_TARGET = 12.0;', (self.project / 'project.pbxproj').read_text())

    def test_override_uses_highest_project_value_when_above_minimum(self):
        self.set_project_targets('12.0', '16.0')
        code, out = self.run_bash('_prepare_wda_xcconfig; echo "override=$WDA_DEPLOYMENT_TARGET_OVERRIDE"')
        self.assertEqual(code, 0, out)
        self.assertIn('override=16.0', out)
        self.assertIn('IPHONEOS_DEPLOYMENT_TARGET = 16.0', self.xcconfig.read_text())

    def test_older_xcode_leaves_the_build_alone_and_retires_a_stale_override(self):
        self.set_sdk_minimum('12.0')
        self.xcconfig.write_text('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n')
        code, out = self.run_bash(
            f'export XCODE_XCCONFIG_FILE="{self.xcconfig}"\n'
            '_prepare_wda_xcconfig\n'
            'echo "override=[$WDA_DEPLOYMENT_TARGET_OVERRIDE] env=[${XCODE_XCCONFIG_FILE:-}]"\n',
            xcode='26.5', sdk_version='26.5')
        self.assertEqual(code, 0, out)
        self.assertIn('override=[] env=[]', out)
        self.assertFalse(self.xcconfig.exists())

    def test_project_already_supported_needs_no_override(self):
        self.set_project_targets('15.0', '15.0')
        code, out = self.run_bash('_prepare_wda_xcconfig; echo "override=[$WDA_DEPLOYMENT_TARGET_OVERRIDE]"')
        self.assertIn('override=[]', out)
        self.assertFalse(self.xcconfig.exists())

    def test_unreadable_sdk_settings_fall_back_by_xcode_major(self):
        self.set_sdk_minimum(None)
        code, out = self.run_bash('_prepare_wda_xcconfig; echo "override=[$WDA_DEPLOYMENT_TARGET_OVERRIDE]"')
        self.assertIn('override=[15.0]', out)
        code, out = self.run_bash('_prepare_wda_xcconfig; echo "override=[$WDA_DEPLOYMENT_TARGET_OVERRIDE]"',
                                  xcode='26.5', sdk_version='26.5')
        self.assertIn('override=[]', out)

    def test_inherited_xcconfig_is_included_not_dropped(self):
        manual = self.state / 'wda-override.xcconfig'
        manual.write_text('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n')
        code, out = self.run_bash(
            f'export XCODE_XCCONFIG_FILE="{manual}"\n_prepare_wda_xcconfig\n'
            'echo "env=$XCODE_XCCONFIG_FILE"')
        self.assertEqual(code, 0, out)
        self.assertIn(f'env={self.xcconfig}', out)
        self.assertIn(f'#include? "{manual}"\n', self.xcconfig.read_text())

    def test_symlinked_xcconfig_is_refused(self):
        target = self.root / 'elsewhere'
        target.write_text('keep')
        self.xcconfig.symlink_to(target)
        code, out = self.run_bash('_prepare_wda_xcconfig || echo refused')
        self.assertIn('refused', out)
        self.assertEqual(target.read_text(), 'keep')

    # ── 2. .xctestrun resolution ──────────────────────────────────────────
    def resolve(self, sdk_arg='', **kwargs):
        code, out = self.run_bash(
            f'if _resolve_xctestrun "{self.products}/Debug-iphoneos" "{sdk_arg}"; '
            'then :; else echo AMBIGUOUS; fi', **kwargs)
        self.assertEqual(code, 0, out)
        return out.strip()

    def test_single_xctestrun_is_used_as_before(self):
        (only,) = self.xctestrun('26.5-arm64')
        self.assertEqual(self.resolve(), str(only))

    def test_sdk_upgrade_picks_the_xctestrun_for_the_built_sdk(self):
        _stale, fresh = self.xctestrun('26.5-arm64', '27.0-arm64')
        self.assertEqual(self.resolve('27.0'), str(fresh))
        # Without the build-settings SDK, fall back to the selected SDK.
        self.assertEqual(self.resolve(''), str(fresh))

    def test_no_or_several_matches_stay_ambiguous(self):
        self.xctestrun('26.4-arm64', '26.5-arm64')
        self.assertEqual(self.resolve('27.0'), 'AMBIGUOUS')
        self.assertEqual(self.resolve('', sdk_version=''), 'AMBIGUOUS')
        self.xctestrun('27.0-arm64', '27.0-arm64e')
        self.assertEqual(self.resolve('27.0'), 'AMBIGUOUS')

    def test_empty_products_dir_is_ambiguous(self):
        self.assertEqual(self.resolve('27.0'), 'AMBIGUOUS')

    def test_launch_path_resolves_the_xctestrun_and_refuses_ambiguity(self):
        launch = SOURCE.split('\n_ensure_launchable_runner() {', 1)[1].split('\n}\n', 1)[0]
        self.assertIn('_resolve_xctestrun "$products"', launch)
        self.assertIn('could not resolve a unique .xctestrun', launch)

    # ── 3. doctor ─────────────────────────────────────────────────────────
    def doctor(self, **kwargs):
        return self.run_bash('rc=0; _doctor_xcode_compat || rc=$?; echo "rc=$rc"', **kwargs)

    def test_doctor_warns_when_xcode27_has_no_override_yet(self):
        code, out = self.doctor()
        self.assertIn('warn: ~ Xcode 27 supports iOS deployment targets from 15.0, but the runner project sets 12.0', out)
        self.assertIn('rc=0', out)

    def test_doctor_fails_when_installed_script_predates_the_override(self):
        installed = self.state / 'setup-wda.sh'
        installed.write_text('#!/bin/bash\n# old copy\n')
        code, out = self.doctor()
        self.assertIn('warn: X Xcode 27', out)
        self.assertIn('rc=1', out)

    def test_doctor_is_quiet_once_the_override_is_in_place(self):
        self.xcconfig.write_text('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n')
        code, out = self.doctor()
        self.assertIn('ok: Xcode 27 deployment target override', out)
        self.assertNotIn('warn:', out)

    def test_doctor_says_nothing_on_an_older_xcode(self):
        self.set_sdk_minimum('12.0')
        code, out = self.doctor(xcode='26.5', sdk_version='26.5')
        self.assertNotIn('warn:', out)
        self.assertIn('rc=0', out)

    def test_doctor_lists_multiple_xctestrun_files(self):
        self.xcconfig.write_text('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n')
        self.xctestrun('26.5-arm64', '27.0-arm64')
        code, out = self.doctor()
        self.assertIn('warn: ~ multiple .xctestrun files in', out)
        self.assertIn('IPhoneUseRunner_iphoneos26.5-arm64.xctestrun', out)
        self.assertIn('setup uses the one for the current SDK (iphoneos27.0)', out)
        self.assertIn('rc=0', out)
        code, out = self.doctor(sdk_version='28.0')
        self.assertIn('none is uniquely for the current SDK', out)

    def test_doctor_quiet_with_one_xctestrun(self):
        self.xcconfig.write_text('IPHONEOS_DEPLOYMENT_TARGET = 15.0\n')
        self.xctestrun('27.0-arm64')
        code, out = self.doctor()
        self.assertNotIn('multiple .xctestrun', out)


if __name__ == '__main__':
    unittest.main()
