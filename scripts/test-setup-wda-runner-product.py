#!/usr/bin/env python3
"""Validate/repair synthetic runner products; no Xcode build or phone access."""
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()
HELPERS = SOURCE.split('# BEGIN runner product validation.', 1)[1].split('# END runner product validation.', 1)[0]
HELPERS += SOURCE.split('# BEGIN ASC signing helpers.', 1)[1].split('# END ASC signing helpers.', 1)[0]
for _name in ('_safe_expected', '_resolve_xctestrun', '_valid_os_version'):
    HELPERS += '\n' + _name + '() {' + SOURCE.split('\n' + _name + '() {', 1)[1].split('\n}\n', 1)[0] + '\n}\n'


def create_runner(app, poison=False):
    for bundle, name in [(app, 'RunnerBinary'),
                         (app / 'Frameworks/Testing.framework', 'Testing'),
                         (app / 'PlugIns/RunnerTests.xctest', 'TestBinary')]:
        bundle.mkdir(parents=True, exist_ok=True)
        (bundle / 'Info.plist').write_bytes(plistlib.dumps({'CFBundleExecutable': name}))
        (bundle / name).write_bytes(b'fixture executable')
        (bundle / name).chmod(0o755)
    if poison:
        (app / 'Frameworks/Testing.framework/Info.plist').unlink()


class ProductTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='wda-product-test-')
        self.root = Path(self.temp.name)
        self.products = self.root / 'DerivedData/Build/Products/Debug-iphoneos'
        self.app = self.products / 'iPhoneUse-Runner.app'
        create_runner(self.app)
        self.helpers = self.root / 'helpers.sh'
        self.helpers.write_text(HELPERS)
        self.calls = self.root / 'build-calls'
        self.status = self.root / 'status'

    def tearDown(self):
        self.temp.cleanup()

    def run_shell(self, body, *, poison=False, repair_used=False, app=None):
        harness = self.root / 'run.sh'
        harness.write_text('''set -eu
. "$HELPERS"
RUNNER_APP_NAME=iPhoneUse-Runner.app
RUNNER_PRODUCTS_DIR="$PRODUCTS"
RUNNER_BUILD_LOCKED=0
WDA_RUNNER_REPAIR_ATTEMPTED="$REPAIR_USED"
_ios_sdk_version() { return 1; }
_setstatus() { printf '%s\\n' "$*" >> "$STATUS_LOG"; }
warn() { printf '%s\\n' "$*"; }
# Signature verification deliberately succeeds even for an empty framework,
# matching the observed installd failure that codesign --deep did not catch.
codesign() { return 0; }
_run_runner_prebuild() {
    printf 'build\\n' >> "$BUILD_CALLS"
    "$FIXTURE_PYTHON" "$FIXTURE_SCRIPT" --create "$APP" "$POISON"
}
''' + body + '\n')
        return subprocess.run(['/bin/bash', str(harness)], capture_output=True, text=True,
                              timeout=10, env={**{k: v for k, v in os.environ.items() if not k.startswith('WDA_ASC_')},
                                  'HELPERS': str(self.helpers), 'PRODUCTS': str(self.products),
                                  'APP': str(app or self.app), 'BUILD_CALLS': str(self.calls),
                                  'STATUS_LOG': str(self.status), 'LOG': str(self.root / 'build.log'),
                                  'FIXTURE_PYTHON': sys.executable, 'FIXTURE_SCRIPT': __file__,
                                  'POISON': '1' if poison else '0',
                                  'REPAIR_USED': '1' if repair_used else '0'})

    def test_valid_bundle_uses_plist_executable_name(self):
        result = self.run_shell('_validate_runner_bundle "$APP"')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_framework_is_caught_and_rebuilt_once(self):
        (self.app / 'Frameworks/Testing.framework/Info.plist').unlink()
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ['build'])
        self.assertIn('Testing.framework/Info.plist is missing', self.status.read_text())
        self.assertTrue((self.app / 'Frameworks/Testing.framework/Info.plist').is_file())

    def test_cstemp_causes_complete_product_rebuild(self):
        leftover = self.app / 'Frameworks/Testing.framework/Testing.cstemp'
        leftover.touch()
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(leftover.exists())
        self.assertIn('possible interrupted signing', self.status.read_text())
        self.assertEqual(self.calls.read_text().splitlines(), ['build'])

    def test_still_invalid_after_rebuild_fails_without_looping(self):
        (self.app / 'Frameworks/Testing.framework/Info.plist').unlink()
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"', poison=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls.read_text().splitlines(), ['build'])
        self.assertIn('building-fail', self.status.read_text())

    def test_repair_budget_is_shared_across_launch_stages(self):
        (self.app / 'PlugIns/RunnerTests.xctest/TestBinary').unlink()
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"', repair_used=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.calls.exists())
        self.assertTrue(self.app.exists())

    def test_missing_xctest_binary_is_rejected(self):
        (self.app / 'PlugIns/RunnerTests.xctest/TestBinary').unlink()
        result = self.run_shell('if _validate_runner_bundle "$APP"; then exit 99; fi\nprintf "%s" "$WDA_RUNNER_VALIDATION_ERROR"')
        self.assertEqual(result.returncode, 0)
        self.assertIn('TestBinary is missing', result.stdout)

    def test_unowned_app_is_never_removed(self):
        unowned = self.root / 'unrelated.app'
        unowned.mkdir()
        marker = unowned / 'keep'
        marker.write_text('user data')
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"', app=unowned)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(marker.read_text(), 'user data')
        self.assertFalse(self.calls.exists())

    def test_app_symlink_is_never_followed_for_removal(self):
        link = self.products / 'Other-Runner.app'
        link.symlink_to(self.app)
        result = self.run_shell('_repair_runner_if_invalid "$PRODUCTS" "$APP" "$LOG"', app=link)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.app / 'Info.plist').is_file())
        self.assertTrue(link.is_symlink())

    def test_launch_path_builds_validates_and_picks_the_xctestrun(self):
        (self.app / 'Frameworks/Testing.framework/Info.plist').unlink()
        (self.products.parent / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun').write_text('plist')
        body = '''STATE_DIR="$(dirname "$LOG")"
_ensure_launchable_runner
printf '%s\\n' "$WDA_XCTESTRUN" "$RUNNER_BUILT_PRODUCTS" "$RUNNER_APP_PATH"
'''
        result = self.run_shell(body)
        self.assertEqual(result.returncode, 0, result.stderr)
        # The (incremental) build always runs first; it restored the product.
        self.assertEqual(self.calls.read_text().splitlines(), ['build'])
        xctestrun, products, app = result.stdout.splitlines()[-3:]
        self.assertEqual(xctestrun, str(self.products.parent / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun'))
        self.assertEqual(products, str(self.products))
        self.assertEqual(app, str(self.app))

    def test_launch_path_repairs_a_poisoned_build_once(self):
        (self.products.parent / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun').write_text('plist')
        body = '''STATE_DIR="$(dirname "$LOG")"
_ensure_launchable_runner
'''
        result = self.run_shell(body, poison=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls.read_text().splitlines(), ['build', 'build'])
        self.assertIn('building-fail', self.status.read_text())

    def test_ambiguous_xctestrun_is_not_launchable(self):
        for sdk in ('26.5', '27.0'):
            (self.products.parent / f'IPhoneUseRunner_iphoneos{sdk}-arm64.xctestrun').write_text('plist')
        body = '''STATE_DIR="$(dirname "$LOG")"
if _ensure_launchable_runner; then exit 99; fi
printf '%s' "$WDA_RUNNER_VALIDATION_ERROR"
'''
        result = self.run_shell(body)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('unique .xctestrun', result.stdout)

    def test_only_this_instances_products_dir_may_be_repaired(self):
        other = self.root / 'other/Build/Products/Debug-iphoneos'
        other_app = other / 'iPhoneUse-Runner.app'
        create_runner(other_app, poison=True)
        result = self.run_shell('_repair_runner_if_invalid "' + str(other) + '" "$APP" "$LOG"', app=other_app)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(other_app.exists())
        self.assertFalse(self.calls.exists())


if __name__ == '__main__':
    if len(sys.argv) == 4 and sys.argv[1] == '--create':
        create_runner(Path(sys.argv[2]), sys.argv[3] == '1')
    else:
        unittest.main()
