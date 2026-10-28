#!/usr/bin/env python3
"""Verify one native system-capture request against a new Photos asset on an owned Simulator."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import signal
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = 'com.leeguoo.iphone-use.capture-fixture'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--xctestrun', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--runtime', required=True)
    args = parser.parse_args()
    recipe = args.xctestrun.resolve(strict=True)
    out = args.out.resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    run_id = str(uuid.uuid4()).upper()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('IPU_', 'TEST_RUNNER_IPU_', 'IPHONE_USE_', 'SIMCTL_CHILD_'))}
    env['TEST_RUNNER_IPU_CAPTURE_PROBE_RUN_ID'] = run_id
    result = {'run_id': run_id, 'scope': 'owned Simulator only; native request plus independent Photos/128-cell proof; not physical evidence',
              'passed': False, 'capture_verified': False, 'cleanup_errors': [], 'recipe_sha256': hashlib.sha256(recipe.read_bytes()).hexdigest()}
    simulator = process = fixture_documents = host_documents = None

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
                '-target', f'{arch}-apple-ios17.0-simulator', '-framework', 'UIKit', '-framework', 'Photos', '-framework', 'ImageIO',
                '-framework', 'CryptoKit', source, '-o', app / 'CaptureFixture', timeout=120)
        info = {'CFBundleExecutable': 'CaptureFixture', 'CFBundleIdentifier': BUNDLE, 'CFBundleName': 'CaptureFixture',
                'CFBundlePackageType': 'APPL', 'CFBundleVersion': '1', 'CFBundleShortVersionString': '1.0',
                'CFBundleSupportedPlatforms': ['iPhoneSimulator'], 'MinimumOSVersion': '17.0', 'UIDeviceFamily': [1],
                'UILaunchScreen': {}, 'NSPhotoLibraryUsageDescription': 'Verify a new system screenshot in this disposable Simulator.'}
        (app / 'Info.plist').write_bytes(plistlib.dumps(info))
        command('codesign', '--force', '--sign', '-', app)
        name = 'ipu-system-capture-' + run_id[:8]
        simulator = command('xcrun', 'simctl', 'create', name, 'com.apple.CoreSimulator.SimDeviceType.iPhone-15', args.runtime)
        uuid.UUID(simulator)
        rows = json.loads(command('xcrun', 'simctl', 'list', 'devices', '--json'))['devices']
        matches = [d for runtime, ds in rows.items() for d in ds if runtime == args.runtime and d['udid'] == simulator and d['name'] == name]
        assert len(matches) == 1, 'owned Simulator identity mismatch'
        result['simulator'] = simulator
        command('xcrun', 'simctl', 'boot', simulator)
        command('xcrun', 'simctl', 'bootstatus', simulator, '-b', timeout=120)
        command('xcrun', 'simctl', 'install', simulator, app)
        command('xcrun', 'simctl', 'privacy', simulator, 'grant', 'photos', BUNDLE)
        command('xcrun', 'simctl', 'launch', simulator, BUNDLE, run_id)
        fixture_documents = Path(command('xcrun', 'simctl', 'get_app_container', simulator, BUNDLE, 'data')) / 'Documents'
        deadline = time.monotonic() + 30
        while not (fixture_documents / 'capture-ready.json').exists():
            if time.monotonic() >= deadline:
                raise RuntimeError('fixture did not publish its ready receipt')
            time.sleep(.1)
        ready = receipt(fixture_documents, 'capture-ready.json')
        assert ready.get('ready') is True and ready['marker_cells'] == 128, 'fixture not ready'
        result['phase'] = 'one_capture_test_started'
        with (out / 'xcode.log').open('wb') as log:
            process = subprocess.Popen(['xcodebuild', 'test-without-building', '-xctestrun', str(recipe),
                '-destination', 'platform=iOS Simulator,id=' + simulator,
                '-only-testing:IPhoneUseRunnerUITests/RunnerTests/testSystemScreenshotProbe',
                '-parallel-testing-enabled', 'NO', '-test-iterations', '1'], env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            result['xcode_exit'] = process.wait(timeout=180)
        host_documents = Path(command('xcrun', 'simctl', 'get_app_container', simulator, host_bundle, 'data')) / 'Documents'
        dispatch = receipt(host_documents, 'capture-dispatch.json')
        assert dispatch.get('invoked') is True and dispatch.get('acknowledged') is True and dispatch.get('saved_verified') is False, 'dispatch not acknowledged; never repeat the capture'
        deadline = time.monotonic() + 30
        while not (fixture_documents / 'capture-photo.json').exists():
            if time.monotonic() >= deadline:
                raise RuntimeError('no independently verified new Photos asset; never repeat the capture')
            time.sleep(.1)
        photo = receipt(fixture_documents, 'capture-photo.json')
        assert photo['pid'] == ready['pid'] and photo.get('verified') is True, 'Photos image does not match this fixture/run'
        assert photo['new_assets'] == 1 and photo['screenshot_subtype'] is True and photo['matched_cells'] == photo['marker_cells'] == 128
        assert photo['negative_matched_cells'] < 128, 'pixel verifier accepted a wrong challenge'
        assert photo['pixel_width'] == ready['screen_width'] and photo['pixel_height'] == ready['screen_height'], 'saved image has unexpected dimensions'
        assert result['xcode_exit'] == 0, 'XCTest failed despite retained capture receipts'
        result.update(capture_verified=True, phase='new_photos_asset_matches_fixture', photo_sha256=photo['photo_sha256'])
    except Exception as error:
        result['error'] = f'{type(error).__name__}: {error}'
    finally:
        stop_child()
        for directory, names in [(fixture_documents, ['capture-ready.json', 'capture-photo.json']), (host_documents, ['capture-dispatch.json'])]:
            if directory:
                for name in names:
                    try:
                        if (directory / name).exists(): receipt(directory, name)
                    except Exception as error:
                        result['cleanup_errors'].append(f'receipt {name}: {error}')
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
