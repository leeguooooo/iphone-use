#!/usr/bin/env python3
"""Independent UIKit touch recipients across a controlled read/action race.

Only the explicit owned Simulator is used. The proxy forwards real signed
requests and real Native responses; it never fabricates a tree or coverage.
"""
import argparse
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import socket
import subprocess
import threading
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from simulator_ui_readiness import launch_once, observe_process, state_reasons

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('moving_transport', ROOT / 'scripts/test-drag-simulator.py')
drag = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drag)
BUNDLE = 'com.leeguoo.iphone-use.moving-target-oracle'


def build(out):
    project = out / 'fixture-src'
    shutil.copytree(ROOT / 'runner/gesture-fixture', project)
    (project / 'main.swift').write_bytes((ROOT / 'scripts/fixtures/moving-target/main.swift').read_bytes())
    pbx = project / 'GestureFixture.xcodeproj/project.pbxproj'
    pbx.write_text(pbx.read_text().replace('com.leeguoo.iphone-use.gesture-fixture', BUNDLE))
    info = project / 'Info.plist'
    data = plistlib.loads(info.read_bytes())
    data.update(CFBundleIdentifier=BUNDLE, CFBundleName='Moving Target Oracle', CFBundleDisplayName='Moving Target Oracle')
    data['UIApplicationSceneManifest']['UISceneConfigurations']['UIWindowSceneSessionRoleApplication'][0]['UISceneDelegateClassName'] = '$(PRODUCT_MODULE_NAME).MovingSceneDelegate'
    info.write_bytes(plistlib.dumps(data))
    result = drag.command(['xcodebuild', '-project', project / 'GestureFixture.xcodeproj', '-scheme', 'GestureFixture',
        '-configuration', 'Release', '-sdk', 'iphonesimulator', '-destination', 'generic/platform=iOS Simulator',
        '-derivedDataPath', out / 'fixture-build', '-jobs', '1', 'OTHER_SWIFT_FLAGS=$(inherited) -j1',
        'CODE_SIGNING_ALLOWED=NO', 'build'], check=False, timeout=180)
    (out / 'fixture-build.log').write_text(result.stdout + result.stderr)
    assert result.returncode == 0, 'fixture build failed'
    app = out / 'fixture-build/Build/Products/Release-iphonesimulator/GestureFixture.app'
    assert plistlib.loads((app / 'Info.plist').read_bytes())['CFBundleIdentifier'] == BUNDLE
    drag.command(['codesign', '--force', '--sign', '-', app])
    return app


def close_rect(left, right):
    return all(type(left.get(k)) in (int, float) and abs(left[k] - right[k]) <= 1 for k in ('x', 'y', 'width', 'height'))


def create_consumer_state():
    # macOS TMPDIR commonly starts /var, a symlink to /private/var. The daemon
    # deliberately rejects symlinked state paths; resolve our own new directory.
    created = Path(tempfile.mkdtemp(prefix='ipu-race-consumer-'))
    try:
        path = created.resolve(strict=True)
        path.chmod(0o700)
        return path
    except Exception:
        shutil.rmtree(created)
        raise


