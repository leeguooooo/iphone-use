#!/usr/bin/env python3
"""Create and delete one dedicated Simulator for Native list conformance; never select a phone."""
import argparse
import ast
import hashlib
import json
import os
import re
import signal
from pathlib import Path
import subprocess
import time
import urllib.request
import uuid


ROOT = Path(__file__).resolve().parents[1]
# Prebuilt CLI: boot240s + runner600s + daemon60s +120s setup/metadata.
# Do not terminate it before its own bounded phases can finish.
SIMULATOR_START_TIMEOUT = 1020
PROBES = {
    'list': ('test-collect-simulator.py', 4),
    'selector': ('test-selector-simulator.py', 16),
    'agent-selector': ('test-agent-selector-simulator.py', 7),
    'wait': ('test-wait-simulator.py', 11),
}


def probe_recipe(oracle_root, probe):
    source_root = oracle_root.resolve(strict=True)
    path = (source_root / 'scripts/run-collect-simulator.py').resolve(strict=True)
    assert path.is_relative_to(source_root), 'oracle manifest escapes source root'
    module = ast.parse(path.read_text())
    assignments = [node for node in module.body if
        (isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == 'PROBES' for target in node.targets))
        or (isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name) and node.target.id == 'PROBES')]
    assert len(assignments) == 1 and isinstance(assignments[0], ast.Assign), 'one literal PROBES assignment required'
    definitions = ast.literal_eval(assignments[0].value)
    script, count = definitions[probe]
    assert isinstance(script, str) and Path(script).name == script and script.endswith('.py'), 'invalid oracle script'
    assert type(count) is int and 1 <= count <= 256, 'invalid oracle case count'
    test = (oracle_root / 'scripts' / script).resolve(strict=True)
    assert test.parent == (oracle_root / 'scripts').resolve(strict=True) and test.is_relative_to(source_root), 'oracle script escapes source root'
    return test, count


class CommandTerminationUnverified(RuntimeError):
    pass


