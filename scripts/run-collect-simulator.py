#!/usr/bin/env python3
"""Create and delete one dedicated Simulator for Native list conformance; never select a phone."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import subprocess
import time
import urllib.request
import uuid

from mcp_probe_peer import stop

ROOT = Path(__file__).resolve().parents[1]
PROBES = {
    'list': ('test-collect-simulator.py', 4),
    'selector': ('test-selector-simulator.py', 11),
    'agent-selector': ('test-agent-selector-simulator.py', 7),
}


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
                stop(process)
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
    args = parser.parse_args()
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
              'xcode': command(['xcodebuild', '-version'], env=env), 'cleanup_errors': []}
    match = re.fullmatch(r'iphone-use (\d+\.\d+\.\d+)', result['version'])
    assert match, 'unknown product version'
    result['runner_sources'] = {str(path.relative_to(runner)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in runner.rglob('*') if path.is_file() and path.suffix in ('.swift', '.m', '.h')}
    try:
        simulator = command(['xcrun', 'simctl', 'create', name, 'com.apple.CoreSimulator.SimDeviceType.iPhone-15', args.runtime], env=env)
        uuid.UUID(simulator); result['simulator'] = simulator
        devices = json.loads(command(['xcrun', 'simctl', 'list', 'devices', '--json'], env=env))['devices']
        assert any(d['udid'] == simulator and d['name'] == name for d in devices.get(args.runtime, [])), 'created Simulator identity differs'
        start = [daemon, 'simulator', 'start', simulator, '--instance', instance]
        start += ['--xctestrun', args.xctestrun.resolve(strict=True)]
        logged(start, args.out / 'simulator-start.log', env, 360)
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
        test_script, expected_cases = PROBES[args.probe]
        for folder, flags, field in groups:
            logged(['python3', ROOT / 'scripts' / test_script, '--base', f"http://127.0.0.1:{binding['daemon_port']}",
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
