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
            stop(process)
            raise RuntimeError(f'owned command timed out; inspect {log.name}; no mutation replay')
    if code:
        raise RuntimeError(f'owned command exit {code}; inspect {log.name}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('daemon', 'mcp', 'runner-source', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--runtime', required=True)
    parser.add_argument('--xctestrun', type=Path)
    args = parser.parse_args()
    daemon, mcp, runner = (path.resolve(strict=True) for path in (args.daemon, args.mcp, args.runner_source))
    assert (runner / 'IPhoneUseRunner/IPhoneUseRunner.xcodeproj').is_dir()
    args.out = args.out.resolve(); args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('IPHONE_USE_', 'PHONE_REMOTE_', 'IPU_', 'TEST_RUNNER_'))}
    env.pop('SDKROOT', None)
    env.update(IPU_RUNNER_SRC=str(runner), IPU_RUNNER_DERIVED_DATA=str(args.out / 'runner-build'),
               IPHONE_USE_MCP_PREWARM='0', IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1')
    runtimes = json.loads(command(['xcrun', 'simctl', 'list', 'runtimes', '--json'], env=env))['runtimes']
    assert any(r['identifier'] == args.runtime and r.get('isAvailable') is True for r in runtimes), 'requested runtime unavailable'
    nonce = str(uuid.uuid4()); name = 'ipu-list-oracle-' + nonce[:8]; instance = 'list-oracle-' + nonce[:8]
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
        if args.xctestrun:
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
        logged(['python3', ROOT / 'scripts/test-collect-simulator.py', '--base', f"http://127.0.0.1:{binding['daemon_port']}",
                '--udid', simulator, '--owner', instance, '--mcp', mcp, '--runner-record', record,
                '--product-version', match[1],
                '--out', args.out / 'conformance'], args.out / 'conformance.log', env, 720)
        proof = json.loads((args.out / 'conformance/result.json').read_text())
        assert proof['passed'] is True and not proof['cleanup_errors'] and len(proof['cases']) == 4
        result.update(passed=True, cases=proof['cases'])
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
                    logged([daemon, 'simulator', 'stop', simulator, '--instance', instance, '--shutdown'], args.out / 'simulator-stop.log', env, 120)
            except Exception as error:
                result['cleanup_errors'].append(str(error))
            try:
                devices = json.loads(command(['xcrun', 'simctl', 'list', 'devices', '--json'], env=env))['devices']
                owned = [d for group in devices.values() for d in group if d['udid'] == simulator and d['name'] == name]
                assert len(owned) == 1, 'cleanup no longer matches created Simulator'
                if owned[0]['state'] != 'Shutdown':
                    command(['xcrun', 'simctl', 'shutdown', simulator], env=env)
                command(['xcrun', 'simctl', 'delete', simulator], env=env)
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