def judge_case(case, body, delta, established, expect_race, activation_delta=0):
    keys = {'race.target', 'race.overlay', 'race.decoy', 'background'}
    if not isinstance(delta, dict) or set(delta) != keys or any(type(v) is not int or v < 0 for v in delta.values()):
        return False
    wrong = delta['race.overlay'] + delta['race.decoy']
    if case == 'control':
        return body.get('ok') is True and delta['race.target'] == 1 and wrong == 0 and activation_delta == 1
    if not established:
        return False
    if case == 'after-rect':
        if expect_race:
            return wrong == 1 and delta['race.target'] == 0
        return body.get('ok') is False and body.get('error') == 'target_changed' and body.get('outcome') == 'not_sent' and body.get('retry_safe') is True and all(v == 0 for v in delta.values())
    if case == 'after-query':
        return body.get('ok') is False and wrong == 0 and delta['race.target'] == 0
    return False


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class OwnedDaemon(drag.Daemon):
    def call(self, path, body=None):
        headers = {"X-Phone-Owner": self.owner}
        data = None if body is None else json.dumps(body).encode()
        if body is not None:
            headers.update({"Content-Type": "application/json", "X-Phone-Control": "1"})
        request = urllib.request.Request(self.base + path, data=data, headers=headers)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        with opener.open(request, timeout=40) as response:
            return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('base', 'udid', 'owner', 'product-version'):
        parser.add_argument('--' + name, required=True)
    for name in ('mcp', 'runner-record', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--expect-after-read-race', action='store_true', help='Diagnostic only: require observed wrong recipient, never acceptance')
    args = parser.parse_args()
    uuid.UUID(args.udid)
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    parsed_base = urllib.parse.urlparse(args.base)
    assert parsed_base.scheme == 'http' and parsed_base.hostname in ('127.0.0.1', 'localhost', '::1')
    assert not parsed_base.username and not parsed_base.password and not parsed_base.query and not parsed_base.fragment
    original = OwnedDaemon(args.base, args.owner)
    original.require_simulator(args.udid)
    record = json.loads(args.runner_record.read_text())
    assert record['udid'].lower() == args.udid.lower()
    assert all(type(record.get(key)) is int and 1 <= record[key] <= 65535 for key in ('port', 'mjpeg_port', 'daemon_port'))
    assert urllib.parse.urlparse(args.base).port == record['daemon_port']
    token_path = args.runner_record.parent / 'runner-token'
    assert not token_path.is_symlink()
    assert token_path.resolve().parent == args.runner_record.parent.resolve() and token_path.is_file()
    token = token_path.read_text().strip()
    assert re.fullmatch(r'[0-9a-f]{32,128}', token)
    daemon_binary = args.mcp.resolve().with_name('iphone-use')
    assert drag.command([daemon_binary, '--version']).stdout.strip() == 'iphone-use ' + args.product_version
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    state = {'passed': False, 'scope': 'Owned Simulator; real Native responses and independent UIKit recipients',
             'diagnostic_expects_race': args.expect_after_read_race, 'product_acceptance': False,
             'cases': [], 'cleanup_errors': [], 'transport': 'public HTTP /agent/actions;MCP not exercised', 'mcp_sha256': hashlib.sha256(args.mcp.read_bytes()).hexdigest(), 'daemon_sha256': hashlib.sha256(daemon_binary.read_bytes()).hexdigest()}
    installed = False
    child = None
    proxy = None
    proxy_thread = None
    sequence = 0
    temporary_state = None
    current_before = None
    nonce = str(uuid.uuid4())
    data_dir = None
    expected_pid = None
    armed = None
    target_ids = set()
    hooks = []
    traffic = []

    def save(name, value):
        path = args.out / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value, indent=2) + '\n')

    def native_call(target, body=None):
        assert target.startswith("/") and not target.startswith("//")
        data = None if body is None else json.dumps(body).encode()
        method = "GET" if body is None else "POST"
        request = urllib.request.Request(f"http://127.0.0.1:{record['port']}" + target, data=data,
            headers={"Content-Type": "application/json", "Authorization": drag.authorization(token, method, target, data or b"")})
        with opener.open(request, timeout=30) as response:
            return json.load(response), response.headers

    def app_state():
        value = json.loads((data_dir / 'race-state.json').read_text())
        assert not state_reasons(value, nonce, expected_pid), 'App identity/foreground changed'
        return value

    def command(move):
        nonlocal sequence
        sequence += 1
        temporary = data_dir / 'race-command.tmp'
        temporary.write_text(json.dumps({'run_id': nonce, 'pid': expected_pid, 'sequence': sequence, 'move': move}))
        temporary.replace(data_dir / 'race-command.json')
        until = time.monotonic() + 5
        while time.monotonic() < until:
            value = app_state()
            if value['command_sequence'] == sequence and value['moved'] is move:
                return value
            time.sleep(0.03)
        raise RuntimeError('one App command not acknowledged; no replay')

    def move_after_response(element_id, captured):
        moved = command(True)
        if isinstance(captured.get('value'), dict) and 'x' in captured['value']:
            assert close_rect(captured['value'], current_before['target_rect']), 'captured frame was not the confirmed old pose'
            assert not close_rect(captured['value'], moved['target_rect']), 'App did not change the target pose'
        # A new signed read only: prove Native sees the real new frame before
        # returning the older response to the consumer. Never reuse an auth nonce.
        until = time.monotonic() + 4
        while time.monotonic() < until:
            observed, _ = native_call('/element/' + urllib.parse.quote(element_id, safe='') + '/rect')
            if close_rect(observed['value'], moved['target_rect']):
                receipt = {'sequence': sequence, 'app': moved, 'captured_native': captured,
                           'fresh_native': observed, 'native_matches_app_new_frame': True}
                hooks.append(receipt)
                return
            time.sleep(0.1)
        raise RuntimeError('App moved but fresh Native frame not confirmed; race not established')

    class Proxy(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.forward()

        def do_POST(self):
            self.forward()

        def do_DELETE(self):
            self.forward()

        def forward(self):
            nonlocal armed
            raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'connection', 'content-length')}
            req = urllib.request.Request(f"http://127.0.0.1:{record['port']}" + self.path,
                    data=raw if self.command != 'GET' else None, headers=headers, method=self.command)
            try:
                upstream = opener.open(req, timeout=35)
            except urllib.error.HTTPError as error:
                upstream = error
            try:
                with upstream:
                    status, response_headers, body = upstream.status, dict(upstream.headers), upstream.read()
                path = urllib.parse.unquote(self.path.split('?', 1)[0])
                query = self.command == 'POST' and path.endswith('/elements') and b'race.target' in raw
                trace = {'method': self.command, 'path': path, 'status': status,
                         'request_sha256': hashlib.sha256(raw).hexdigest(),
                         'response_sha256': hashlib.sha256(body).hexdigest()}
                if self.command == 'POST' and path.endswith(('/actions', '/click', '/scrollTo')):
                    trace['action'] = json.loads(raw) if raw else None
                traffic.append(trace)
                parsed = None
                if status == 200 and (query or armed == 'after-rect' and self.command == 'GET' and path.endswith('/rect')):
                    try:
                        parsed = json.loads(body)
                    except ValueError:
                        trace['hook_parse_error'] = True
                if status == 200 and query and isinstance(parsed, dict):
                    ids = [row.get('ELEMENT') or row.get('element-6066-11e4-a52e-4f735466cecf') for row in parsed.get('value', [])]
                    target_ids.update(id for id in ids if isinstance(id, str))
                    if armed == 'after-query' and len(ids) == 1:
                        armed = None
                        move_after_response(ids[0], parsed)
                if status == 200 and armed == 'after-rect' and self.command == 'GET' and isinstance(parsed, dict):
                    hit = next((id for id in target_ids if path.endswith('/element/' + id + '/rect')), None)
                    if hit is not None:
                        armed = None
                        move_after_response(hit, parsed)
                self.send_response(status)
                for key, value in response_headers.items():
                    if key.lower() not in ('connection', 'content-length', 'transfer-encoding'):
                        self.send_header(key, value)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except Exception as error:
                save('proxy-error.json', {'type': type(error).__name__, 'message': str(error)})
                self.send_error(502, 'owned race fixture failed; no action replay')

    try:
        assert drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'data'], check=False).returncode != 0, 'existing fixture must not be overwritten'
        app = build(args.out)
        drag.command(['xcrun', 'simctl', 'install', args.udid, app])
        installed = True
        data_dir = Path(drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'data']).stdout.strip()) / 'Documents'
        started = time.monotonic()
        def sdk(label, argv, timeout):
            return drag.command(argv, timeout=timeout).stdout.strip()
        expected_pid, launch = launch_once(sdk, args.udid, BUNDLE, nonce)
        save('launch.json', launch)
        until = min(started + 210, time.monotonic() + 60)
        while True:
            try:
                ready = json.loads((data_dir / 'race-state.json').read_text())
                if not state_reasons(ready, nonce, expected_pid) and ready.get('appeared') is True:
                    app_container = Path(drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'app']).stdout.strip())
                    valid, detail = observe_process(sdk, ready['pid'], str(app_container / 'GestureFixture'), nonce, 10)
                    if valid:
                        expected_pid = ready['pid']
                        break
            except (OSError, ValueError, KeyError):
                pass
            assert time.monotonic() < until, 'one launch not confirmed; no replay'
            time.sleep(0.1)
        active, _ = native_call('/wda/activeAppInfo')
        assert active['value']['bundleId'] == BUNDLE and active['value']['pid'] == expected_pid
        proxy = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
        proxy_thread = threading.Thread(target=lambda: proxy.serve_forever(poll_interval=0.05), daemon=True)
        proxy_thread.start()
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        temporary_state = create_consumer_state()
        child_state = temporary_state
        copied = child_state / 'runner-token'
        copied.write_text(token)
        copied.chmod(0o600)
        owner = 'race-' + nonce[:8]
        env = {k: v for k, v in os.environ.items() if not k.startswith(('IPHONE_USE_', 'PHONE_REMOTE_', 'IOS_USE_', 'IPU_', 'TEST_RUNNER_')) and k not in ('GH_TOKEN', 'GITHUB_TOKEN')}
        env.update(IPHONE_USE_INSTANCE=owner, IPHONE_USE_STATE_DIR=str(child_state), IPHONE_USE_SIMULATOR=args.udid,
                   IPHONE_USE_RUNNER_URL=f'http://127.0.0.1:{proxy.server_port}', IPHONE_USE_RUNNER_MJPEG_URL=f'http://127.0.0.1:{record["mjpeg_port"]}',
                   IPHONE_USE_RUNNER_MANAGED='false', IPHONE_USE_HOST='127.0.0.1', IPHONE_USE_PORT=str(port),
                   IPHONE_USE_OWNER=owner, IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1', CI='true')
        with (args.out / 'proxy-daemon.log').open('w') as log:
            child = subprocess.Popen([str(daemon_binary), 'serve'], env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        consumer = OwnedDaemon(f'http://127.0.0.1:{port}', owner)
        until = time.monotonic() + 30
        while True:
            assert child.poll() is None, 'proxy consumer stopped'
            try:
                consumer.require_simulator(args.udid)
                break
            except (OSError, RuntimeError):
                assert time.monotonic() < until, 'proxy consumer not ready'
                time.sleep(0.1)
        for case, via, hook in [('control', 'element', None), ('after-query', 'element', 'after-query'), ('after-rect', 'element', 'after-rect')]:
            before = command(False)
            current_before = before
            target_ids.clear()
            hooks.clear()
            traffic.clear()
            armed = hook
            action = {'observe': False, 'steps': [{'kind': 'action', 'action': {'type': 'tap_locator', 'locator': {'identifier': 'race.target', 'kind': 'Button'}, 'via': via}}]}
            try:
                body = consumer.call('/agent/actions', action)
            except urllib.error.HTTPError as error:
                body = json.load(error)
            after = app_state()
            samples = []
            until = time.monotonic() + 3
            while time.monotonic() < until:
                after = app_state()
                samples.append(after)
                time.sleep(0.1)
            save(case + '/before.json', before)
            save(case + '/response.json', body)
            save(case + '/hooks.json', hooks)
            save(case + '/traffic.json', traffic)
            save(case + '/quiet.json', samples)
            delta = {k: after['received'][k] - before['received'][k] for k in before['received']}
            wrong = delta['race.overlay'] + delta['race.decoy']
            established = hook is None or len(hooks) == 1 and hooks[0]['native_matches_app_new_frame']
            passed = judge_case(case, body, delta, established, args.expect_after_read_race,
                                after['activated']['race.target'] - before['activated']['race.target'])
            if case == 'control':
                passed = passed and any(t['method'] == 'POST' and t['path'].endswith('/elements') and t['status'] == 200 for t in traffic)
                passed = passed and any(t['method'] == 'POST' and t['path'].endswith('/click') and t['status'] == 200 for t in traffic)
            if case == 'after-query':
                daemon_log = (args.out / 'proxy-daemon.log').read_text(errors='replace')
                passed = passed and body.get('error') == 'outcome_unknown' and body.get('retry_safe') is False
                passed = passed and any(t['method'] == 'POST' and t['path'].endswith('/scrollTo') for t in traffic)
                passed = passed and all(t['status'] == 200 for t in traffic if t['method'] == 'GET' or t['path'].endswith('/elements'))
                passed = passed and 'element_occluded after reveal; no tap sent' in daemon_log

            state['cases'].append({'name': case, 'passed': passed, 'delta': delta, 'established': established,
                                   'diagnostic_counterexample': case == 'after-rect' and args.expect_after_read_race})
            assert passed, f'{case}: independent touch recipients violate expectation: {delta}'
        active, _ = native_call('/wda/activeAppInfo')
        assert active['value']['bundleId'] == BUNDLE and active['value']['pid'] == expected_pid
        state['passed'] = True
        state['product_acceptance'] = False
        state['race_regression_passed'] = not args.expect_after_read_race
    except Exception as error:
        state['error'] = type(error).__name__ + ': ' + str(error)
        raise
    finally:
        try:
            if child is not None:
                try:
                    os.killpg(child.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
                until = time.monotonic() + 2
                group_alive = True
                while time.monotonic() < until:
                    try: os.killpg(child.pid, 0)
                    except ProcessLookupError:
                        group_alive = False
                        break
                    time.sleep(0.05)
                if group_alive:
                    os.killpg(child.pid, signal.SIGKILL)
                    until = time.monotonic() + 5
                    while time.monotonic() < until:
                        try: os.killpg(child.pid, 0)
                        except ProcessLookupError:
                            group_alive = False
                            break
                        time.sleep(0.05)
                if group_alive:
                    raise RuntimeError('owned process group still exists after stop')
        except Exception as error:
            state['cleanup_errors'].append('owned consumer termination unconfirmed: ' + str(error))
        state['owned_consumer_stopped'] = (child is None or child.poll() is not None) and not any('termination' in error or 'group' in error for error in state['cleanup_errors'])
        if proxy:
            try:
                proxy.shutdown()
                proxy.server_close()
                proxy_thread.join(timeout=2)
                assert not proxy_thread.is_alive()
            except Exception as error:
                state['cleanup_errors'].append('owned proxy shutdown: ' + str(error))
        if temporary_state and state['owned_consumer_stopped']:
            try:
                shutil.rmtree(temporary_state)
            except Exception as error:
                state['cleanup_errors'].append('temporary credential state cleanup: ' + str(error))
        elif temporary_state:
            state['cleanup_errors'].append('owned consumer termination unconfirmed;temporary credential state preserved outside artifacts')
        log = args.out / 'proxy-daemon.log'
        if log.exists():
            try:
                log.write_text(log.read_text(errors='replace').replace(token, 'REDACTED'))
            except Exception as error:
                state['cleanup_errors'].append('log redaction failed: ' + str(error))
                try: log.unlink()
                except OSError: pass
        if installed:
            try:
                drag.command(['xcrun', 'simctl', 'uninstall', args.udid, BUNDLE])
            except Exception as error:
                state['cleanup_errors'].append(str(error))
        if state['cleanup_errors'] or not state['owned_consumer_stopped']:
            state['passed'] = False
        save('result.json', state)
    if not state['passed']:
        raise RuntimeError('diagnostic or cleanup failed;inspect result.json;no replay')


if __name__ == '__main__':
    main()
