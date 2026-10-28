#!/usr/bin/env python3
"""Verify one native system-capture request against a new Photos asset on an owned Simulator."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import signal
import secrets
import urllib.request
import sqlite3
import subprocess
import time
import uuid

from mcp_probe_peer import Peer, payload, port, stop
from runner_auth import authorization

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = 'com.leeguoo.iphone-use.capture-fixture'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--xctestrun', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--runtime', required=True)
    parser.add_argument('--daemon', type=Path)
    parser.add_argument('--mcp', type=Path)
    args = parser.parse_args()
    if bool(args.daemon) != bool(args.mcp): parser.error('--daemon and --mcp must be supplied together')
    daemon_binary = args.daemon.resolve(strict=True) if args.daemon else None
    mcp_binary = args.mcp.resolve(strict=True) if args.mcp else None
    recipe = args.xctestrun.resolve(strict=True)
    out = args.out.resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    run_id = str(uuid.uuid4()).upper()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('IPU_', 'TEST_RUNNER_IPU_', 'IPHONE_USE_', 'PHONE_REMOTE_', 'SIMCTL_CHILD_'))}
    env['TEST_RUNNER_IPU_CAPTURE_PROBE_RUN_ID'] = run_id
    result = {'run_id': run_id, 'scope': 'owned Simulator only; native request plus independent Photos/128-cell proof; not physical evidence',
              'passed': False, 'capture_verified': False, 'screenshot_category_verified': False, 'cleanup_errors': [], 'recipe_sha256': hashlib.sha256(recipe.read_bytes()).hexdigest()}
    simulator = process = fixture_documents = host_documents = host_bundle = media = baseline = None
    daemon_process = peer = None
    probe_token = secrets.token_hex(32)
    result['capture_path'] = 'mcp-daemon-native' if mcp_binary else 'native-direct-probe'
    if mcp_binary:
        result['daemon_binary_sha256'] = hashlib.sha256(daemon_binary.read_bytes()).hexdigest()
        result['mcp_binary_sha256'] = hashlib.sha256(mcp_binary.read_bytes()).hexdigest()

    def command(*argv, timeout=45):
        completed = subprocess.run([str(v) for v in argv], env=env, capture_output=True, text=True, timeout=timeout)
        if completed.returncode:
            raise RuntimeError(f'{argv[0]} exited {completed.returncode}: {completed.stderr[-1500:]}')
        return completed.stdout.strip()

    def receipt(path, name):
        data = (path / name).read_bytes()
        value = json.loads(data)
        assert value.get('run_id', value.get('probe_run_id')) == run_id, 'stale diagnostic receipt'
        (out / name).write_bytes(data)
        return value

    def stop_child():
        if process is not None and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)

    try:
        runtimes = json.loads(command('xcrun', 'simctl', 'list', 'runtimes', '--json'))['runtimes']
        assert any(r['identifier'] == args.runtime and r.get('isAvailable') is True for r in runtimes), 'requested runtime unavailable'
        assert args.runtime.startswith('com.apple.CoreSimulator.SimRuntime.iOS-'), 'requires an iOS Simulator runtime'
        config = plistlib.loads(recipe.read_bytes())
        targets = [v for v in config.values() if isinstance(v, dict) and 'TestHostPath' in v]
        if not targets:
            targets = [v for c in config.get('TestConfigurations', []) for v in c.get('TestTargets', []) if 'TestHostPath' in v]
        assert len(targets) == 1, 'ambiguous XCTest host'
        host = Path(targets[0]['TestHostPath'].replace('__TESTROOT__', str(recipe.parent))).resolve(strict=True)
        host.relative_to(recipe.parent)
        host_bundle = plistlib.loads((host / 'Info.plist').read_bytes())['CFBundleIdentifier']
        assert host_bundle == 'com.leeguoo.iphone-use.runner.xctrunner', 'unexpected XCTest host'
        result['runner_binary_sha256'] = hashlib.sha256((host / 'PlugIns/iPhoneUse.xctest/iPhoneUse').read_bytes()).hexdigest()
        source = ROOT / 'scripts/fixtures/system-capture/CaptureFixture.swift'
        result['fixture_source_sha256'] = hashlib.sha256(source.read_bytes()).hexdigest()
        app = out / 'CaptureFixture.app'
        app.mkdir()
        sdk = command('xcrun', '--sdk', 'iphonesimulator', '--show-sdk-path')
        arch = 'arm64' if platform.machine() == 'arm64' else 'x86_64'
        command('xcrun', '--sdk', 'iphonesimulator', 'swiftc', '-swift-version', '5', '-parse-as-library', '-sdk', sdk,
                '-target', f'{arch}-apple-ios17.0-simulator', '-framework', 'UIKit', source, '-o', app / 'CaptureFixture', timeout=120)
        info = {'CFBundleExecutable': 'CaptureFixture', 'CFBundleIdentifier': BUNDLE, 'CFBundleName': 'CaptureFixture',
                'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1', 'CFBundleShortVersionString': '1.0',
                'CFBundleSupportedPlatforms': ['iPhoneSimulator'], 'MinimumOSVersion': '17.0', 'UIDeviceFamily': [1],
                'UILaunchScreen': {}}
        (app / 'Info.plist').write_bytes(plistlib.dumps(info))
        command('codesign', '--force', '--sign', '-', app)
        verifier = out / 'verify-capture'
        mac_sdk = command('xcrun', '--sdk', 'macosx', '--show-sdk-path')
        command('xcrun', '--sdk', 'macosx', 'swiftc', '-sdk', mac_sdk, '-target', f'{arch}-apple-macos13.0',
                '-framework', 'CoreGraphics', '-framework', 'ImageIO', '-framework', 'CryptoKit',
                ROOT / 'scripts/fixtures/system-capture/VerifyCapture.swift', '-o', verifier, timeout=120)
        result['offline_verifier_checks'] = json.loads(command('python3', ROOT / 'scripts/test-system-capture-verifier.py', '--verifier', verifier))
        name = 'ipu-system-capture-' + run_id[:8]
        created_id = command('xcrun', 'simctl', 'create', name, 'com.apple.CoreSimulator.SimDeviceType.iPhone-15', args.runtime)
        uuid.UUID(created_id)
        simulator = created_id
        rows = json.loads(command('xcrun', 'simctl', 'list', 'devices', '--json'))['devices']
        matches = [d for runtime, ds in rows.items() for d in ds if runtime == args.runtime and d['udid'] == simulator and d['name'] == name]
        assert len(matches) == 1, 'owned Simulator identity mismatch'
        result['simulator'] = simulator
        command('xcrun', 'simctl', 'boot', simulator)
        command('xcrun', 'simctl', 'bootstatus', simulator, '-b', timeout=120)
        command('xcrun', 'simctl', 'install', simulator, app, timeout=120)
        command('xcrun', 'simctl', 'launch', simulator, BUNDLE, run_id)
        fixture_documents = Path(command('xcrun', 'simctl', 'get_app_container', simulator, BUNDLE, 'data')) / 'Documents'
        deadline = time.monotonic() + 30
        while not (fixture_documents / 'capture-ready.json').exists():
            if time.monotonic() >= deadline:
                raise RuntimeError('fixture did not publish its ready receipt')
            time.sleep(.1)
        ready = receipt(fixture_documents, 'capture-ready.json')
        assert ready.get('ready') is True and ready['marker_cells'] == 128, 'fixture not ready'
        media = Path.home() / 'Library/Developer/CoreSimulator/Devices' / simulator / 'data/Media'
        dcim = media / 'DCIM'
        def image_paths():
            return {p for p in dcim.rglob('*') if p.is_file() and p.suffix.lower() in ['.png', '.jpg', '.jpeg', '.heic']}
        baseline = image_paths()
        result['baseline_dcim_images'] = len(baseline)
        if not mcp_binary:
            result['phase'] = 'one_capture_test_started'
            with (out / 'xcode.log').open('wb') as log:
                process = subprocess.Popen(['xcodebuild', 'test-without-building', '-xctestrun', str(recipe),
                    '-destination', 'platform=iOS Simulator,id=' + simulator,
                    '-only-testing:IPhoneUseRunnerUITests/RunnerTests/testSystemScreenshotProbe',
                    '-parallel-testing-enabled', 'NO'], env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                result['xcode_exit'] = process.wait(timeout=180)
            host_documents = Path(command('xcrun', 'simctl', 'get_app_container', simulator, host_bundle, 'data')) / 'Documents'
            dispatch = receipt(host_documents, 'capture-dispatch.json')
            assert dispatch.get('invoked') is True and dispatch.get('acknowledged') is True and dispatch.get('saved_verified') is False, 'dispatch not acknowledged; never repeat the capture'
        else:
            runner_port = port()
            env.update(TEST_RUNNER_IPU_RUNNER_PORT=str(runner_port), TEST_RUNNER_IPU_RUNNER_MJPEG_PORT='0',
                       TEST_RUNNER_IPU_RUNNER_TOKEN=probe_token, TEST_RUNNER_IPU_DAEMON_PORT='0')
            result['phase'] = 'owned_runner_starting'
            with (out / 'xcode.log').open('wb') as log:
                process = subprocess.Popen(['xcodebuild', 'test-without-building', '-xctestrun', str(recipe),
                    '-destination', 'platform=iOS Simulator,id=' + simulator,
                    '-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe', '-parallel-testing-enabled', 'NO'],
                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            def native(path, body=None):
                raw = None if body is None else json.dumps(body).encode()
                method = 'GET' if body is None else 'POST'
                request = urllib.request.Request(f'http://127.0.0.1:{runner_port}' + path, data=raw,
                    headers={'Content-Type': 'application/json', 'Authorization': authorization(probe_token, method, path, raw or b'')})
                with opener.open(request, timeout=5) as response: return json.load(response)['value']
            until = time.monotonic() + 120
            while True:
                if process.poll() is not None: raise RuntimeError('owned Runner stopped before readiness')
                try:
                    if native('/status').get('ready') is True: break
                except (OSError, ValueError): pass
                if time.monotonic() >= until: raise RuntimeError('owned Runner readiness deadline')
                time.sleep(.2)
            result['runner_ready'] = True
            session = native('/session', {})
            assert session['capabilities'].get('ipu:systemCaptureRequest') is True, 'Native capture capability missing'
            prefix = '/session/' + session['sessionId']
            native(prefix + '/wda/apps/launch', {'bundleId': BUNDLE})
            active = native(prefix + '/wda/activeAppInfo')
            assert active['bundleId'] == BUNDLE and active['pid'] == ready['pid'], 'wrong foreground fixture'
            state = out / 'daemon-state'; state.mkdir(mode=0o700)
            token_file = state / 'runner-token'; token_file.write_text(probe_token); token_file.chmod(0o600)
            daemon_port = port()
            child_env = dict(env, IPHONE_USE_HOST='127.0.0.1', IPHONE_USE_PORT=str(daemon_port),
                IPHONE_USE_INSTANCE='cap-' + run_id[:8].lower(), IPHONE_USE_STATE_DIR=str(state),
                IPHONE_USE_SIMULATOR=simulator, IPHONE_USE_RUNNER_MANAGED='0',
                IPHONE_USE_RUNNER_URL=f'http://127.0.0.1:{runner_port}',
                IPHONE_USE_RUNNER_MJPEG_URL=f'http://127.0.0.1:{runner_port}', IPU_RUNNER_TOKEN_FILE=str(token_file),
                IPHONE_USE_AGENT_TOKEN=probe_token, IPHONE_USE_PASSWORD='fixture-' + probe_token,
                IPHONE_USE_AUTO_FOCUS='0', IPHONE_USE_IDLE_RELEASE_SECS='0', IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1',
                IPHONE_USE_NO_UPDATE_CHECK='1')
            with (out / 'daemon.log').open('w') as log:
                daemon_process = subprocess.Popen([str(daemon_binary), 'serve'], env=child_env,
                    stdout=log, stderr=log, start_new_session=True)
            def daemon_status():
                request = urllib.request.Request(f'http://127.0.0.1:{daemon_port}/agent/status',
                    headers={'Authorization': 'Bearer ' + probe_token})
                with opener.open(request, timeout=5) as response: return json.load(response)
            until = time.monotonic() + 60
            while True:
                if daemon_process.poll() is not None: raise RuntimeError('owned daemon stopped before readiness')
                try:
                    status = daemon_status()
                    assert status.get('target') == 'simulator' and status.get('udid') == simulator, 'wrong daemon target'
                    if status.get('drivable') is True: break
                except (OSError, ValueError): pass
                if time.monotonic() >= until: raise RuntimeError('owned daemon readiness deadline')
                time.sleep(.2)
            peer_env = dict(child_env, IPHONE_USE_URL=f'http://127.0.0.1:{daemon_port}', IPHONE_USE_TOKEN=probe_token,
                IPHONE_USE_OWNER='capture-' + run_id[:8].lower(), IPHONE_USE_MCP_PREWARM='0')
            peer = Peer([str(mcp_binary)], peer_env, out / 'mcp.log')
            _, _, routed = payload(peer.request('tools/call', {'name': 'phone_status', 'arguments': {}}, timeout=15))
            assert routed.get('target') == 'simulator' and routed.get('udid') == simulator and routed.get('drivable') is True, 'MCP target mismatch'
            held, _, _ = payload(peer.request('tools/call', {'name': 'phone_hold', 'arguments': {'secs': 60, 'private': True}}, timeout=15))
            assert held.get('isError') is not True and daemon_status().get('screen_private_secs', 0) > 0
            refused, _, refusal = payload(peer.request('tools/call', {'name': 'phone_shortcut',
                'arguments': {'name': 'save_screenshot', 'observe': False}}, timeout=20))
            (out / 'capture-private-refusal.json').write_text(json.dumps(refusal, indent=2) + '\n')
            assert refused.get('isError') is True and refusal.get('outcome') == 'not_sent' and refusal.get('retry_safe') is True
            assert image_paths() == baseline, 'capture occurred during private hold'
            cleared, _, _ = payload(peer.request('tools/call', {'name': 'phone_hold', 'arguments': {'secs': 0}}, timeout=15))
            assert cleared.get('isError') is not True and daemon_status().get('screen_private_secs') == 0
            # Exactly one capture request after the prior, acknowledged not-sent
            # refusal. A timeout leaves the same Peer handle pending; no retry.
            (out / 'mcp-capture-attempt.json').write_text(json.dumps({'run_id': run_id}) + '\n')
            result['phase'] = 'mcp_capture_requested_once'
            reply = peer.request('tools/call', {'name': 'phone_shortcut',
                'arguments': {'name': 'save_screenshot', 'observe': False}}, timeout=25)
            (out / 'capture-mcp-reply.json').write_text(json.dumps(reply, indent=2) + '\n')
            tool, message, body = payload(reply)
            assert tool.get('isError') is not True and body.get('ok') is True, 'MCP did not confirm the request'
            capture = body.get('system_capture', {})
            assert capture.get('requested') is True and capture.get('saved_verified') is False
            assert 'not independently verified' in message, 'text-only clients lost the save boundary'
            result['private_hold_refused_before_capture'] = True
            result['mcp_request_acknowledged'] = True
        deadline = time.monotonic() + 30
        registered = None
        original_path = None
        while time.monotonic() < deadline:
            added = image_paths() - baseline
            assert len(added) <= 1, 'more than one new original image; attribution is ambiguous'
            database = media / 'PhotoData/Photos.sqlite'
            if len(added) == 1:
                original_path = added.pop()
                original_path.resolve().relative_to(media.resolve())
            if original_path is not None and database.exists():
                try:
                    with sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=1) as connection:
                        connection.row_factory = sqlite3.Row
                        columns = {row[1] for row in connection.execute('PRAGMA table_info(ZASSET)')}
                        result['photos_schema_supported'] = {'ZFILENAME', 'ZDIRECTORY'}.issubset(columns)
                        if {'ZFILENAME', 'ZDIRECTORY'}.issubset(columns):
                            selected = [key for key in ['Z_PK', 'ZFILENAME', 'ZDIRECTORY', 'ZUUID', 'ZKIND', 'ZKINDSUBTYPE', 'ZTRASHEDSTATE'] if key in columns]
                            rows = [dict(row) for row in connection.execute('SELECT ' + ','.join(selected) + ' FROM ZASSET WHERE ZFILENAME=?', (original_path.name,))]
                            relative_directory = original_path.parent.relative_to(media).as_posix()
                            rows = [row for row in rows if row['ZDIRECTORY'] in [relative_directory, original_path.parent.name] and row.get('ZTRASHEDSTATE', 0) == 0]
                            if len(rows) == 1:
                                registered = rows[0]
                                break
                except sqlite3.Error as error:
                    result['photos_observation_error'] = str(error)
                    # Read-only observation of the same pending asset, never another capture.
            time.sleep(.2)
        assert registered is not None and original_path is not None, 'could not verify new original image registration in Photos; never repeat capture'
        (out / 'capture-photos-registration.json').write_text(json.dumps({'run_id': run_id, 'asset': registered,
            'original_relative_path': original_path.relative_to(media).as_posix(),
            'classification_note': 'raw private subtype retained without assuming its enum meaning'}, indent=2) + '\n')
        original = original_path.read_bytes()
        (out / 'capture-photo-original.bin').write_bytes(original)
        photo = json.loads(command(verifier, out / 'capture-photo-original.bin', out / 'capture-ready.json'))
        (out / 'capture-photo.json').write_text(json.dumps(photo, indent=2) + '\n')
        assert photo['run_id'] == run_id and photo['verified'] is True, 'original Photos bytes do not match this run marker'
        assert photo['matched_cells'] == photo['marker_cells'] == 128 and photo['negative_matched_cells'] < 128
        assert hashlib.sha256(original).hexdigest() == photo['photo_sha256'], 'asset bytes hash mismatch'
        assert result.get('runner_ready') is True if mcp_binary else result.get('xcode_exit') == 0, 'Runner/XCTest did not complete its required stage'
        result.update(capture_verified=True, phase='new_photos_asset_matches_fixture', photo_sha256=photo['photo_sha256'])
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
    finally:
        if peer:
            try: peer.close()
            except Exception as error: result['cleanup_errors'].append(f'MCP close: {error}')
            try: stop(peer.process)
            except Exception as error: result['cleanup_errors'].append(f'owned MCP: {error}')
        if daemon_process:
            try: stop(daemon_process)
            except Exception as error: result['cleanup_errors'].append(f'owned daemon: {error}')
        try: stop_child()
        except Exception as error: result['cleanup_errors'].append(f'owned XCTest process: {error}')
        # A timed-out xcodebuild can already have dispatched the one capture.
        # Recover its existing receipt read-only before deleting the owned Sim.
        if simulator and host_bundle and host_documents is None:
            try:
                host_documents = Path(command('xcrun', 'simctl', 'get_app_container', simulator, host_bundle, 'data', timeout=10)) / 'Documents'
            except Exception as error:
                result['dispatch_evidence_unavailable'] = str(error)
        if media is not None and baseline is not None:
            try:
                pending_images = image_paths() - baseline
                if len(pending_images) == 1 and not (out / 'capture-photo-original.bin').exists():
                    candidate = pending_images.pop()
                    candidate.resolve().relative_to(media.resolve())
                    (out / 'capture-photo-original.bin').write_bytes(candidate.read_bytes())
                    result['pending_original_retained'] = True
            except Exception as error:
                result['photo_evidence_unavailable'] = str(error)
        for directory, names in [(fixture_documents, ['capture-ready.json']), (host_documents, ['capture-dispatch.json'])]:
            if directory:
                for name in names:
                    try:
                        if (directory / name).exists(): receipt(directory, name)
                    except Exception as error:
                        result['cleanup_errors'].append(f'receipt {name}: {error}')
        if mcp_binary and (out / 'xcode.log').exists():
            lines = (out / 'xcode.log').read_text(errors='replace').splitlines()
            dispatches = {line for line in lines if re.search(r'ipu-runner: POST /(?:session/[^/\s]+/)?ipu/system-capture ->', line)}
            result['native_capture_posts_logged'] = len(dispatches)
            if result['capture_verified'] and len(dispatches) != 1:
                result['cleanup_errors'].append('expected exactly one Native capture POST across privacy refusal and successful request')
        for name in ['xcode.log', 'daemon.log', 'mcp.log']:
            log = out / name
            if log.exists(): log.write_text(log.read_text(errors='replace').replace(probe_token, '[redacted fixture token]'))
        # Preserve evidence before potentially slow CoreSimulator cleanup.
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        if simulator:
            for operation in ['shutdown', 'delete']:
                try: command('xcrun', 'simctl', operation, simulator, timeout=30)
                except Exception as error: result['cleanup_errors'].append(f'{operation}: {error}')
        result['passed'] = result['capture_verified'] and not result['cleanup_errors']
        (out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result), flush=True)
    return 0 if result['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
