#!/usr/bin/env python3
"""Real MCP against loopback inventory fixtures; xcrun is a recording fake."""
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

if len(sys.argv)<2:raise SystemExit('usage: test-mcp-compat-target.py MCP_BINARY')
BINARY=Path(sys.argv.pop(1)).resolve(strict=True)
FAKE='''import json,os,sys
from pathlib import Path
args=sys.argv[1:]
with open(os.environ['FAKE_XCRUN_LOG'],'a') as out:out.write(json.dumps(args)+'\\n')
assert args[:3]==['devicectl','device','info'],args
path=Path(args[args.index('--json-output')+1])
body={'result':{'apps':[{'bundleIdentifier':'com.example.fixture','name':'Fixture','version':'1.0','defaultApp':False}]}} if args[3]=='apps' else {'result':{'deviceProperties':{'osVersionNumber':'27.0'}}}
path.write_text(json.dumps(body))
'''

class CompatTargetTests(unittest.TestCase):
    def exercise(self,simulator,inventory_available):
        calls=[]
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*_):pass
            def do_POST(self):
                calls.append(('POST',self.path));self.send_error(500)
            def do_GET(self):
                calls.append(('GET',self.path))
                code=200
                if self.path.startswith('/agent/elements'):
                    body={'snapshot':'fixture-snapshot','elements':[{'kind':'Button','label':'Fixture','rect':[0,0,20,20]}],
                          'registry':{'flows':[{'id':'fixture'}]}}
                elif self.path=='/agent/apps':
                    if inventory_available:body={'ok':True,'source':'simctl','device':{'ios':'27.0'},'apps':[{'bundle':'com.example.fixture','name':'Fixture','version':'1.0','system':False}]}
                    else:code,body=503,{'ok':False,'error':'inventory_unavailable'}
                elif self.path=='/agent/status':
                    body={'ok':True,'udid':'00000000-0000-0000-0000-000000000000','version':'fixture','drivable':True,'runner':True,'released':False,'backend':'direct','mode':'agent'}
                    if simulator:body['target']='simulator'
                else:code,body=404,{'ok':False}
                raw=json.dumps(body).encode();self.send_response(code);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
        with tempfile.TemporaryDirectory(prefix='mcp-compat-') as folder,ThreadingHTTPServer(('127.0.0.1',0),Handler) as server:
            root=Path(folder);fake=root/'xcrun'
            fake.write_text('#!'+sys.executable+'\n'+FAKE);fake.chmod(0o700)
            probe=root/'xcrun-calls.jsonl'
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            env={k:v for k,v in os.environ.items() if not k.startswith(('IPHONE_USE_','PHONE_REMOTE_','IPU_'))}
            env.update(PATH=str(root)+os.pathsep+os.environ.get('PATH',''),FAKE_XCRUN_LOG=str(probe),
                       IPHONE_USE_URL=f'http://127.0.0.1:{server.server_port}',IPHONE_USE_MCP_PREWARM='0',IPHONE_USE_MCP_RECONNECT_ON_USE='0',
                       IPHONE_USE_MCP_TOOLS='all',IPHONE_USE_MCP_STRUCTURED='full',IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1',
                       IPHONE_USE_FLOWS_DIR=str(root/'flows'),IPHONE_USE_STATE_DIR=str(root/'state'),RUST_LOG='error')
            with (root/'stderr.log').open('wb') as log:
                process=subprocess.Popen([str(BINARY)],env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,start_new_session=True)
            reader=selectors.DefaultSelector();reader.register(process.stdout,selectors.EVENT_READ);buffer=b''
            def rpc(serial,method,params):
                nonlocal buffer
                process.stdin.write((json.dumps({'jsonrpc':'2.0','id':serial,'method':method,'params':params})+'\n').encode());process.stdin.flush()
                until=time.monotonic()+15
                while time.monotonic()<until:
                    while b'\n' in buffer:
                        line,buffer=buffer.split(b'\n',1);reply=json.loads(line)
                        if reply.get('id')==serial:return reply
                    if reader.select(max(0,until-time.monotonic())):
                        data=os.read(process.stdout.fileno(),65536)
                        if not data:raise AssertionError('fixture MCP exited')
                        buffer+=data
                raise AssertionError('fixture MCP response deadline')
            try:
                rpc(1,'initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'compat-target-fixture','version':'1'}})
                process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n');process.stdin.flush()
                reply=rpc(2,'tools/call',{'name':'phone_elements','arguments':{}})
                self.assertNotIn('error',reply,reply)
                self.assertIsNot(reply['result'].get('isError'),True,reply)
                invocations=[json.loads(line) for line in probe.read_text().splitlines()] if probe.exists() else []
            finally:
                if process.poll() is None:os.killpg(process.pid,signal.SIGTERM)
                process.wait(timeout=5);process.stdin.close();process.stdout.close();reader.close()
                server.shutdown();thread.join()
        self.assertFalse(any(method=='POST' for method,_ in calls),calls)
        self.assertIn(('GET','/agent/apps'),calls)
        return calls,invocations

    def test_simulator_inventory_failure_never_queries_a_physical_device(self):
        _,invocations=self.exercise(True,False)
        self.assertEqual(invocations,[],'simulator ID was passed to physical devicectl fallback')

    def test_physical_inventory_failure_keeps_the_same_device_fallback(self):
        _,invocations=self.exercise(False,False)
        self.assertEqual(len(invocations),2)
        for args in invocations:self.assertEqual(args[args.index('--device')+1],'00000000-0000-0000-0000-000000000000')

    def test_simulator_inventory_still_uses_the_daemon(self):
        calls,invocations=self.exercise(True,True)
        self.assertEqual(invocations,[])
        self.assertNotIn(('GET','/agent/status'),calls)

if __name__=='__main__':unittest.main()
