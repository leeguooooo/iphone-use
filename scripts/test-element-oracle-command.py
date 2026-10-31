#!/usr/bin/env python3
"""Execute the real workflow command with fake Python, including macOS Bash 3.2."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]


def workflow_command(kind='published'):
    filename = 'candidate-native-oracle.yml' if kind == 'candidate' else 'simulator-smoke.yml'
    step_name = 'Run exact-source App oracle on a new owned Simulator' if kind == 'candidate' else 'Run the version-pinned Native oracle in the selected startup context'
    lines = (ROOT / '.github/workflows' / filename).read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == '- name: ' + step_name)
    start = next(i for i in range(start, len(lines)) if lines[i].strip() == 'run: |') + 1
    end = next(i for i in range(start, len(lines)) if lines[i].startswith('      - name:'))
    return textwrap.dedent('\n'.join(lines[start:end])) + '\n'


class Command(unittest.TestCase):
    def execute(self, warm, kind='published'):
        with tempfile.TemporaryDirectory() as folder:
            out = Path(folder)
            binary = out / 'bin'
            binary.mkdir()
            capture = out / 'arguments.json'
            (out / 'list-oracle-source.json').write_text('{"oracle_commit":"' + 'a' * 40 + '"}')
            shim = binary / 'python3'
            shim.write_text('#!' + sys.executable + '\n' + textwrap.dedent('''\
                import json, os, sys
                from pathlib import Path
                if sys.argv[1] == 'scripts/run-collect-simulator.py':
                    Path(os.environ['IPU_TEST_CAPTURE']).write_text(json.dumps(sys.argv[1:]))
                elif sys.argv[1] == '-c' and 'list-oracle-source.json' in sys.argv[2]:
                    print('a' * 40)
                elif sys.argv[1] == '-c' and any(name in sys.argv[2] for name in ['list-runner/Build/Products','candidate-runner/Build/Products']):
                    print(str(Path(os.environ['RUNNER_TEMP']) / 'fixture.xctestrun'))
                else:
                    raise SystemExit('Unexpected Python invocation; refusing to run real infrastructure')
            '''))
            shim.chmod(0o700)
            script = out / 'command.sh'
            script.write_text(workflow_command(kind))
            env = dict(os.environ, PATH=str(binary) + os.pathsep + os.environ.get('PATH', ''),
                       RUNNER_TEMP=str(out), GITHUB_WORKSPACE='/fixture-workspace',
                       IPU_TEST_CAPTURE=str(capture), PREPARE_ELEMENT_UI='true' if warm else 'false',
                       ELEMENT_PROBE='selector', ELEMENT_RUNTIME='com.apple.CoreSimulator.SimRuntime.iOS-18-5',
                       ORACLE_PROBE='selector', ORACLE_RUNTIME='com.apple.CoreSimulator.SimRuntime.iOS-18-5',
                       PREPARE_UI='true' if warm else 'false', GITHUB_SHA='a' * 40)
            process = subprocess.run(['/bin/bash', str(script)], env=env, cwd=ROOT,
                                     capture_output=True, text=True, timeout=10)
            self.assertEqual(process.returncode, 0, process.stderr)
            args = json.loads(capture.read_text())
            self.assertEqual(args.count('--prepare-ui'), int(warm))
            self.assertEqual(args[args.index('--oracle-commit') + 1], 'a' * 40)
            self.assertEqual(args[args.index('--oracle-root') + 1], '/fixture-workspace' + ('/.release-oracle' if kind == 'published' else ''))
            self.assertEqual(args[args.index('--runtime') + 1], env['ELEMENT_RUNTIME'])

    def test_cold_start_reaches_the_collector_without_preparation(self):
        self.execute(False)

    def test_warm_start_passes_one_explicit_preparation_flag(self):
        self.execute(True)

    def test_candidate_cold_command_reaches_the_collector(self):
        self.execute(False, 'candidate')

    def test_candidate_warm_command_passes_explicit_preparation(self):
        self.execute(True, 'candidate')


if __name__ == '__main__':
    unittest.main()
