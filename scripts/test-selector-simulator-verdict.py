#!/usr/bin/env python3
"""Negative controls for the independent selector App/Native verdict."""
import copy,importlib.util,unittest,sys
from pathlib import Path
HERE=Path(__file__).resolve().parent
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('selector_oracle',HERE/'test-selector-simulator.py');probe=importlib.util.module_from_spec(spec);spec.loader.exec_module(probe)
def sample():
 row={'identifier':'selector.primary','kind':'Button','label':'Save','value':'stable','enabled':True}
 model={'run_id':'owned','rows':copy.deepcopy(probe.EXPECTED_ROWS)};seed={'snapshot':'S','elements':[{'kind':'Application'},*copy.deepcopy(probe.EXPECTED_ROWS)]}
 state={'run_id':'owned','pid':123,'touches':0,'application_state':0,'key_window':True}
 body={'snapshot':'S','elements':[{**row,'index':1}],'filter':{'matched':1,'omitted':0}}
 return [model,seed,state,copy.deepcopy(state),body,['selector.primary'],1,{'selector':{'value':'stable','enabled':True}}]
class Verdict(unittest.TestCase):
 def test_valid_independent_model(self):self.assertEqual(probe.verify_case(*sample()),[])
 def test_no_stale_missing_extra_reindexed_or_unknown_value_passes(self):
  for fault in ['nonce','pid','touch','foreground','label','kind','id','extra','missing','index','snapshot','count','omitted','value','enabled','enabled-number','truncated','wrong-snapshot','model-value','model-enabled','missing-model-control','missing-seed-control']:
   v=sample();row=v[4]['elements'][0]
   if fault=='nonce':v[3]['run_id']='old'
   elif fault=='pid':v[3]['pid']=999
   elif fault=='touch':v[3]['touches']=1
   elif fault=='foreground':v[3]['application_state']=2
   elif fault in ['label','kind','value']:row[fault]='wrong'
   elif fault=='id':row['identifier']='other'
   elif fault=='extra':v[4]['elements'].append(copy.deepcopy(row))
   elif fault=='missing':v[4]['elements']=[]
   elif fault=='index':row['index']=0
   elif fault=='snapshot':v[4]['snapshot']=''
   elif fault=='count':v[4]['filter']['matched']=2
   elif fault=='omitted':v[4]['filter']['omitted']=1
   elif fault=='enabled':row.pop('enabled')
   elif fault=='enabled-number':row['enabled']=1
   elif fault=='wrong-snapshot':v[4]['snapshot']='other-nonempty'
   elif fault=='model-value':v[0]['rows'][0]['value']='changed'
   elif fault=='model-enabled':v[0]['rows'][0]['enabled']=False
   elif fault=='missing-model-control':v[0]['rows'].pop()
   elif fault=='missing-seed-control':v[1]['elements'].pop()
   else:row['value_truncated']=True
   with self.subTest(fault=fault):self.assertTrue(probe.verify_case(*v))
if __name__=='__main__':unittest.main()
