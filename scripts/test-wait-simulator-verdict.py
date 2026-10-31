#!/usr/bin/env python3
"""Reject fake wait success, unreadable failures, extra reads and App side effects."""
import copy, importlib.util, sys, unittest
from pathlib import Path
HERE=Path(__file__).resolve().parent
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('wait_probe',HERE/'test-wait-simulator.py')
probe=importlib.util.module_from_spec(spec);spec.loader.exec_module(probe)
def sample(ok=True):
 model={'run_id':'owned','rows':copy.deepcopy(probe.fixture.EXPECTED_ROWS)}
 state={'run_id':'owned','pid':123,'touches':0,'application_state':0,'key_window':True}
 observation={'read':True,'sparse':False,'missing_present':[] if ok else [0],'violated_absent':[],'present_unproven':[],'absent_unproven':[],'attempts':1,'reads':1,'single_read':True}
 body={'ok':ok,'applied_actions':0,'timing':{'runner':[{'call':'GET /source','count':1}]}}
 if ok:body.update(completed=1,steps=[{'attempts':1,'reads':1,'single_read':True,'observation':observation}])
 else:body.update(error='expectation_timeout',retry_safe=True,observation=observation)
 return [model,state,copy.deepcopy(state),body,{'present':[{'label_contains':'av'}]},ok,0]
class Verdict(unittest.TestCase):
 def test_valid_known_success_and_failure(self):
  for ok in [True,False]:self.assertEqual(probe.verify_case(*sample(ok)),[])
 def test_app_or_execution_faults_cannot_pass(self):
  for fault in ['model','nonce','pid','touch','inactive','not-key','applied','applied-bool','wrong-ok','completed-bool','attempts','attempts-bool','reads','single','source-count','source-bool','extra-source','probe']:
   v=sample();body=v[3];step=body['steps'][0]
   if fault=='model':v[0]['rows'].pop()
   elif fault=='nonce':v[2]['run_id']='old'
   elif fault=='pid':v[2]['pid']=99
   elif fault=='touch':v[2]['touches']=1
   elif fault=='inactive':v[2]['application_state']=2
   elif fault=='not-key':v[2]['key_window']=False
   elif fault=='applied':body['applied_actions']=1
   elif fault=='applied-bool':body['applied_actions']=False
   elif fault=='wrong-ok':body['ok']=1
   elif fault=='completed-bool':body['completed']=True
   elif fault=='attempts':step['attempts']=2
   elif fault=='attempts-bool':step['attempts']=True
   elif fault=='reads':step['reads']=0
   elif fault=='single':step['single_read']=False
   elif fault=='source-count':body['timing']['runner'][0]['count']=0
   elif fault=='source-bool':body['timing']['runner'][0]['count']=True
   elif fault=='extra-source':body['timing']['runner'][0]['count']=2
   else:body['timing']['runner'].append({'call':'POST /elements','count':1})
   with self.subTest(fault=fault):self.assertTrue(probe.verify_case(*v))
 def test_unknown_stale_sparse_or_missing_reads_cannot_be_substituted(self):
  for ok in [True,False]:
   for fault in ['unread','stale','sparse','unknown-present','unknown-absent','missing-known-key','contradiction']:
    v=sample(ok);observation=v[3]['steps'][0]['observation'] if ok else v[3]['observation']
    if fault=='unread':observation['read']=False
    elif fault=='stale':observation['stale']=True
    elif fault=='sparse':observation['sparse']=True
    elif fault=='unknown-present':observation['present_unproven']=[0]
    elif fault=='unknown-absent':observation['absent_unproven']=[0]
    elif fault=='missing-known-key':observation.pop('present_unproven')
    else:observation['violated_absent']=[0]
    with self.subTest(ok=ok,fault=fault):self.assertTrue(probe.verify_case(*v))
 def test_failed_wait_must_name_the_expected_known_mismatch(self):
  for fault in ['wrong-error','retry','attempts','attempts-bool','reads','single','missing','bool-index']:
   v=sample(False);body=v[3];observation=body['observation']
   if fault=='wrong-error':body['error']='runner_source_failed'
   elif fault=='retry':body['retry_safe']=False
   elif fault=='attempts':observation['attempts']=2
   elif fault=='attempts-bool':observation['attempts']=True
   elif fault=='reads':observation['reads']=0
   elif fault=='single':observation['single_read']=False
   elif fault=='missing':observation['missing_present']=[]
   else:observation['missing_present']=[False]
   with self.subTest(fault=fault):self.assertTrue(probe.verify_case(*v))
 def test_action_verifier_requires_one_tap_on_the_declared_unicode_button(self):
  v=sample();model,before,after=v[:3];after.update(touches=1,last_tapped='selector.unicode');body={'ok':True,'completed':1,'applied_actions':1}
  self.assertEqual(probe.verify_action(model,before,after,body),[])
  for fault in ['nonce','pid','wrong-target','double-tap','tap-bool','background','no-ack','wrong-count','count-bool']:
   a=copy.deepcopy(after);b=copy.deepcopy(body)
   if fault=='nonce':a['run_id']='old'
   elif fault=='pid':a['pid']=44
   elif fault=='wrong-target':a['last_tapped']='selector.primary'
   elif fault=='double-tap':a['touches']=2
   elif fault=='tap-bool':a['touches']=True
   elif fault=='background':a['application_state']=2
   elif fault=='no-ack':b['ok']=False
   elif fault=='wrong-count':b['applied_actions']=0
   else:b['applied_actions']=True
   with self.subTest(fault=fault):self.assertTrue(probe.verify_action(model,before,a,b))
if __name__=='__main__':unittest.main()
