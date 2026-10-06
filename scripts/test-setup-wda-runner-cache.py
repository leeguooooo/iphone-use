#!/usr/bin/env python3
"""Exercise the runner product cache helpers from setup-wda.sh in a throwaway
state dir — no Xcode, no device. The bundle validator is stubbed."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SOURCE = (Path(__file__).parent / 'setup-wda.sh').read_text()
HELPERS = SOURCE.split('# BEGIN runner product cache.', 1)[1].split('# END runner product cache.', 1)[0]


class RunnerCacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='wda-runner-cache-test-')
        self.root = Path(self.temp.name)
        self.state = self.root / 'state'
        self.state.mkdir()
        self.products = self.state / 'runner-build' / 'Build' / 'Products' / 'Debug-iphoneos'
        self.products.mkdir(parents=True)
        (self.products / 'iPhoneUse-Runner.app').mkdir()
        # xcodebuild writes the .xctestrun beside the configuration directory.
        self.xctestrun = self.products.parent / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun'
        self.xctestrun.write_text('plist')

    def tearDown(self):
        self.temp.cleanup()

    def run_bash(self, body, *, validate_ok=True, asc=False):
        script = self.root / 'run.sh'
        script.write_text(
            'set -u\n'
            f'STATE_DIR="{self.state}"\n'
            f'RUNNER_PRODUCTS_DIR="{self.products}"\n'
            'RUNNER_APP_NAME=iPhoneUse-Runner.app\n'
            'RUNNER_SOURCE_HASH=1111aaaa\nWDA_BUNDLE_ID=com.example.wda\nTEAM_ID=TEAM000000\n'
            'WDA_UDID=00008150-0000000000000000\nXCODE_VERSION="Xcode 27.0"\nWDA_IOS_SDK_VERSION=27.0\n'
            'WDA_ASC_KEY_ID=KEY0000001\n'
            + ('_asc_signing_enabled() { return 0; }\n' if asc else '_asc_signing_enabled() { return 1; }\n')
            + 'ok() { printf "ok: %s\\n" "$*"; }\nwarn() { printf "warn: %s\\n" "$*"; }\n'
            + ('_validate_runner_bundle() { return 0; }\n' if validate_ok else '_validate_runner_bundle() { return 1; }\n')
            + HELPERS + '\n' + body + '\n'
        )
        proc = subprocess.run(['bash', str(script)], capture_output=True, text=True, env=dict(os.environ))
        return proc.returncode, proc.stdout.strip().splitlines()

    def write_body(self, xctestrun=None, products=None):
        return (f'RUNNER_BUILT_PRODUCTS="{products or self.products}"\n'
                f'WDA_XCTESTRUN="{xctestrun or self.xctestrun}"\n'
                '_runner_cache_write || exit 9\n')

    def read_body(self):
        return ('RUNNER_BUILT_PRODUCTS=""\nWDA_XCTESTRUN=""\nWDA_RUNNER_FROM_CACHE=0\n'
                'if _runner_cache_read; then echo "hit $WDA_RUNNER_FROM_CACHE $RUNNER_BUILT_PRODUCTS $WDA_XCTESTRUN $RUNNER_APP_PATH"; else echo miss; fi\n')

    def test_write_then_read_reuses_the_product(self):
        code, out = self.run_bash(self.write_body() + self.read_body())
        self.assertEqual(code, 0, out)
        record = json.loads((self.state / 'wda-runner-product.json').read_text())
        self.assertEqual(record['schema_version'], 1)
        self.assertTrue(record['key'].startswith('v3|1111aaaa|'), record['key'])
        self.assertEqual(record['xctestrun'], str(self.xctestrun))
        self.assertEqual(out[-1], f'hit 1 {self.products} {self.xctestrun} {self.products}/iPhoneUse-Runner.app')

    def test_every_key_part_invalidates_the_record(self):
        for change in ('RUNNER_SOURCE_HASH=2222bbbb', 'WDA_BUNDLE_ID=com.example.other',
                       'TEAM_ID=TEAM000001', 'WDA_UDID=00008150-0000000000000001',
                       'XCODE_VERSION="Xcode 27.1"', 'WDA_IOS_SDK_VERSION=27.1',
                       'WDA_DEPLOYMENT_TARGET_OVERRIDE=16.0',
                       '_asc_signing_enabled() { return 0; }'):
            with self.subTest(change=change):
                code, out = self.run_bash(self.write_body() + change + '\n' + self.read_body())
                self.assertEqual(out[-1], 'miss', out)

    def test_signing_with_another_api_key_misses(self):
        code, out = self.run_bash(self.write_body() + 'WDA_ASC_KEY_ID=KEY0000002\n' + self.read_body(), asc=True)
        self.assertEqual(out[-1], 'miss', out)
        code, out = self.run_bash(self.write_body() + self.read_body(), asc=True)
        self.assertTrue(out[-1].startswith('hit 1 '), out)

    def test_unknown_source_hash_never_reads_or_writes(self):
        code, out = self.run_bash(self.write_body() + 'RUNNER_SOURCE_HASH=""\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        code, out = self.run_bash('RUNNER_SOURCE_HASH=""\n' + self.write_body())
        self.assertEqual(code, 9, out)

    def test_missing_files_or_invalid_bundle_miss(self):
        code, out = self.run_bash(self.write_body() + f'rm "{self.xctestrun}"\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        code, out = self.run_bash(self.write_body() + self.read_body(), validate_ok=False)
        self.assertEqual(out[-1], 'miss', out)
        code, out = self.run_bash(self.write_body() + f'rm -r "{self.products}/iPhoneUse-Runner.app"\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)

    def test_write_requires_both_paths_and_refuses_symlinks(self):
        code, out = self.run_bash('RUNNER_BUILT_PRODUCTS=""\nWDA_XCTESTRUN=""\n_runner_cache_write; echo "rc=$?"\n')
        self.assertEqual(out[-1], 'rc=1', out)
        target = self.root / 'elsewhere.json'
        (self.state / 'wda-runner-product.json').symlink_to(target)
        code, out = self.run_bash(self.write_body() + 'echo written\n')
        self.assertEqual(code, 9, out)
        self.assertFalse(target.exists())
        code, out = self.run_bash('_runner_cache_drop; echo "rc=$?"\n')
        self.assertEqual(out[-1], 'rc=1', out)
        self.assertTrue((self.state / 'wda-runner-product.json').is_symlink())

    def test_drop_removes_the_record(self):
        code, out = self.run_bash(self.write_body() + '_runner_cache_drop\n' + self.read_body())
        self.assertEqual(out[-1], 'miss', out)
        self.assertFalse((self.state / 'wda-runner-product.json').exists())

    def test_only_this_instances_products_are_reused(self):
        other = self.root / 'other-instance' / 'runner-build' / 'Build' / 'Products' / 'Debug-iphoneos'
        (other / 'iPhoneUse-Runner.app').mkdir(parents=True)
        other_run = other.parent / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun'
        other_run.write_text('plist')
        code, out = self.run_bash(self.write_body(other_run, other) + self.read_body())
        self.assertEqual(out[-1], 'miss', out)

    def test_xctestrun_must_sit_beside_the_configuration_dir(self):
        for stray in (self.products / 'IPhoneUseRunner_iphoneos27.0-arm64.xctestrun',
                      self.root / 'stray.xctestrun'):
            with self.subTest(stray=stray.name):
                stray.write_text('plist')
                code, out = self.run_bash(self.write_body(stray) + self.read_body())
                self.assertEqual(out[-1], 'miss', out)

    def test_record_from_the_webdriveragent_era_is_ignored(self):
        bogus = self.state / 'wda-runner-product.json'
        bogus.write_text(json.dumps({
            'schema_version': 1,
            'key': 'v2|54f9fc70|com.example.wda|TEAM000000|00008150-0000000000000000|||Xcode 27.0|27.0|',
            'products_dir': str(self.products), 'xctestrun': str(self.xctestrun)}))
        code, out = self.run_bash(self.read_body())
        self.assertEqual(out[-1], 'miss', out)


if __name__ == '__main__':
    unittest.main()
