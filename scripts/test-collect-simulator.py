#!/usr/bin/env python3
"""Compare Native list collection to an App-owned model on an explicit owned Simulator."""
import argparse
from collections import Counter
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

from mcp_probe_peer import Peer, payload

ROOT = Path(__file__).resolve().parents[1]
BUNDLE = 'com.leeguoo.iphone-use.list-oracle'
MODES = ('unique', 'missing', 'reused', 'counter')
spec = importlib.util.spec_from_file_location('list_transport', ROOT / 'scripts/test-drag-simulator.py')
drag = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drag)


def row_key(row):
    return (row.get('identifier') or None, row.get('label'), row.get('value'))


def fixture_model(mode):
    return [{'index': index,
             'identifier': None if mode == 'missing' else ('shared-row' if mode == 'reused' else f'row.{index:03d}'),
             'label': f'Item {index:03d}' if mode == 'reused' else 'Same item',
             'value': 'before' if mode == 'counter' else 'stable'}
            for index in range(8 if mode == 'counter' else 32)]


def verify_case(oracle, before, after, result, last_tree, seed_tree=None):
    """A Native ACK alone cannot satisfy model coverage or UIKit movement."""
    errors = []
    mode, nonce = oracle.get('mode'), oracle.get('run_id')
    if mode not in MODES or not nonce:
        return ['invalid fixture oracle']
    for state in (before, after):
        if state.get('run_id') != nonce or state.get('mode') != mode:
            errors.append('stale fixture state')
    if before.get('pid') != after.get('pid') or not before.get('pid'):
        errors.append('fixture process changed')
    rows, expected = result.get('rows'), fixture_model(mode)
    if oracle.get('rows') != expected:
        errors.append('fixture oracle differs from declared model')
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        return errors + ['missing collected rows']
    pages = result.get('pages')
    if not isinstance(pages, list) or not all(isinstance(page, dict) for page in pages):
        return errors + ['missing page receipts']
    if result.get('swipes') != result.get('swipes_confirmed') or len(pages) != result.get('swipes_confirmed', -2) + 1:
        errors.append('swipe and page receipts disagree')
    if any(type(row.get('page')) is not int or not 1 <= row['page'] <= len(pages) for row in rows):
        errors.append('row refers to an invalid page')
    if result.get('ok') is not True or result.get('observation_omitted') or result.get('stop_error'):
        errors.append('collection refused, failed or withheld observations')
    if result.get('swipes_uncertain') != 0 or type(result.get('swipes_confirmed')) is not int or result['swipes_confirmed'] < 1:
        errors.append('no confirmed swipe or delivery remains uncertain')
    if result.get('row_count') != len(rows) or len(result.get('pages', [])) < 2:
        errors.append('collection did not cover multiple reads')
    if not result.get('snapshot') or result['snapshot'] != last_tree.get('snapshot'):
        errors.append('returned snapshot differs from final page')
    if mode in ('unique', 'reused'):
        if Counter(map(row_key, rows)) != Counter(map(row_key, expected)):
            errors.append('collected rows differ from independent App model')
        if result.get('complete') is not True or result.get('end_label_seen') is not True or result.get('stop_reason') != 'end_label':
            errors.append('did not reach the independent model end')
        if after.get('max_offset_y', 0) < 100 or after.get('scroll_events', 0) < 1:
            errors.append('UIKit did not observe list movement')
        if set(after.get('seen_indices', [])) != set(range(len(expected))):
            errors.append('UIKit did not expose every model index')
    elif mode == 'missing':
        if len(rows) != 1 or result.get('complete') is not False or result.get('stop_reason') not in ('duplicate_page', 'no_progress'):
            errors.append('missing identifiers falsely imply business completeness')
        if rows and row_key(rows[0]) != row_key(expected[0]):
            errors.append('fallback row differs from model')
        if after.get('max_offset_y', 0) < 100:
            errors.append('fallback did not exercise real scrolling')
    else:
        if Counter(map(row_key, rows)) != Counter(map(row_key, expected)):
            errors.append('counter refresh changed or duplicated first-observed rows')
        if result.get('complete') is not False or result.get('stop_reason') != 'duplicate_page':
            errors.append('in-place value refresh was treated as another page')
        if after.get('counter_changed') is not True or after.get('pan_events', 0) < 1 or abs(after.get('offset_y', 999)) > 1 or after.get('max_abs_offset_y', 999) > 1:
            errors.append('UIKit did not observe a value-only refresh')
        seed_cells = [row for row in (seed_tree or {}).get('elements', []) if row.get('kind') == 'Cell']
        last_cells = [row for row in last_tree.get('elements', []) if row.get('kind') == 'Cell']
        if Counter(map(row_key, seed_cells)) != Counter(map(row_key, expected)):
            errors.append('Native seed did not expose every before value')
        changed = [{**row, 'value': 'after'} for row in expected]
        if Counter(map(row_key, last_cells)) != Counter(map(row_key, changed)):
            errors.append('Native final tree did not expose every after value')
    return errors