def command(argv, *, env=None, timeout=120):
    result = subprocess.run([str(x) for x in argv], env=env, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{argv[0]} exit {result.returncode}: {result.stdout}{result.stderr}')
    return result.stdout.strip()


def logged(argv, log, env, timeout):
    with log.open('w') as stream:
        process = subprocess.Popen([str(x) for x in argv], env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                # The UIKit preparer forwards TERM to its active command group;
                # allow its bounded cleanup to finish before killing the parent.
                try: os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError: pass
                try: process.wait(timeout=15)
                except subprocess.TimeoutExpired: pass
                try: os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError: pass
                process.wait(timeout=5)
            except Exception as error:
                raise CommandTerminationUnverified(f'owned pid {process.pid} termination unverified; inspect {log.name}; do not replay') from error
            raise RuntimeError(f'owned command timed out; inspect {log.name}; no mutation replay')
    if code:
        raise RuntimeError(f'owned command exit {code}; inspect {log.name}')


def cleanup_owned_simulator(simulator, name, env, out):
    """Dispatch SDK mutations once; settle unknown outcomes by readback."""
    receipt = {'passed': False, 'shutdown_attempts': 0, 'delete_attempts': 0,
               'observations': [], 'commands': [], 'final_state': None}

    def inspect():
        devices = json.loads(command(['xcrun', 'simctl', 'list', 'devices', '--json'], env=env, timeout=30))['devices']
        owned = [device for group in devices.values() for device in group if device['udid'] == simulator]
        if not owned:
            state = None
        elif len(owned) != 1 or owned[0]['name'] != name:
            raise RuntimeError('cleanup no longer matches created Simulator; no further mutation sent')
        else:
            state = owned[0]['state']
            if not isinstance(state, str) or not state.strip():
                raise RuntimeError('owned Simulator has no readable state; cleanup outcome unverified')
        receipt['observations'].append(state if state is not None else 'absent')
        return state

    def normalized(state):
        return state.replace(' ', '').lower() if isinstance(state, str) else None

    def dispatch(action, timeout):
        receipt[action + '_attempts'] += 1
        entry = {'action': action, 'state': 'sent_once'}
        receipt['commands'].append(entry)
        try:
            logged(['xcrun', 'simctl', action, simulator], out / f'simulator-{action}.log', env, timeout)
            entry['state'] = 'client_completed'
        except CommandTerminationUnverified as error:
            entry.update(state='client_termination_unverified', error=str(error))
            raise
        except Exception as error:
            # The SDK may keep working after its client exits. Never repeat
            # the mutation, including when the client returned a known error.
            entry.update(state='client_failed_outcome_unknown', error=str(error))

    def observe(goal, deadline):
        while True:
            state = inspect()
            if state is None or (goal == 'shutdown' and normalized(state) == 'shutdown'):
                return state
            if time.monotonic() >= deadline:
                raise RuntimeError(f'Simulator {goal} outcome unverified (last state {state}); no mutation replay')
            time.sleep(1)

    try:
        state = inspect()
        if state is not None and normalized(state) != 'shutdown':
            deadline = time.monotonic() + 240
            if normalized(state) in ('booted', 'booting'):
                dispatch('shutdown', 240)
            elif normalized(state) != 'shuttingdown':
                raise RuntimeError(f'cannot safely shut down Simulator in state {state}')
            state = observe('shutdown', max(deadline, time.monotonic() + 30))
        if state is not None:
            deadline = time.monotonic() + 120
            dispatch('delete', 120)
            observe('absent', max(deadline, time.monotonic() + 30))
        receipt.update(passed=True, final_state='absent')
        return receipt
    except Exception as error:
        receipt['error'] = str(error)
        raise
    finally:
        (out / 'simulator-cleanup.json').write_text(json.dumps(receipt, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon', 'mcp', 'runner-source', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--runtime', required=True)
    parser.add_argument('--probe', choices=tuple(PROBES), default='list')
    parser.add_argument('--xctestrun', type=Path, required=True)
    parser.add_argument('--oracle-root', type=Path)
    parser.add_argument('--oracle-commit')
    parser.add_argument('--prepare-ui', action='store_true')
    args = parser.parse_args()
    assert not args.prepare_ui or (args.oracle_root and args.oracle_commit), 'warm preparation requires explicit oracle source and commit'
    oracle_root = (args.oracle_root or ROOT).resolve(strict=True)
    oracle_commit = None
    if args.oracle_commit:
        assert re.fullmatch(r'[0-9a-f]{40}', args.oracle_commit), 'expected full oracle commit'
        oracle_commit = command(['git', '-C', oracle_root, 'rev-parse', 'HEAD'])
        assert oracle_commit == args.oracle_commit, 'oracle checkout differs from requested commit'
        command(['git', '-C', oracle_root, 'diff', '--quiet', 'HEAD'])
    test_script, expected_cases = probe_recipe(oracle_root, args.probe)
    daemon, mcp, runner = (path.resolve(strict=True) for path in (args.daemon, args.mcp, args.runner_source))
    assert (runner / 'IPhoneUseRunner/IPhoneUseRunner.xcodeproj').is_dir()
    args.out = args.out.resolve(); args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('IPHONE_USE_', 'PHONE_REMOTE_', 'IPU_', 'TEST_RUNNER_')) and k not in ('GH_TOKEN','GITHUB_TOKEN')}
    env.pop('SDKROOT', None)
    env.update(IPU_RUNNER_SRC=str(runner), IPU_RUNNER_DERIVED_DATA=str(args.out / 'runner-build'),
               IPHONE_USE_MCP_PREWARM='0', IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1')
    runtimes = json.loads(command(['xcrun', 'simctl', 'list', 'runtimes', '--json'], env=env))['runtimes']
    assert any(r['identifier'] == args.runtime and r.get('isAvailable') is True for r in runtimes), 'requested runtime unavailable'
    nonce = str(uuid.uuid4()); name = 'ipu-' + args.probe + '-oracle-' + nonce[:8]; instance = args.probe + '-oracle-' + nonce[:8]
    state = Path.home() / '.iphone-use/instances' / instance
    assert not state.exists(), 'disposable instance already exists'
    simulator = None
    result = {'passed': False, 'scope': 'new owned Simulator only', 'run_id': nonce, 'runtime': args.runtime,
              'instance': instance, 'daemon_sha256': hashlib.sha256(daemon.read_bytes()).hexdigest(),
              'mcp_sha256': hashlib.sha256(mcp.read_bytes()).hexdigest(),
              'version': command([daemon, '--version'], env=env),
              'xcode': command(['xcodebuild', '-version'], env=env), 'cleanup_errors': [],
              'startup_command_budget_secs': SIMULATOR_START_TIMEOUT}
    match = re.fullmatch(r'iphone-use (\d+\.\d+\.\d+)', result['version'])
    assert match, 'unknown product version'
    result['oracle_source'] = {'root': str(oracle_root), 'script': str(test_script.relative_to(oracle_root)),
        'script_sha256': hashlib.sha256(test_script.read_bytes()).hexdigest(), 'expected_cases': expected_cases,
        'manifest_sha256': hashlib.sha256((oracle_root / 'scripts/run-collect-simulator.py').read_bytes()).hexdigest(), 'commit': oracle_commit}
    result['runner_sources'] = {str(path.relative_to(runner)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in runner.rglob('*') if path.is_file() and path.suffix in ('.swift', '.m', '.h')}
    try:
        simulator = command(['xcrun', 'simctl', 'create', name, 'com.apple.CoreSimulator.SimDeviceType.iPhone-15', args.runtime], env=env)
        uuid.UUID(simulator); result['simulator'] = simulator
        devices = json.loads(command(['xcrun', 'simctl', 'list', 'devices', '--json'], env=env))['devices']
        assert any(d['udid'] == simulator and d['name'] == name for d in devices.get(args.runtime, [])), 'created Simulator identity differs'
        if args.prepare_ui:
            platform = os.environ.get('IPU_DIAGNOSTIC_PLATFORM')
            assert platform in ('xcode16.2-ios18.2', 'xcode16.4-ios18.5'), 'explicit UI preparation platform required'
            warm_env = dict(env, IPU_DIAGNOSTIC_PLATFORM=platform)
            result['startup_context'] = 'ordinary UIKit preparation requested; cold startup is not evaluated'
            result['ui_preparation_artifact'] = str(args.out / 'platform-warmup/result.json')
            logged(['python3', ROOT / 'scripts/diagnose-simulator-platform.py', '--prepare-ui-only',
                    '--owned-simulator', simulator, '--owned-name', name, '--oracle-root', oracle_root,
                    '--out', args.out / 'platform-warmup'], args.out / 'platform-warmup.log', warm_env, 600)
            warm = json.loads((args.out / 'platform-warmup/result.json').read_text())
            assert warm['mode'] == 'ordinary-ui-only' and warm['passed'] is True and not warm['cleanup_errors']
            assert warm['simulator'].lower() == simulator.lower() and warm['simulator_name'] == name
            assert warm['platform'] == platform and warm['runtime'] == args.runtime
            assert warm['caller_owned_target']['udid'].lower() == simulator.lower() and warm['caller_owned_target']['name'] == name
            assert warm['ordinary_app']['ready'] is True and warm['ordinary_app']['bundle_isolated'] is True
            result['ui_preparation'] = warm
            result['startup_context'] = 'warm, after independently confirmed ordinary UIKit window; not cold-start acceptance'
        start = [daemon, 'simulator', 'start', simulator, '--instance', instance]
        start += ['--xctestrun', args.xctestrun.resolve(strict=True)]
        logged(start, args.out / 'simulator-start.log', env, SIMULATOR_START_TIMEOUT)
        record = state / 'simulator.json'
        binding = json.loads(record.read_text())
        assert binding['udid'].lower() == simulator.lower()
        assert type(binding['daemon_port']) is int and 1 <= binding['daemon_port'] <= 65535
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        until = time.monotonic() + 180
        while True:
            with opener.open(f"http://127.0.0.1:{binding['daemon_port']}/agent/status", timeout=10) as response:
                status = json.load(response)
            assert status.get('target') == 'simulator' and str(status.get('udid', '')).lower() == simulator.lower(), 'daemon target differs; no collection sent'
            assert status.get('owner') in (None, instance), 'another owner; no collection sent'
            result['readiness'] = {key: status.get(key) for key in ('target', 'udid', 'drivable', 'released', 'human_handoff', 'reconnecting')}
            if status.get('drivable') is True:
                break
            assert time.monotonic() < until, 'Simulator Agent API did not become drivable'
            time.sleep(2)
        groups = [('conformance', [], 'cases')] if args.probe != 'list' else [
            ('conformance', [], 'cases'), ('default-conformance', ['--without-end-label'], 'default_cases')]
        for folder, flags, field in groups:
            logged(['python3', test_script, '--base', f"http://127.0.0.1:{binding['daemon_port']}",
                    '--udid', simulator, '--owner', instance, '--mcp', mcp, '--runner-record', record,
                    '--product-version', match[1], '--out', args.out / folder, *flags],
                   args.out / (folder + '.log'), env, 720)
            proof = json.loads((args.out / folder / 'result.json').read_text())
            assert proof['passed'] is True and not proof['cleanup_errors'] and len(proof['cases']) == expected_cases, f'{args.probe}: expected {expected_cases} passing cases and clean teardown'
            result[field] = proof['cases']
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        if simulator:
            owned_record = False
            try:
                record = state / 'simulator.json'
                if record.exists():
                    owned_record = json.loads(record.read_text())['udid'].lower() == simulator.lower()
                    assert owned_record, 'cleanup record belongs elsewhere'
                    # The wrapper owns the single SDK shutdown below. The CLI
                    # shutdown budget is 240s; killing it at 120s and sending
                    # shutdown again leaves an unknown operation in flight.
                    logged([daemon, 'simulator', 'stop', simulator, '--instance', instance], args.out / 'simulator-stop.log', env, 120)
            except Exception as error:
                result['cleanup_errors'].append(str(error))
            try:
                result['simulator_cleanup'] = cleanup_owned_simulator(simulator, name, env, args.out)
            except Exception as error:
                result['cleanup_errors'].append(str(error))
            for name in ('simulator-runner.log', 'simulator-daemon.log', 'runner-build-simulator.log'):
                path = state / name
                if path.is_file():
                    (args.out / name).write_bytes(path.read_bytes())
            # Do not archive the per-instance credential. Retain all non-secret diagnostics.
            token = state / 'runner-token'
            if owned_record and token.is_file() and not state.is_symlink():
                token.unlink()
        if result['cleanup_errors']:
            result['passed'] = False
        (args.out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result), flush=True)
        if result['cleanup_errors']:
            raise RuntimeError('; '.join(result['cleanup_errors']))


if __name__ == '__main__':
    main()
