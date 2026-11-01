#!/usr/bin/env python3
"""Real stdio MCP routing against two isolated mock daemons; never opens a phone."""
import http.server
import json
import os
from pathlib import Path
import plistlib
import selectors
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit, parse_qs

binary = str(Path(sys.argv[1]).resolve())
seen = []
seen_lock = threading.Lock()
servers = []

class Daemon(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.respond()

    def respond(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        with seen_lock:
            seen.append((self.server.name, self.command, self.path, self.headers.get('Authorization'), body, self.headers.get('X-Phone-Control')))
        if self.path.startswith('/agent/metrics'):
            owner = parse_qs(urlsplit(self.path).query).get('owner', [''])[0]
            value = {'ok':True, 'open':[{'key':{'kind':'inferred','owner':owner,'incarnation':1},
                'started_ms':1,'ended_ms':2,'tool_calls':2,'dropped_events':1,
                'outcome_unknown':1,'model_round_trips':None}], 'recent':[]}
        elif self.path.startswith('/agent/capabilities'):
            value = {'ok': True, 'operations': {'single': ['tap', 'type'],
                'batch': {'max': 24}}, 'limits': {'nested': {'value': 'x' * 300}}}
        elif self.path.startswith('/agent/value'):
            text = 'value-' + self.server.name
            value = {'ok': True, 'value': text, 'source': 'ax_value_attribute',
                'utf16_length': len(text), 'returned_utf16_length': len(text),
                'limit_utf16': 8192, 'complete': True, 'truncated': False}
        elif self.path.startswith('/agent/elements'):
            value = {'ok': True, 'snapshot': self.server.name, 'elements': []}
        elif self.command == 'POST' and self.path.startswith('/agent/input') and json.loads(body).get('name') == 'save_screenshot':
            value = {'ok': True, 'system_capture': {'requested': True, 'saved_verified': False, 'destination': 'iPhone Photos'}}
        elif self.command == 'POST':
            value = {'ok': True, 'snapshot': self.server.name + '2', 'elements': []}
        else:
            value = {'ok': True, 'runner': True, 'drivable': True, 'released': False,
                     'device': {'name': self.server.name},
                     'target_configured': self.server.name != 'B',
                     'recovery_owner': 'unconfigured' if self.server.name == 'B' else 'daemon',
                     'recovery': {'cause': 'target_unconfigured', 'actor': 'agent'} if self.server.name == 'B' else None}
        encoded = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

for name in ('A', 'B', 'unexpected-proxy'):
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Daemon)
    server.daemon_threads = True
    server.name = name
    threading.Thread(target=server.serve_forever, daemon=True).start()
    servers.append(server)

try:
    with tempfile.TemporaryDirectory(prefix='ipu-mcp-instances-') as home:
        agents = Path(home) / 'Library/LaunchAgents'
        agents.mkdir(parents=True)
        def config(name, server, token):
            label = 'com.leeguoo.iphone-use' + ('' if name == 'default' else '.' + name)
            path = agents / (label + '.plist')
            path.write_bytes(plistlib.dumps({'Label': label, 'EnvironmentVariables': {
                'IPHONE_USE_PORT': str(server.server_port), 'IPHONE_USE_AGENT_TOKEN': token,
                'IPHONE_USE_PASSWORD': 'must-not-use-password',
                'IPHONE_USE_URL': 'https://must-not-contact.invalid'}}))
            path.chmod(0o600)
            return path
        config('default', servers[0], 'token-A')
        path_b = config('i13', servers[1], 'token-B')
        env = {'HOME': home, 'PATH': os.environ['PATH'], 'NO_PROXY': '', 'no_proxy': '', 'HTTP_PROXY': f'http://127.0.0.1:{servers[2].server_port}', 'http_proxy': f'http://127.0.0.1:{servers[2].server_port}', 'HTTPS_PROXY': f'http://127.0.0.1:{servers[2].server_port}',
               'IPHONE_USE_URL': f'http://127.0.0.1:{servers[0].server_port}',
               'IPHONE_USE_TOKEN': 'ambient-token', 'IPHONE_USE_MCP_TOOLS': 'all',
               'IPHONE_USE_MCP_PREWARM': '0'}
        process = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, env=env)
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        buffer = b''
        next_id = 0
        def rpc(method, params=None):
            global buffer, next_id
            next_id += 1
            process.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': next_id,
                'method': method, 'params': params or {}}) + '\n').encode())
            process.stdin.flush()
            until = time.monotonic() + 15
            while time.monotonic() < until:
                while b'\n' in buffer:
                    line, buffer = buffer.split(b'\n', 1)
                    if line.strip():
                        message = json.loads(line)
                        if message.get('id') == next_id:
                            return message
                if selector.select(min(0.2, max(0, until - time.monotonic()))):
                    chunk = os.read(process.stdout.fileno(), 65536)
                    assert chunk, 'MCP exited before response'
                    buffer += chunk
            raise AssertionError('bounded MCP request did not return')
        def call(tool_name, **args):
            return rpc('tools/call', {'name': tool_name, 'arguments': args})
        try:
            assert 'result' in rpc('initialize', {'protocolVersion': '2025-03-26',
                'capabilities': {}, 'clientInfo': {'name': 'instance-routing-test', 'version': '1'}})
            process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            process.stdin.flush()
            tools = rpc('tools/list')['result']['tools']
            expected = json.loads((Path(__file__).resolve().parent / 'fixtures/mcp-tools.json').read_text())
            assert sorted(t['name'] for t in tools) == expected
            assert all('instance' in t['inputSchema'].get('properties', {})
                       for t in tools if t['name'] != 'phone_instances')
            inventory = call('phone_instances')['result']['structuredContent']
            assert inventory['scope'] == 'installed_daemon_instances'
            assert inventory['paired_device_discovery_command'] == 'xcrun devicectl list devices'
            catalog = inventory['instances']
            assert {r['instance'] for r in catalog} == {'default', 'i13'}
            assert all(r['available'] for r in catalog)
            unconfigured = next(r for r in catalog if r['instance'] == 'i13')
            assert unconfigured['target_configured'] is False
            assert unconfigured['recovery_owner'] == 'unconfigured'
            assert unconfigured['recovery']['cause'] == 'target_unconfigured'
            assert not any(r[1] == 'POST' for r in seen), 'inventory must stay read-only'
            # No explicit instance keeps startup configuration and ambient credentials.
            assert 'result' in call('phone_status')
            assert seen[-1][0] == 'A' and seen[-1][3] == 'Bearer ambient-token'
            for instance, marker, token in [('i13', 'B', 'token-B'), ('default', 'A', 'token-A')]:
                result = call('phone_status', instance=instance)['result']
                assert result['_meta']['iphone-use/instance'] == instance
                assert result['_meta']['openai/widgetSessionId'].endswith(instance)
                assert seen[-1][0] == marker and seen[-1][3] == 'Bearer ' + token
                assert 'result' in call('phone_elements', instance=instance)
            for instance, marker, token in [('i13','B','token-B'),('default','A','token-A')]:
                reply = call('phone_read_value', instance=instance, identifier='field-a')['result']
                assert not reply.get('isError'), reply
                decoded = next(json.loads(item['text']) for item in reply['content']
                    if item.get('type') == 'text' and item.get('text','').startswith('{'))
                assert decoded['value'] == 'value-' + marker
                assert seen[-1][0] == marker and seen[-1][3] == 'Bearer ' + token
                assert seen[-1][1] == 'POST' and seen[-1][5] is None, 'read must not claim control'
            capabilities = call('phone_capabilities', instance='i13')['result']
            assert capabilities['structuredContent'] == {'ok': True,
                'operations': {'single': ['tap', 'type'], 'batch': {'max': 24}},
                'limits': {'nested': {'value': 'x' * 300}}}, 'named capabilities must preserve the full nested contract'
            for instance, marker, token in [('i13','B','token-B'),('default','A','token-A')]:
                reply = call('phone_metrics', instance=instance, limit=1)['result']
                assert reply.get('isError') is not True, reply
                request = seen[-1]
                assert request[0] == marker and request[3] == 'Bearer ' + token
                assert request[1] == 'GET' and urlsplit(request[2]).path == '/agent/metrics'
                assert request[5] is None, 'metrics must not claim control'
                report = reply['structuredContent']
                assert report['owner'] == parse_qs(urlsplit(request[2]).query)['owner'][0]
                assert report['open'][0]['outcome_unknown'] == 1
                assert report['open'][0]['model_round_trips'] is None
            # Interleaved reads must keep separate observation baselines.
            for instance, marker in [('i13', 'B'), ('default', 'A')]:
                response = call('phone_tap', instance=instance, x=0.5, y=0.5, observe=True)
                assert not response.get('error'), response
                requests = [r for r in seen if r[0] == marker and r[1] == 'POST']
                assert requests
                assert parse_qs(urlsplit(requests[-1][2]).query).get('since') == [marker]
            for instance, marker, token in [('i13','B','token-B'),('default','A','token-A')]:
                for name in ['volume_up', 'volume_down']:
                    response = call('phone_shortcut', instance=instance, name=name, observe=False)
                    assert not response.get('error'), response
                    request = seen[-1]
                    assert request[0] == marker and request[3] == 'Bearer ' + token
                    assert request[1] == 'POST' and urlsplit(request[2]).path == '/agent/input'
                    body = json.loads(request[4])
                    assert body['type'] == 'shortcut' and body['name'] == name
                    assert request[5], 'button mutation must claim the selected instance control'
                    response = call('phone_run_steps', instance=instance,
                        steps=[{'kind': 'shortcut', 'name': name}], observe=False)
                    assert not response.get('error'), response
                    request = seen[-1]
                    assert request[0] == marker and request[3] == 'Bearer ' + token
                    assert request[1] == 'POST' and urlsplit(request[2]).path == '/agent/actions'
                    body = json.loads(request[4])
                    assert body['steps'][0]['action']['name'] == name
                    assert request[5], 'batch must claim only the selected instance control'

            # The new Photos mutation follows the selected instance and token,
            # including with observe:false (no ambient/default fallback).
            for instance, marker, token in [('i13', 'B', 'token-B'), ('default', 'A', 'token-A')]:
                before = len(seen)
                response = call('phone_shortcut', instance=instance, name='save_screenshot', observe=False)['result']
                assert response.get('isError') is not True, response
                assert response['structuredContent']['system_capture']['saved_verified'] is False
                calls = seen[before:]
                assert calls and all(row[0] == marker and row[3] == 'Bearer ' + token for row in calls)
                posts = [row for row in calls if row[1] == 'POST']
                assert len(posts) == 1 and posts[0][5] == '1'
                assert json.loads(posts[0][4]) == {'type': 'shortcut', 'name': 'save_screenshot'}
            # Missing, malformed and symlinked configuration must not touch any daemon.
            count = len(seen)
            for instance in ['missing', '../i13', 'runner']:
                assert 'error' in call('phone_tap', instance=instance, x=0.1, y=0.1)
            path_b.unlink()
            path_b.symlink_to(agents / 'com.leeguoo.iphone-use.plist')
            assert 'error' in call('phone_status', instance='i13')
            assert len(seen) == count, 'unsafe selection fell back or sent a request'
            path_b.unlink()
            config('i13', servers[1], 'rotated-B')
            assert 'result' in call('phone_status', instance='i13')
            assert seen[-1][0] == 'B' and seen[-1][3] == 'Bearer rotated-B'
            assert not any(row[0] == 'unexpected-proxy' for row in seen), 'a local daemon request reached the environment proxy'
            print('MCP instances: discovery, credential isolation, snapshots, fail-closed routing and token rotation passed')
        finally:
            process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            selector.close()
finally:
    for server in servers:
        server.shutdown()
        server.server_close()
