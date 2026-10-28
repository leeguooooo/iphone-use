#!/usr/bin/env python3
"""Exercise real stdio handles without an iPhone, daemon or model process."""
import importlib.util
import json
from pathlib import Path
import stat
import sys
import tempfile
import unittest

HERE=Path(__file__).resolve().parent
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('owned_protocol',HERE/'mcp_probe_peer.py')
protocol=importlib.util.module_from_spec(spec);spec.loader.exec_module(protocol)
FAKE='''import json,sys,time
from pathlib import Path
for line in sys.stdin:
 request=json.loads(line)
 if 'id' not in request:continue
 name=request.get('params',{}).get('name')
 if name=='delayed':
  deadline=time.monotonic()+5
  while not (Path(sys.argv[1])/'release').exists() and time.monotonic()<deadline:time.sleep(.01)
 if name=='eof':sys.exit(0)
 print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'content':[{'type':'text','text':'PRIVATE-RESPONSE'}]}}),flush=True)
'''

class PeerTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='owned-peer-')
        self.root=Path(self.temp.name)
        fake=self.root/'fake.py';fake.write_text(FAKE)
        self.peer=protocol.Peer([sys.executable,str(fake),str(self.root)],{},self.root/'mcp.log')

    def tearDown(self):
        (self.root/'release').touch()
        self.peer.close()
        self.temp.cleanup()

    def events(self):
        return [json.loads(line) for line in self.peer.events.read_text().splitlines()]

    def test_journal_records_stage_and_identity_without_arguments_or_response_content(self):
        reply=self.peer.request('tools/call',{'name':'phone_type','arguments':{'text':'PRIVATE-ARGUMENT','continue_token':'b'*64}})
        self.assertIn('PRIVATE-RESPONSE',json.dumps(reply))
        events=[v for v in self.events() if v['id']==2]
        self.assertEqual([v['event'] for v in events],['sending','sent','received'])
        self.assertTrue(all(v['tool']=='phone_type' and v['method']=='tools/call' for v in events))
        text=self.peer.events.read_text()
        for marker in ['PRIVATE-ARGUMENT','PRIVATE-RESPONSE','b'*64,'arguments','content']:
            self.assertNotIn(marker,text)
        self.assertEqual(stat.S_IMODE(self.peer.events.stat().st_mode),0o600)

    def test_timeout_keeps_same_live_request_and_refuses_a_replay(self):
        with self.assertRaises(TimeoutError):
            self.peer.request('tools/call',{'name':'delayed'},timeout=.01)
        self.assertEqual(self.peer.pending,2)
        with self.assertRaises(AssertionError):self.peer.request('tools/call',{'name':'phone_type'})
        (self.root/'release').touch()
        reply=self.peer.wait(2)
        self.assertEqual(reply['id'],2)
        self.assertIsNone(self.peer.pending)
        events=[v for v in self.events() if v['id']==2]
        self.assertEqual([v['event'] for v in events],['sending','sent','observation_timeout','received'])
        self.assertEqual(sum(v['event']=='sent' for v in self.events() if v['tool']=='delayed'),1)

    def test_process_exit_remains_unknown_and_is_not_a_new_request(self):
        with self.assertRaises(RuntimeError):self.peer.request('tools/call',{'name':'eof'},timeout=2)
        self.peer.process.wait(timeout=2)
        self.assertEqual(self.peer.pending,2)
        with self.assertRaises(AssertionError):self.peer.request('tools/call',{'name':'phone_type'})
        self.assertEqual(self.events()[-1]['event'],'process_eof')

if __name__=='__main__':unittest.main()
