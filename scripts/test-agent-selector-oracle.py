#!/usr/bin/env python3
"""Blind task oracle checks; no model, Simulator or phone calls."""
import copy
import importlib.util
import json
from pathlib import Path
import uuid
import sys
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('selector_oracle', HERE/'agent-bench/selector_oracle.py')
oracle = importlib.util.module_from_spec(spec); spec.loader.exec_module(oracle)
run_id = str(uuid.uuid4())
case = oracle.make_case('a'*64, run_id)
assert case == oracle.make_case('a'*64, run_id)
assert case['rows'] != oracle.make_case('b'*64, run_id)['rows']
assert oracle.target_identifier(case['rows']) not in oracle.PROMPT
assert len({row['identifier'] for row in case['rows']}) == 8
assert {row['label'] for row in case['rows']} == {'Record'}
# Position and ID change across cases; no target role name enters an ID.
positions = set(); identifiers = set()
for index in range(32):
    value = oracle.make_case(f'{index:064x}', run_id)
    target = oracle.target_identifier(value['rows'])
    positions.add(next(i for i,row in enumerate(value['rows']) if row['identifier'] == target))
    identifiers.add(target)
assert len(positions) == 8 and len(identifiers) == 32
model = {'run_id':run_id, 'case_sha256':oracle.case_digest(case), 'rows':case['rows']}
state = {'run_id':run_id, 'case_sha256':oracle.case_digest(case), 'pid':1234,
         'input_events':0, 'foreground_losses':0, 'button_activations':0, 'application_state':0, 'key_window':True}
active = {'bundleId':oracle.BUNDLE,'pid':state['pid']}
target = oracle.target_identifier(model['rows'])
answer = json.dumps({'identifier':target})
for key,collection in [('identifier','elements'), ('name','nodes')]:
    tool='mcp__fixture__read'
    events=[{'type':'assistant','message':{'content':[{'type':'tool_use','id':'read-1','name':tool,'input':{}}]}},
            {'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'read-1','content':[{'type':'text','text':json.dumps({collection:[{key:target}]})}]}]}}]
    assert oracle.verify(case,model,state,state,answer,events,[tool],active_before=active,active_after=active) == []
    for wrong in ['', 'not JSON', json.dumps({'identifier':'wrong'}), json.dumps({'identifier':target,'extra':True}),
                  '{"identifier":"wrong","identifier":'+json.dumps(target)+'}']:
        assert oracle.verify(case,model,state,state,wrong,events,[tool],active_before=active,active_after=active)
    for field,value in [('run_id',str(uuid.uuid4())), ('case_sha256','0'*64), ('pid',456),
                        ('input_events',1), ('foreground_losses',1), ('button_activations',1), ('application_state',2), ('key_window',False),
                        ('pid',True), ('input_events',False), ('application_state',False)]:
        after={**state,field:value}
        assert oracle.verify(case,model,state,after,answer,events,[tool],active_before=active,active_after=active), field
    # Mere answer/echoed input, failed calls, foreign tools and ambiguous IDs
    # cannot supply the independent backend-read witness.
    assert oracle.verify(case,model,state,state,answer,[],[tool],active_before=active,active_after=active)
    assert oracle.verify(case,model,state,state,answer,events,['mcp__other__read'],active_before=active,active_after=active)
    altered=copy.deepcopy(events);altered[1]['message']['content'][0]['is_error']=True
    assert not oracle.witnessed_identifier(altered,target,[tool])
    altered=copy.deepcopy(events);altered[1]['message']['content'][0]['content']=json.dumps({'selector':{key:target}})
    assert not oracle.witnessed_identifier(altered,target,[tool])
    for body in [{'selector':{collection:[{key:target}]}}, {'diagnostics':{collection:[{key:target}]}},
                 {'error':{'code':'failed'},collection:[{key:target}]},
                 {'isError':True,'structuredContent':{collection:[{key:target}]}},
                 {'ok':False,collection:[{key:target}]}]:
        altered=copy.deepcopy(events);altered[1]['message']['content'][0]['content']=json.dumps(body)
        assert not oracle.witnessed_identifier(altered,target,[tool])
    nested=copy.deepcopy(events);nested[1]['message']['content'][0]['content']=json.dumps({'structuredContent':{collection:[{key:target}]},'isError':False})
    assert oracle.witnessed_identifier(nested,target,[tool])
    assert not oracle.witnessed_identifier(list(reversed(events)),target,[tool])
    assert not oracle.witnessed_identifier(events+[events[0]],target,[tool])
    assert not oracle.witnessed_identifier(events+[events[1]],target,[tool])
    assert oracle.witnessed_identifier([{'type':'system','subtype':'permission_denied','message':'denied'}]+events,target,[tool])
changed=copy.deepcopy(model);changed['rows'].reverse()
assert oracle.verify(case,changed,state,state,answer,events,[tool],active_before=active,active_after=active)
for malformed in [[],[{}]*8, [{**row,'identifier':'selector.empty'} for row in case['rows']],
                  [{**row,'enabled':1} for row in case['rows']], [{**row,'value':[]} for row in case['rows']]]:
    try:oracle.validate_rows(malformed)
    except ValueError:pass
    else:raise AssertionError('malformed rows accepted')

# A stale clean file cannot hide a fixture exit or replacement process.
for invalid in [None, {}, {'bundleId':'com.apple.springboard','pid':state['pid']}, {'bundleId':oracle.BUNDLE,'pid':state['pid']+1}]:
    assert oracle.verify(case,model,state,state,answer,events,[tool],active_before=active,active_after=invalid)

# Target proof comes from the very same MCP process before any screen read.
sys.path.insert(0,str(HERE))
spec=importlib.util.spec_from_file_location('blind_sim_probe',HERE/'test-agent-selector-simulator.py')
probe=importlib.util.module_from_spec(spec);spec.loader.exec_module(probe)
args=SimpleNamespace(udid=str(uuid.uuid4()),owner='owned-instance')
class Peer:
    def __init__(self,status):self.status=status;self.calls=[]
    def request(self,method,params):
        self.calls.append((method,params['name']))
        return {'result':{'structuredContent':self.status,'content':[],'isError':False}}
healthy={'target':'simulator','udid':args.udid,'drivable':True,'owner':args.owner}
peer=Peer(healthy);probe.require_mcp_simulator(peer,args)
assert peer.calls==[('tools/call','phone_status')]
for change in [{'target':'device'},{'udid':str(uuid.uuid4())},{'drivable':False},{'owner':'someone-else'}]:
    peer=Peer({**healthy,**change})
    try:probe.require_mcp_simulator(peer,args)
    except RuntimeError:pass
    else:raise AssertionError('misbound MCP accepted')
    assert peer.calls==[('tools/call','phone_status')]
print('blind selector oracle: neutral/randomized cases, App identity, independent answers and correlated read witnesses passed; offline only')
