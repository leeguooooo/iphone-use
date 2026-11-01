#!/usr/bin/env python3
"""Version-specific oracle recipes must never fall back to moving main."""
import importlib.util
import re
from pathlib import Path
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location('collector', HERE / 'run-collect-simulator.py')
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class Recipe(unittest.TestCase):
    def test_outer_start_budget_covers_the_prebuilt_clis_bounded_phases(self):
        source=(HERE.parent/'crates/server/src/simulator.rs').read_text()
        boot=re.search(r'const BOOT_TIMEOUT: Duration = Duration::from_secs\((\d+)\)',source)
        self.assertIsNotNone(boot,'inspect the CLI boot budget before changing the outer deadline')
        start=source.split('fn start(',1)[1].split('\npub fn',1)[0]
        readiness=[int(value) for value in re.findall(r'started\.elapsed\(\)\s*>\s*Duration::from_secs\((\d+)\)',start)]
        self.assertEqual(len(readiness),2,'inspect changed runner/daemon readiness phases')
        self.assertGreaterEqual(collector.SIMULATOR_START_TIMEOUT,int(boot[1])+sum(readiness)+120)

    def test_long_probe_keeps_instance_inside_product_name_limit(self):
        nonce = '01234567-89ab-cdef-0123-456789abcdef'
        for probe in ['wait', 'tap-guard', 'tap-identity-race', 'x' * 100]:
            instance = collector.oracle_instance(probe, nonce)
            self.assertRegex(instance, r'^[a-z][a-z0-9-]{0,31}$')
            self.assertTrue(instance.endswith('-oracle-01234567'))
        self.assertEqual(collector.oracle_instance('wait', nonce), 'wait-oracle-01234567')

    def fixture(self, root, recipe):
        scripts = root / 'scripts'
        scripts.mkdir()
        (scripts / 'oracle.py').write_text('raise AssertionError("must not execute")\n')
        (scripts / 'run-collect-simulator.py').write_text(
            'PROBES = ' + repr({'selector': recipe}) + '\nraise AssertionError("must not execute")\n')

    def test_selected_version_defines_its_script_and_count_without_execution(self):
        for count in [11, 16]:
            with self.subTest(count=count), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                self.fixture(root, ('oracle.py', count))
                self.assertEqual(collector.probe_recipe(root, 'selector'), ((root / 'scripts/oracle.py').resolve(), count))

    def test_invalid_recipes_fail_instead_of_using_the_current_default(self):
        for recipe in [('oracle.py', True), ('oracle.py', 0), ('oracle.py', 257), ('../oracle.py', 11), ('missing.py', 11)]:
            with self.subTest(recipe=recipe), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                self.fixture(root, recipe)
                with self.assertRaises((AssertionError, FileNotFoundError)):
                    collector.probe_recipe(root, 'selector')
                with self.assertRaises(KeyError):
                    collector.probe_recipe(root, 'unknown')

    def test_duplicate_or_annotated_manifests_are_not_selected_arbitrarily(self):
        for suffix in ["PROBES = {'selector': ('oracle.py', 16)}", "PROBES: dict = {'selector': ('oracle.py', 16)}"]:
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as folder:
                root = Path(folder)
                self.fixture(root, ('oracle.py', 11))
                manifest = root / 'scripts/run-collect-simulator.py'
                manifest.write_text(manifest.read_text() + suffix + '\n')
                with self.assertRaises(AssertionError):
                    collector.probe_recipe(root, 'selector')

    def test_a_script_symlink_cannot_escape_the_selected_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder) / 'source'
            root.mkdir()
            self.fixture(root, ('oracle.py', 11))
            outside = Path(folder) / 'outside.py'
            outside.write_text('raise AssertionError("must not execute")\n')
            (root / 'scripts/oracle.py').unlink()
            (root / 'scripts/oracle.py').symlink_to(outside)
            with self.assertRaises(AssertionError):
                collector.probe_recipe(root, 'selector')

    def test_a_manifest_symlink_cannot_escape_the_selected_source(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder) / 'source'
            root.mkdir()
            self.fixture(root, ('oracle.py', 11))
            outside = Path(folder) / 'manifest.py'
            outside.write_text('raise AssertionError("must not read or execute")\n')
            manifest = root / 'scripts/run-collect-simulator.py'
            manifest.unlink()
            manifest.symlink_to(outside)
            with self.assertRaises(AssertionError):
                collector.probe_recipe(root, 'selector')


if __name__ == '__main__':
    unittest.main()
