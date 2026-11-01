#!/usr/bin/env python3
import copy,subprocess,unittest,sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parent))
from simulator_ui_readiness import launch_once,state_reasons,process_matches,observe_process
class Readiness(unittest.TestCase):
 def test_launch_is_never_replayed_after_timeout_error_or_missing_ack(self):
  for outcome in [subprocess.TimeoutExpired(['simctl','launch'],90),RuntimeError('service error'),'unexpected output']:
   calls=[]
   def command(label,args,timeout):
    calls.append((label,args,timeout))
    if isinstance(outcome,Exception):raise outcome
    return outcome
   pid,ack=launch_once(command,'owned','com.owned','nonce')
   self.assertIsNone(pid);self.assertEqual(ack['state'],'unconfirmed');self.assertEqual(len(calls),1)
 def test_confirmed_pid_is_exact_and_positive(self):
  for output,pid in [('com.owned: 123',123),('com.other: 123',None),('com.owned: 0',None),('com.owned: 123\ncom.owned: 456',None)]:
   self.assertEqual(launch_once(lambda *args:output,'owned','com.owned','nonce')[0],pid)
 def test_only_fresh_active_visible_state_can_resolve_unknown_launch(self):
  state={'run_id':'nonce','pid':123,'application_state':0,'key_window':True,'width':390,'height':844,'visible_indices':[0,1]}
  self.assertEqual(state_reasons(state,'nonce'),[]);self.assertEqual(state_reasons(state,'nonce',123),[])
  for key,value in [('run_id','old'),('pid',True),('pid',0),('application_state',False),('application_state',2),('key_window',False),('width',True),('width',float('nan')),('width',0),('height',0),('height',False),('visible_indices',[]),('visible_indices',[True])]:
   bad=copy.deepcopy(state);bad[key]=value
   with self.subTest(key=key,value=value):self.assertTrue(state_reasons(bad,'nonce'))
  self.assertTrue(state_reasons(state,'nonce',999))
 def test_unconfirmed_launch_requires_owned_executable_and_nonce_in_kernel_identity(self):
  self.assertTrue(process_matches('/owned/App --run-id nonce --mode unique','/owned/App','nonce'))
  for command in ['/other/App --run-id nonce','/owned/App --run-id old','/owned/App.evil --run-id nonce','python -c /owned/App --run-id nonce','/owned/App --run-id nonce-suffix']:
   with self.subTest(command=command):self.assertFalse(process_matches(command,'/owned/App','nonce'))
 def test_process_probe_failures_are_explicitly_unavailable_not_ready(self):
  for failure in [RuntimeError('ps failed'),subprocess.TimeoutExpired(['ps'],1)]:
   calls=[]
   def command(*args):calls.append(args);raise failure
   verified,report=observe_process(command,123,'/owned/App','nonce',1)
   self.assertFalse(verified);self.assertEqual(report['reason'],'kernel_process_unavailable');self.assertEqual(len(calls),1)
if __name__=='__main__':unittest.main()