def transport_errors(*, http_status=None, mcp_reply=None):
    if http_status is not None and not 200 <= http_status < 300:
        return ['HTTP collection failed']
    if mcp_reply is not None:
        result = mcp_reply.get('result')
        if 'error' in mcp_reply or not isinstance(result, dict) or result.get('isError') is True:
            return ['MCP collection failed']
    return []


def native_ids(value):
    if isinstance(value, dict):
        found = {value['rawIdentifier']} if value.get('type') == 'XCUIElementTypeCell' and value.get('rawIdentifier') else set()
        for child in value.values():
            found.update(native_ids(child))
        return found
    if isinstance(value, list):
        return set().union(*(native_ids(child) for child in value))
    return set()


def build_fixture(out):
    fixture_root = out / 'fixture-src'
    project = fixture_root / 'runner/gesture-fixture'
    shutil.copytree(ROOT / 'runner/gesture-fixture', project)
    (project / 'main.swift').write_bytes((ROOT / 'scripts/fixtures/list-collection/main.swift').read_bytes())
    pbx = project / 'GestureFixture.xcodeproj/project.pbxproj'
    pbx.write_text(pbx.read_text().replace('com.leeguoo.iphone-use.gesture-fixture', BUNDLE))
    info = project / 'Info.plist'
    metadata = plistlib.loads(info.read_bytes())
    metadata.update(CFBundleIdentifier=BUNDLE, CFBundleName='List Oracle', CFBundleDisplayName='List Oracle', CFBundleVersion='3')
    metadata['UIApplicationSceneManifest'] = {'UIApplicationSupportsMultipleScenes': False, 'UISceneConfigurations': {
        'UIWindowSceneSessionRoleApplication': [{'UISceneConfigurationName': 'List Oracle',
            'UISceneDelegateClassName': '$(PRODUCT_MODULE_NAME).ListSceneDelegate'}]}}
    info.write_bytes(plistlib.dumps(metadata))
    previous = drag.ROOT
    try:
        drag.ROOT = fixture_root
        app = drag.build_fixture(out)
    finally:
        drag.ROOT = previous
    built = plistlib.loads((app / 'Info.plist').read_bytes())
    assert built['CFBundleIdentifier'] == BUNDLE and built['CFBundleVersion'] == '3'
    assert built['UIApplicationSceneManifest']['UISceneConfigurations']['UIWindowSceneSessionRoleApplication'][0]['UISceneDelegateClassName'] == 'GestureFixture.ListSceneDelegate'
    return app


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('base', 'udid', 'owner', 'product-version'):
        parser.add_argument('--' + name, required=True)
    for name in ('mcp', 'runner-record', 'out'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    args.out = args.out.resolve()
    args.out.mkdir(mode=0o700, parents=True, exist_ok=False)
    urllib.request.install_opener(urllib.request.build_opener(urllib.request.ProxyHandler({})))
    daemon = drag.Daemon(args.base, args.owner)
    daemon.require_simulator(args.udid)
    record = json.loads(args.runner_record.read_text())
    assert record['udid'].lower() == args.udid.lower() and record['daemon_port'] == urllib.parse.urlsplit(args.base).port
    devices = json.loads(drag.command(['xcrun', 'simctl', 'list', 'devices', '--json']).stdout)
    assert any(d['udid'].lower() == args.udid.lower() and d['state'] == 'Booted' for group in devices['devices'].values() for d in group)
    assert drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'data'], check=False).returncode != 0, 'fixture already installed; nothing overwritten'
    app = build_fixture(args.out)
    result = {'scope': 'owned Simulator Native list conformance', 'passed': False, 'cases': [],
              'binary_sha256': hashlib.sha256(args.mcp.read_bytes()).hexdigest(),
              'fixture_sha256': hashlib.sha256((args.out / 'fixture-src/runner/gesture-fixture/main.swift').read_bytes()).hexdigest()}
    installed = False
    peer = None
    cleanup = []
    try:
        daemon.require_simulator(args.udid)
        drag.command(['xcrun', 'simctl', 'install', args.udid, app]); installed = True
        home = args.out / 'mcp-home'; home.mkdir(mode=0o700)
        env = {k: v for k, v in os.environ.items() if not k.startswith(('IPHONE_USE_', 'PHONE_REMOTE_', 'IPU_', 'TEST_RUNNER_'))}
        env.update(HOME=str(home), IPHONE_USE_URL=args.base, IPHONE_USE_OWNER=args.owner,
                   IPHONE_USE_MCP_PREWARM='0', IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1', IPHONE_USE_MCP_STRUCTURED='full')
        peer = Peer([str(args.mcp.resolve())], env, args.out / 'mcp.log')
        server = peer.initialization.get('result', {}).get('serverInfo', {})
        result['mcp_server'] = server
        assert server.get('version') == args.product_version, 'MCP and product versions differ; no collection sent'
        for mode in MODES:
            case = args.out / mode; case.mkdir()
            nonce = str(uuid.uuid4())
            if result['cases']:
                drag.command(['xcrun', 'simctl', 'terminate', args.udid, BUNDLE], timeout=30)
            daemon.require_simulator(args.udid)
            drag.command(['xcrun', 'simctl', 'launch', args.udid, BUNDLE, '--run-id', nonce, '--mode', mode])
            data = Path(drag.command(['xcrun', 'simctl', 'get_app_container', args.udid, BUNDLE, 'data']).stdout.strip()) / 'Documents'
            deadline = time.monotonic() + 20
            while True:
                try:
                    before = json.loads((data / 'list-state.json').read_text())
                    oracle = json.loads((data / 'list-oracle.json').read_text())
                    if before['run_id'] == oracle['run_id'] == nonce and before['visible_indices'] and before['width'] > 0:
                        break
                except (OSError, ValueError, KeyError):
                    pass
                assert time.monotonic() < deadline, 'fixture did not publish fresh ready state'
                time.sleep(.1)
            (case / 'oracle.json').write_text(json.dumps(oracle, indent=2))
            (case / 'before.json').write_text(json.dumps(before, indent=2))
            active, _ = drag.runner_call(args, '/wda/activeAppInfo')
            assert active['value']['bundleId'] == BUNDLE and active['value']['pid'] == before['pid'], 'fixture not foreground'
            size, _ = drag.runner_call(args, '/window/size')
            drag.require_matching_viewport(before, size)
            seed = daemon.call('/agent/elements?ocr=off')
            raw, _ = drag.runner_call(args, '/source?format=json&max_depth=12&max_nodes=300&extension_calls=0')
            (case / 'raw-source.json').write_text(json.dumps(raw, indent=2))
            exposed = {r.get('identifier') for r in seed['elements'] if r.get('kind') == 'Cell' and r.get('identifier')}
            if mode != 'missing':
                assert exposed and exposed <= {r['identifier'] for r in oracle['rows']}, 'Native identifiers differ from model'
                assert exposed <= native_ids(raw), 'rawIdentifier was lost in Native source'
            (case / 'seed.json').write_text(json.dumps(seed, indent=2))
            parameters = {'max_pages': 4 if mode in ('missing', 'counter') else 10, 'snapshot': seed['snapshot']}
            if mode != 'counter':
                parameters['end_label'] = 'END OF LIST ORACLE'
            started = time.monotonic()
            if mode == 'unique':
                reply = peer.request('tools/call', {'name': 'phone_collect_list', 'arguments': parameters}, timeout=110)
                (case / 'mcp-response.json').write_text(json.dumps(reply, indent=2))
                assert not transport_errors(mcp_reply=reply), 'MCP collection failed; inspect response, no replay'
                _, _, collection = payload(reply)
            else:
                request = urllib.request.Request(args.base + '/agent/collect', data=json.dumps(parameters).encode(),
                    headers={'Content-Type': 'application/json', 'X-Phone-Control': '1', 'X-Phone-Owner': args.owner})
                try:
                    response = urllib.request.urlopen(request, timeout=100)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    collection = json.load(response)
                    http_status = response.status
                (case / 'http-response.json').write_text(json.dumps({'status': http_status, 'body': collection}, indent=2))
                assert not transport_errors(http_status=http_status), 'HTTP collection failed; inspect response, no replay'
            elapsed = time.monotonic() - started
            (case / 'collection.json').write_text(json.dumps(collection, indent=2))
            after = json.loads((data / 'list-state.json').read_text())
            (case / 'after.json').write_text(json.dumps(after, indent=2))
            active, _ = drag.runner_call(args, '/wda/activeAppInfo')
            assert active['value']['bundleId'] == BUNDLE and active['value']['pid'] == before['pid']
            last = daemon.call('/agent/elements?ocr=off')
            (case / 'last-tree.json').write_text(json.dumps(last, indent=2))
            raw_last, _ = drag.runner_call(args, '/source?format=json&max_depth=12&max_nodes=300&extension_calls=0')
            (case / 'raw-last-source.json').write_text(json.dumps(raw_last, indent=2))
            last_exposed = {r.get('identifier') for r in last['elements'] if r.get('kind') == 'Cell' and r.get('identifier')}
            if mode != 'missing':
                assert last_exposed and last_exposed <= native_ids(raw_last), 'final Native rawIdentifier propagation failed'
            if mode == 'unique':
                assert 'row.031' in last_exposed and last_exposed != exposed, 'Native never exposed a new final page'
            errors = verify_case(oracle, before, after, collection, last, seed)
            result['cases'].append({'mode': mode, 'run_id': nonce, 'elapsed_secs': elapsed,
                'rows': collection.get('row_count'), 'stop': collection.get('stop_reason'), 'errors': errors, 'passed': not errors})
            assert not errors, '; '.join(errors)
        result['passed'] = True
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        if peer:
            peer.close()
        try:
            until = time.monotonic() + 30
            while True:
                status, _ = drag.runner_call(args, '/status')
                if status.get('value', {}).get('busy') is False:
                    break
                assert time.monotonic() < until, 'Native work still pending; no fixture uninstall'
                time.sleep(.2)
            if installed:
                drag.command(['xcrun', 'simctl', 'uninstall', args.udid, BUNDLE])
        except Exception as error:
            cleanup.append(str(error))
        result['cleanup_errors'] = cleanup
        if cleanup:
            result['passed'] = False
        (args.out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result), flush=True)
        if cleanup:
            raise RuntimeError('; '.join(cleanup))


if __name__ == '__main__':
    main()
