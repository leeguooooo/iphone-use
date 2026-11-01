#!/usr/bin/env python3
"""Compare Native state and controlled text input to UIKit on one owned Simulator."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

if not __debug__:
    raise SystemExit("state oracle requires assertions")

from mcp_probe_peer import Peer, payload as mcp_payload
from simulator_ui_readiness import launch_once, observe_process, state_reasons

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('state_transport', ROOT / 'scripts/test-moving-target-simulator.py')
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)
drag = transport.drag
BUNDLE = 'com.leeguoo.iphone-use.state-oracle'
CASES = [
    ('enabled', 'state.enabled', 'enabled', True),
    ('disabled', 'state.disabled', 'enabled', False),
    ('selected', 'state.selected', 'selected', True),
    ('unselected', 'state.unselected', 'selected', False),
    ('focus-before', 'state.field', 'focused', False),
    ('focus-on', 'state.field', 'focused', True),
    ('focus-off', 'state.field', 'focused', False),
]


def judge(before, after, nonce, pid, identifier, field, expected, value):
    for sample in (before, after):
        if state_reasons(sample, nonce, pid) or type(sample.get('touches')) is not int or sample['touches'] != 0:
            return False
        if sample.get('controls', {}).get(identifier, {}).get(field) is not expected:
            return False
    return type(value) is bool and value is expected


def focus_confirmed(value, sequence, wanted):
    if not isinstance(value, dict) or type(sequence) is not int or type(wanted) is not bool:
        return False
    controls = value.get('controls')
    field = controls.get('state.field') if isinstance(controls, dict) else None
    if not isinstance(field, dict):
        return False
    receipt = value.get('focus_command')
    return (type(value.get('command_sequence')) is int and value['command_sequence'] == sequence
            and isinstance(receipt, dict)
            and type(receipt.get('sequence')) is int and receipt['sequence'] == sequence
            and type(receipt.get('requested')) is bool and receipt['requested'] is wanted
            and type(receipt.get('acknowledged')) is bool and receipt['acknowledged'] is True
            and type(receipt.get('before')) is bool and type(receipt.get('after')) is bool
            and field.get('focused') is wanted)


def text_state_matches(value, text):
    if not isinstance(value, dict) or not isinstance(value.get('controls'), dict):
        return False
    field = value['controls'].get('state.field')
    other = value['controls'].get('state.other')
    if not isinstance(field, dict) or not isinstance(other, dict):
        return False
    return (type(value.get('touches')) is int and value['touches'] == 0
            and type(value.get('return_events')) is int and value['return_events'] == 0
            and field.get('focused') is True and other.get('focused') is False
            and type(field.get('characters')) is int and field['characters'] == len(text)
            and type(field.get('utf16_length')) is int and field['utf16_length'] == len(text.encode('utf-16-le')) // 2
            and field.get('sha256') == hashlib.sha256(text.encode()).hexdigest()
            and other.get('sha256') == hashlib.sha256(b'untouched').hexdigest())


def selected_replacement_matches(before, after, text, response):
    if not isinstance(before, dict) or not isinstance(after, dict) or not isinstance(response, dict):
        return False
    controls = after.get('controls', {})
    prior = before.get('controls', {})
    timing = response.get('timing', {})
    if not isinstance(controls, dict) or not isinstance(prior, dict) or not isinstance(timing, dict):
        return False
    target, other = controls.get('state.other', {}), controls.get('state.field', {})
    start, untouched = prior.get('state.other', {}), prior.get('state.field', {})
    if not all(isinstance(value, dict) for value in (target, other, start, untouched)):
        return False
    routes = timing.get('runner', [])
    if not isinstance(routes, list):
        return False
    calls = {}
    for row in routes:
        if not isinstance(row, dict) or type(row.get('count')) is not int or row['count'] < 1:
            return False
        call = row.get('call')
        if not isinstance(call, str):
            return False
        calls[call] = calls.get(call, 0) + row['count']
    return (response.get('ok') is True
        and type(before.get('pid')) is int and before['pid'] > 0 and type(after.get('pid')) is int
        and all(before.get(key) == after.get(key) and before.get(key) is not None for key in ('pid', 'run_id', 'controller_id'))
        and type(before.get('tick')) is int and type(after.get('tick')) is int and after['tick'] > before['tick']
        and type(before.get('touches')) is int and before['touches'] == 0
        and type(after.get('touches')) is int and after['touches'] == 1
        and type(after.get('return_events')) is int and after['return_events'] == 0
        and type(before.get('return_events')) is int and before['return_events'] == 0
        and start.get('focused') is False and untouched.get('focused') is False
        and start.get('sha256') == hashlib.sha256(b'untouched').hexdigest()
        and target.get('focused') is True and other.get('focused') is False
        and type(target.get('characters')) is int and target['characters'] == len(text)
        and type(target.get('utf16_length')) is int and target['utf16_length'] == len(text.encode('utf-16-le')) // 2
        and target.get('sha256') == hashlib.sha256(text.encode()).hexdigest()
        and other.get('sha256') == untouched.get('sha256') == hashlib.sha256(b'').hexdigest()
        and type(other.get('characters')) is int and other['characters'] == 0
        and type(other.get('utf16_length')) is int and other['utf16_length'] == 0
        and calls.get('POST /ipu/element/:id/input-target') == 1
        and calls.get('POST /element/:id/clear') == 1
        and calls.get('POST /wda/keys') == 3
        and calls.get('GET /element/:id/value', 0) >= 1
        and calls.get('POST /element/:id/value', 0) == 0
        and calls.get('POST /element/:id/click', 0) == 0)


def mcp_text_action(peer, action):
    assert action.get('type') == 'text'
    arguments = {key: value for key, value in action.items() if key != 'type'}
    arguments['observe'] = False
    reply = peer.request('tools/call', {'name': 'phone_type', 'arguments': arguments}, timeout=60)
    assert 'error' not in reply, 'MCP protocol rejected input; no replay'
    result, _, body = mcp_payload(reply)
    if isinstance(result.get('structuredContent'), dict):
        body = result['structuredContent']
    assert isinstance(body, dict) and type(body.get('ok')) is bool, 'MCP returned no authoritative input outcome; no replay'
    return reply, body


def build(out):
    project = out / 'fixture-src'
    shutil.copytree(ROOT / 'runner/gesture-fixture', project)
    (project / 'main.swift').write_bytes((ROOT / 'scripts/fixtures/state-availability/main.swift').read_bytes())
    pbx = project / 'GestureFixture.xcodeproj/project.pbxproj'
    pbx.write_text(pbx.read_text().replace('com.leeguoo.iphone-use.gesture-fixture', BUNDLE))
    info = project / 'Info.plist'
    data = plistlib.loads(info.read_bytes())
    data.update(CFBundleIdentifier=BUNDLE, CFBundleName='State Oracle', CFBundleDisplayName='State Oracle')
    data['UIApplicationSceneManifest']['UISceneConfigurations']['UIWindowSceneSessionRoleApplication'][0]['UISceneDelegateClassName'] = '$(PRODUCT_MODULE_NAME).StateSceneDelegate'
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('base', 'udid', 'owner', 'product-version'):
        parser.add_argument('--' + name, required=True)
    for name in ('mcp', 'runner-record', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--type-text', action='store_true')
    parser.add_argument('--mcp-input', action='store_true')
    parser.add_argument('--selected-replacement', action='store_true')
    args = parser.parse_args()
    assert not args.mcp_input or args.type_text
    assert not args.selected_replacement or not args.type_text
    uuid.UUID(args.udid)
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    base = urllib.parse.urlparse(args.base)
    assert base.scheme == 'http' and base.hostname in ('127.0.0.1', 'localhost', '::1') and not base.username and not base.password and not base.query and not base.fragment
    daemon = transport.OwnedDaemon(args.base, args.owner)
    daemon.require_simulator(args.udid)
    assert not args.runner_record.is_symlink()
    record = json.loads(args.runner_record.read_text())
    assert record['udid'].lower() == args.udid.lower() and type(record['daemon_port']) is int and base.port == record['daemon_port']
    assert type(record['port']) is int and 1 <= record['port'] <= 65535
    token_path = args.runner_record.parent / 'runner-token'
    assert not token_path.is_symlink() and token_path.is_file() and token_path.resolve().parent == args.runner_record.parent.resolve()
    token = token_path.read_text().strip()
    assert re.fullmatch(r'[0-9a-f]{32,128}', token)
    assert drag.command([args.mcp.resolve().with_name('iphone-use'), '--version']).stdout.strip() == 'iphone-use ' + args.product_version
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), transport.NoRedirect())
    result = {'passed': False, 'cases': [], 'cleanup_errors': [], 'product_acceptance': False,
        'scope': 'owned Simulator UIKit/Native getters plus host HTTP waits; programmatic fixture focus, no HID; not MCP or physical proof',
        'mcp_sha256': hashlib.sha256(args.mcp.read_bytes()).hexdigest()}
    if args.type_text:
        result['scope'] = 'owned Simulator Native keyboard input and host HTTP; independent UIKit text hashes; not physical/MCP/performance proof'
    nonce = str(uuid.uuid4())
    expected_pid = None
    expected_controller = None
    sequence = 0
    data_dir = None
    install_attempted = False
    mcp_peer = None
    allowed_touches = {0}

    def save(name, value):
        p = args.out / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(value, indent=2) + '\n')

    def native(target, body=None):
        assert target.startswith('/element/') or target in ('/elements', '/wda/activeAppInfo')
        raw = b'' if body is None else json.dumps(body).encode()
        method = 'GET' if body is None else 'POST'
        assert method == 'GET' or target == '/elements', 'Native mutations forbidden in state oracle'
        request = urllib.request.Request(f'http://127.0.0.1:{record["port"]}' + target,
            data=None if body is None else raw, method=method,
            headers={'Content-Type': 'application/json', 'Authorization': drag.authorization(token, method, target, raw)})
        try:
            response = opener.open(request, timeout=30)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return {'status': response.status, 'body': json.load(response),
                'coverage': response.headers.get('X-IPU-Tree-Complete')}

    def installed_apps():
        raw = drag.command(['xcrun', 'simctl', 'listapps', args.udid], timeout=30).stdout
        converted = subprocess.run(['plutil', '-convert', 'json', '-o', '-', '--', '-'],
            input=raw, capture_output=True, text=True, timeout=30)
        assert converted.returncode == 0, 'installed App inventory unreadable'
        apps = json.loads(converted.stdout)
        assert isinstance(apps, dict), 'invalid App inventory'
        return apps

    def state(allow_pending=False, after_tick=None):
        deadline = time.monotonic() + 2
        while True:
            value = json.loads((data_dir / 'state-oracle.json').read_text())
            assert type(value.get('tick')) is int and value['tick'] >= 0
            if after_tick is None or value['tick'] > after_tick:
                break
            assert time.monotonic() < deadline, 'App heartbeat stopped; stale state cannot pass'
            time.sleep(0.03)
        assert not state_reasons(value, nonce, expected_pid), 'App identity/foreground changed'
        assert expected_controller is None or value.get('controller_id') == expected_controller, 'App controller recreated'
        assert type(value['command_sequence']) is int and (value['command_sequence'] in (sequence-1, sequence) if allow_pending else value['command_sequence'] == sequence), 'fixture command sequence changed'
        assert type(value['touches']) is int and value['touches'] in allowed_touches, 'unexpected touch'
        return value

    def focus(wanted, caret_utf16=None):
        nonlocal sequence
        sequence += 1
        command = {'run_id': nonce, 'pid': expected_pid, 'sequence': sequence, 'focused': wanted}
        if caret_utf16 is not None:
            command['caret_utf16'] = caret_utf16
        save('focus-commands/' + str(sequence) + '/request.json', command)
        temporary = data_dir / 'state-command.tmp'
        temporary.write_text(json.dumps(command))
        temporary.replace(data_dir / 'state-command.json')
        deadline = time.monotonic() + 5
        samples = []
        while time.monotonic() < deadline:
            value = state(allow_pending=True)
            samples.append(value)
            save('focus-commands/' + str(sequence) + '/samples.json', samples)
            if focus_confirmed(value, sequence, wanted):
                if caret_utf16 is not None:
                    field = value['controls']['state.field']
                    assert field.get('selection_location') == caret_utf16 and field.get('selection_length') == 0, 'fixture caret was not confirmed'
                return value
            receipt = value.get('focus_command')
            if isinstance(receipt, dict) and receipt.get('sequence') == sequence and receipt.get('acknowledged') is False:
                raise RuntimeError('fixture focus API refused the one command; no replay')
            time.sleep(0.05)
        raise RuntimeError('one fixture focus command not confirmed; inspect focus-commands; no replay')

    try:
        assert BUNDLE not in installed_apps(), 'preexisting fixture must not be overwritten'
        app = build(args.out)
        install_attempted = True
        drag.command(['xcrun', 'simctl', 'install', args.udid, app])
        data_dir = Path(drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'data']).stdout.strip()) / 'Documents'
        def sdk(label, argv, timeout):
            return drag.command(argv, timeout=timeout).stdout.strip()
        expected_pid, launch = launch_once(sdk, args.udid, BUNDLE, nonce)
        save('launch.json', launch)
        deadline = time.monotonic() + 60
        while True:
            try:
                ready = json.loads((data_dir / 'state-oracle.json').read_text())
                if not state_reasons(ready, nonce, expected_pid) and ready.get('appeared'):
                    container = Path(drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'app']).stdout.strip())
                    valid, detail = observe_process(sdk, expected_pid, str(container / 'GestureFixture'), nonce, 10)
                    if valid:
                        expected_controller = ready['controller_id']
                        uuid.UUID(expected_controller)
                        save('process.json', {'verified': valid, 'pid': expected_pid, 'nonce': nonce})
                        save('ready.json', ready)
                        break
            except (OSError, ValueError):
                pass
            assert time.monotonic() < deadline, 'one App launch not confirmed; no replay'
            time.sleep(0.1)
        state(after_tick=ready['tick'])
        active = native('/wda/activeAppInfo')
        assert active['body']['value']['bundleId'] == BUNDLE and active['body']['value']['pid'] == expected_pid
        assert set(ready['controls']) == {'state.enabled','state.disabled','state.selected','state.unselected','state.field','state.other'}
        save('active-before.json', active)
        handles = {}
        for identifier in ready['controls']:
            query = native('/elements', {'using': 'accessibility id', 'value': identifier})
            save('lookup/' + identifier + '.json', query)
            assert query['status'] == 200 and query['coverage'] == '1'
            matches = query['body']['value']
            assert len(matches) == 1
            handles[identifier] = matches[0].get('ELEMENT') or matches[0]['element-6066-11e4-a52e-4f735466cecf']
        if args.type_text:
            if args.mcp_input:
                clean = {key: value for key, value in os.environ.items()
                         if not key.startswith(('IPHONE_USE_', 'PHONE_REMOTE_', 'IOS_USE_', 'IPU_', 'TEST_RUNNER_'))
                         and key not in ('GH_TOKEN', 'GITHUB_TOKEN')}
                clean.update(IPHONE_USE_URL=args.base, IPHONE_USE_OWNER=args.owner,
                             IPHONE_USE_MCP_PREWARM='0', IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1')
                mcp_peer = Peer.__new__(Peer)
                Peer.__init__(mcp_peer, [str(args.mcp)], clean, args.out / 'input-mcp.log')
                assert mcp_peer.initialization['result']['serverInfo']['version'] == args.product_version
                result['input_transport'] = 'MCP stdio -> host HTTP -> Native keyboard'
                result['scope'] = 'source-built MCP stdio/HTTP/Native input with independent UIKit text hashes; not physical/published/performance proof'
            focus(True)
            untouched = hashlib.sha256(b'untouched').hexdigest()
            last_tick = state()['tick']

            def inspect_text(expected):
                nonlocal last_tick
                deadline = time.monotonic() + 20
                while True:
                    value = state(after_tick=last_tick)
                    last_tick = value['tick']
                    save('input-last-state.json', value)
                    assert type(value.get('return_events')) is int and value['return_events'] == 0, 'text was interpreted as Return'
                    assert value['controls']['state.other']['sha256'] == untouched, 'input changed another field'
                    if text_state_matches(value, expected):
                        return value
                    assert time.monotonic() < deadline, 'UIKit value did not match the full expected text'
                    time.sleep(0.05)

            def call_text(action, label):
                assert action['type'] == 'text'
                save('input/' + label + '-request.json', action)
                if mcp_peer is not None:
                    reply, response = mcp_text_action(mcp_peer, action)
                    save('input/' + label + '-mcp.json', reply)
                else:
                    try:
                        response = daemon.call('/agent/input', action)
                    except urllib.error.HTTPError as error:
                        response = json.load(error)
                save('input/' + label + '-response.json', response)
                return response

            inspect_text('')
            def complete_text(payload, action, label):
                response = call_text(action, label + '-start')
                offset = 0
                remaining = payload
                seen_tokens = set()
                last_consumed = None
                continuations = 0
                while response.get('ok') is not True:
                    assert response.get('error') == 'text_partially_typed', 'Native input failed; inspect response; no replay'
                    assert type(response.get('characters_confirmed')) is int and 0 <= response['characters_confirmed'] <= len(remaining)
                    assert type(response.get('characters_uncertain')) is int and response['characters_uncertain'] == 0, 'uncertain Native input; no continuation sent'
                    assert response.get('continuation_safe') is True
                    token = response.get('continue_token')
                    assert isinstance(token, str) and re.fullmatch(r'[0-9a-f]{64}', token) and token not in seen_tokens
                    seen_tokens.add(token)
                    offset += response['characters_confirmed']
                    remaining = payload[offset:]
                    assert response.get('remaining_text') == remaining
                    boundary = inspect_text(payload[:offset])
                    save('input/' + label + '-boundary-' + str(continuations) + '.json', boundary)
                    continuations += 1
                    assert continuations <= 5, 'continuation did not make bounded progress; no replay'
                    last_consumed = token
                    response = call_text({'type': 'text', 'continue_token': token}, label + '-continue-' + str(continuations))
                after = inspect_text(payload)
                save('input/' + label + '-complete.json', after)
                return after, continuations, last_consumed

            payload = '字' * 500
            after, continuations, last_consumed = complete_text(payload,
                {'type': 'text', 'text': payload, 'budget_ms': 6000}, 'input500')
            save('input-500-state.json', after)
            replay_checked = False
            if last_consumed is not None:
                rejected = call_text({'type': 'text', 'continue_token': last_consumed}, 'consumed-token')
                assert rejected.get('ok') is False and rejected.get('error') == 'input_continuation_expired'
                save('input-500-after-replay.json', inspect_text(payload))
                replay_checked = True
            result['cases'].append({'name': 'input-500', 'passed': True, 'characters': 500,
                'sha256': hashlib.sha256(payload.encode()).hexdigest(), 'continuation_calls': continuations,
                'continuation_exercised': continuations > 0, 'consumed_token_replay_checked': replay_checked})
            focus(True, caret_utf16=250)
            save('input-middle-caret-before-replace.json', inspect_text(payload))
            literal = 'literal-\ue007-\ue003-👋🏻'
            after, literal_continuations, _ = complete_text(literal,
                {'type': 'text', 'text': literal, 'clear': True}, 'literal')
            save('input-literal-state.json', after)
            result['cases'].append({'name': 'literal-private-unicode', 'passed': True,
                'characters': len(literal), 'sha256': hashlib.sha256(literal.encode()).hexdigest(), 'return_events': 0, 'continuation_calls': literal_continuations, 'replacement_from_middle': True})
        if args.selected_replacement:
            result['scope'] = 'owned Simulator selected snapshot replacement through host HTTP/Native, with independent UIKit values and touch count; not physical/MCP/published proof'
            before = state()
            save('selected/before.json', before)
            seed = daemon.call('/agent/elements?ocr=off')
            save('selected/snapshot.json', seed)
            snapshot = seed.get('snapshot')
            rows = [(row.get('index', index), row) for index, row in enumerate(seed.get('elements', []))
                    if row.get('identifier') == 'state.other']
            assert isinstance(snapshot, str) and snapshot and len(rows) == 1, 'selected snapshot target is ambiguous or missing'
            index, row = rows[0]
            assert type(index) is int and index >= 0 and row.get('kind') == 'TextField'
            payload = '选' * 420 + '-\ue007-\ue003-👋🏻'
            action = {'type': 'set_value', 'element': index, 'snapshot': snapshot, 'value': payload}
            save('selected/request.json', action)
            allowed_touches = {0, 1}
            try:
                response = daemon.call('/agent/input', action)
            except urllib.error.HTTPError as error:
                response = json.load(error)
            save('selected/response.json', response)
            assert response.get('ok') is True, 'selected replacement failed; no replay'
            deadline = time.monotonic() + 20
            while True:
                after = state(after_tick=before['tick'])
                save('selected/after.json', after)
                if selected_replacement_matches(before, after, payload, response):
                    break
                assert time.monotonic() < deadline, 'selected replacement did not match UIKit values/routes; no replay'
                time.sleep(0.05)
            allowed_touches = {1}
            result['cases'].append({'name': 'selected-unfocused-replacement', 'passed': True,
                'characters': len(payload), 'sha256': hashlib.sha256(payload.encode()).hexdigest(),
                'target': 'state.other', 'untouched': 'state.field', 'touches': 1,
                'native_binding_exercised': True, 'legacy_element_value_used': False})
        for name, identifier, field, expected in ([] if args.type_text or args.selected_replacement else CASES):
            if name in ('focus-on', 'focus-off'):
                focus(expected)
            before = state()
            path = '/element/' + urllib.parse.quote(handles[identifier], safe='')
            identity = native(path + '/attribute/identifier')
            assert identity['body']['value'] == identifier
            save(name + '/before.json', before)
            save(name + '/identity.json', identity)
            samples = []
            deadline = time.monotonic() + 5
            while True:
                observed = native(path + '/attribute/' + field)
                after = state(after_tick=before['tick'])
                sample = {'native': observed, 'app': after}
                samples.append(sample)
                save(name + '/samples.json', samples)
                if observed['status'] == 200 and judge(before, after, nonce, expected_pid, identifier, field, expected, observed['body'].get('value')):
                    break
                assert time.monotonic() < deadline, f'{name}: Native state did not match independent UIKit state'
                time.sleep(0.1)
            quiet = []
            last_tick = after['tick']
            for _ in range(3):
                time.sleep(0.1)
                observed = native(path + '/attribute/' + field)
                after = state(after_tick=last_tick)
                last_tick = after['tick']
                quiet.append({'native': observed, 'app': after})
                save(name + '/quiet.json', quiet)
                assert observed['status'] == 200 and judge(before, after, nonce, expected_pid, identifier, field, expected, observed['body'].get('value')), 'settled Native state drifted from UIKit'
            if field in ('enabled', 'focused'):
                try:
                    host_wait = daemon.call('/agent/actions', {'observe': False, 'steps': [
                        {'kind': 'wait_for', 'expect': {'present': [{'identifier': identifier, field: expected}]},
                         'timeout_ms': 1000, 'poll_ms': 100}]})
                except urllib.error.HTTPError as error:
                    host_wait = json.load(error)
                save(name + '/host-wait.json', host_wait)
                assert host_wait.get('ok') is True and type(host_wait.get('applied_actions')) is int and host_wait['applied_actions'] == 0
                try:
                    opposite = daemon.call('/agent/actions', {'observe': False, 'steps': [
                        {'kind': 'wait_for', 'expect': {'present': [{'identifier': identifier, field: not expected}]},
                         'timeout_ms': 0, 'poll_ms': 100}]})
                except urllib.error.HTTPError as error:
                    opposite = json.load(error)
                save(name + '/host-opposite.json', opposite)
                observation = opposite.get('observation', {})
                assert opposite.get('ok') is False and opposite.get('error') == 'expectation_timeout'
                assert type(opposite.get('applied_actions')) is int and opposite['applied_actions'] == 0
                assert observation.get('present_unproven') == [] and observation.get('missing_present') == [0]
                after = state()
                save(name + '/host-after.json', after)
                assert judge(before, after, nonce, expected_pid, identifier, field, expected, expected), 'host read changed App state'
            if field in ('enabled', 'selected'):
                direct = native(path + '/' + field)
                save(name + '/direct.json', direct)
                assert direct['status'] == 200 and type(direct['body'].get('value')) is bool and direct['body']['value'] is expected
            save(name + '/before.json', before)
            save(name + '/samples.json', samples)
            save(name + '/identity.json', identity)
            result['cases'].append({'name': name, 'passed': True, 'value': expected, 'identifier': identifier, 'field': field})
        active = native('/wda/activeAppInfo')
        assert active['body']['value']['bundleId'] == BUNDLE and active['body']['value']['pid'] == expected_pid
        save('active-after.json', active)
        result['passed'] = True
    except Exception as error:
        result['error'] = type(error).__name__ + ': ' + str(error)
        raise
    finally:
        if mcp_peer is not None and hasattr(mcp_peer, 'process'):
            try:
                mcp_peer.close()
                result['mcp_process_stopped'] = mcp_peer.process.poll() is not None
                assert result['mcp_process_stopped']
            except Exception as error:
                result['cleanup_errors'].append('MCP cleanup: ' + type(error).__name__ + ': ' + str(error))
        # Preserve the actual fixture response before uninstall removes its files.
        if install_attempted and data_dir is not None:
            try:
                save('app-state-before-cleanup.json', json.loads((data_dir / 'state-oracle.json').read_text()))
            except (OSError, ValueError) as error:
                result['final_state_capture_error'] = type(error).__name__ + ': ' + str(error)
        if install_attempted:
            try:
                result['fixture_uninstall_attempts'] = 0
                if BUNDLE in installed_apps():
                    result['fixture_uninstall_attempts'] = 1
                    try:
                        drag.command(['xcrun', 'simctl', 'uninstall', args.udid, BUNDLE])
                    except (RuntimeError, subprocess.TimeoutExpired) as error:
                        result['uninstall_ack_error'] = str(error)
                deadline = time.monotonic() + 30
                while BUNDLE in installed_apps():
                    assert time.monotonic() < deadline, 'fixture removal unconfirmed; no replay'
                    time.sleep(0.3)
                result['fixture_absent_after_cleanup'] = True
            except Exception as error:
                result['cleanup_errors'].append(str(error))
        if result['cleanup_errors']:
            result['passed'] = False
        save('result.json', result)
        if result['cleanup_errors']:
            raise RuntimeError('fixture cleanup unverified; inspect result; no replay')


if __name__ == '__main__':
    main()
