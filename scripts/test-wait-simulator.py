#!/usr/bin/env python3
"""Wait semantics against independent App state on an explicit owned Simulator."""
import argparse, hashlib, importlib.util, json, os, time, uuid
from pathlib import Path
from mcp_probe_peer import Peer, payload
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('wait_fixture',ROOT/'scripts/test-selector-simulator.py')
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)
drag=fixture.drag;BUNDLE=fixture.BUNDLE
CASES=[
 ('contains',{'present':[{'kind':'Button','label_contains':'av'}]},True,0),
 ('unicode',{'present':[{'identifier':'selector.unicode','label_contains':"保存\n'O\\K'"}]},True,0),
 ('wrong-case',{'present':[{'label_contains':'save'}]},False,0),
 ('absent',{'absent':[{'label_contains':'not-in-oracle'}]},True,0),
 ('still-present',{'absent':[{'label_contains':'Save'}]},False,0),
 ('conjunction',{'present':[{'identifier':'selector.primary','label':'Save','label_contains':'av'}]},True,0),
 ('wrong-conjunction',{'present':[{'identifier':'selector.primary','label_contains':'none'}]},False,0),
 ('polling',{'present':[{'identifier':'selector.primary','label_contains':'av'}]},True,1000),
 ('ordinal-zero',{'present':[{'kind':'Button','label':'Save','index':0}]},True,0),
 ('ordinal-third',{'present':[{'kind':'Button','label':'Save','index':2}]},True,0),
 ('ordinal-fourth-missing',{'present':[{'kind':'Button','label':'Save','index':3}]},False,0),
 ('ordinal-fourth-absent',{'absent':[{'kind':'Button','label':'Save','index':3}]},True,0),
 ('ordinal-third-still-present',{'absent':[{'kind':'Button','label':'Save','index':2}]},False,0),
 ('ordinal-bool',{'present':[{'kind':'Button','label':'Save','index':True}]},None,0),
 ('ordinal-null',{'present':[{'kind':'Button','label':'Save','index':None}]},None,0),
 ('ordinal-too-large',{'present':[{'kind':'Button','label':'Save','index':200}]},None,0),
 ('ordinal-only',{'present':[{'index':2}]},None,0),
 ('empty-contains',{'present':[{'identifier':'selector.primary','label_contains':''}]},None,0),
 ('nul-contains',{'present':[{'identifier':'selector.primary','label_contains':'\0'}]},None,0),
 ('long-contains',{'present':[{'identifier':'selector.primary','label_contains':'x'*1001}]},None,0),
]

def count_is(value,expected):return type(value) is int and value==expected

def verify_case(model,before,after,body,expect,expected,timeout):
 errors=[]
 if model.get('rows')!=fixture.EXPECTED_ROWS:errors.append('independent App model changed')
 if before.get('run_id')!=model.get('run_id') or after.get('run_id')!=model.get('run_id') or before.get('pid')!=after.get('pid'):errors.append('stale App identity')
 if not count_is(after.get('touches'),0) or not count_is(after.get('application_state'),0) or after.get('key_window') is not True:errors.append('App state or input changed')
 if body.get('ok') is not expected or not count_is(body.get('applied_actions'),0):errors.append('wrong verdict or unexpected applied action')
 if expected:
  if not count_is(body.get('completed'),1):errors.append('step not completed')
  step=(body.get('steps') or [{}])[0];observation=step.get('observation',{})
  if timeout==0 and (not count_is(step.get('attempts'),1) or not count_is(step.get('reads'),1) or step.get('single_read') is not True):errors.append('single-read success did not read exactly once')
 else:
  observation=body.get('observation',{})
  if body.get('error')!='expectation_timeout' or body.get('retry_safe') is not True:errors.append('wrong failed-wait outcome')
  if timeout==0 and (not count_is(observation.get('attempts'),1) or not count_is(observation.get('reads'),1) or observation.get('single_read') is not True):errors.append('single-read failure polled or skipped reading')
  key='missing_present' if 'present' in expect else 'violated_absent'
  failed=observation.get(key)
  if not isinstance(failed,list) or len(failed)!=1 or not count_is(failed[0],0):errors.append('wrong condition failed')
  if observation.get('violated_absent' if key=='missing_present' else 'missing_present')!=[]:errors.append('unrelated condition failed')
 if observation.get('read') is not True or observation.get('stale') is True or observation.get('sparse') is not False:errors.append('no current usable observation')
 if observation.get('present_unproven')!=[] or observation.get('absent_unproven')!=[]:errors.append('unknown state substituted for intended known case')
 if expected and (observation.get('missing_present')!=[] or observation.get('violated_absent')!=[]):errors.append('success conflicts with observation')
 for mode,selectors in expect.items():
  for position,selector in enumerate(selectors):
   if 'index' in selector:
    counts=observation.get(mode+'_counts',[])
    if len(counts)<=position:errors.append('missing ordinal evidence');continue
    count=counts[position]
    if not count_is(count.get('required_matches'),selector['index']+1) or not count_is(count.get('known_matches'),3) or not count_is(count.get('unproven_matches'),0) or type(count.get('complete')) is not bool:errors.append('ordinal counts differ from independent three-button model')
    if ((mode=='present' and expected is False) or (mode=='absent' and expected is True)) and count.get('complete') is not True:errors.append('incomplete tree cannot prove ordinal shortage')
 if timeout==0:
  calls=body.get('timing',{}).get('runner',[])
  if any(type(call.get('count')) is not int or call.get('count',-1)<0 for call in calls):errors.append('invalid runner count')
  if sum(call.get('count',0) for call in calls if call.get('call')=='GET /source' and type(call.get('count')) is int)!=1:errors.append('wrong source request count')
  if any('/elements' in call.get('call','') for call in calls):errors.append('predicate pre-probe used')
 return errors

def verify_action(model,before,after,body):
 errors=[]
 if model.get('rows')!=fixture.EXPECTED_ROWS:errors.append('independent App model changed')
 if before.get('run_id')!=model.get('run_id') or after.get('run_id')!=model.get('run_id') or before.get('pid')!=after.get('pid'):errors.append('stale App identity')
 if not count_is(before.get('touches'),0) or not count_is(after.get('touches'),1) or after.get('last_tapped')!='selector.unicode':errors.append('wrong target or input count')
 if not count_is(after.get('application_state'),0) or after.get('key_window') is not True:errors.append('App left foreground')
 if body.get('ok') is not True or not count_is(body.get('completed'),1) or not count_is(body.get('applied_actions'),1):errors.append('wrong action acknowledgement')
 return errors

def main():
 parser=argparse.ArgumentParser(description=__doc__)
 for name in ['base','udid','owner','product-version']:parser.add_argument('--'+name,required=True)
 for name in ['mcp','runner-record','out']:parser.add_argument('--'+name,type=Path,required=True)
 args=parser.parse_args();args.out.mkdir(mode=0o700,parents=True,exist_ok=False)
 daemon=drag.Daemon(args.base,args.owner);daemon.require_simulator(args.udid)
 assert drag.command(['xcrun','simctl','get_app_container',args.udid,BUNDLE,'data'],check=False).returncode!=0,'do not overwrite existing fixture'
 result={'scope':'Independent App model, Native/MCP wait and one guarded test-button tap on owned Simulator;not phone/performance/parity proof','passed':False,'cases':[],'cleanup_errors':[],'mcp_sha256':hashlib.sha256(args.mcp.read_bytes()).hexdigest()};installed=False;peer=None
 def save(name,value):
  path=args.out/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
 try:
  app=fixture.build(args.out);drag.command(['xcrun','simctl','install',args.udid,app]);installed=True
  nonce=str(uuid.uuid4());drag.command(['xcrun','simctl','launch',args.udid,BUNDLE,'--run-id',nonce])
  data=Path(drag.command(['xcrun','simctl','get_app_container',args.udid,BUNDLE,'data']).stdout.strip())/'Documents';until=time.monotonic()+30
  while True:
   try:
    model=json.loads((data/'selector-oracle.json').read_text());before=json.loads((data/'selector-state.json').read_text())
    if model['run_id']==before['run_id']==nonce and before['application_state']==0 and before['key_window'] is True:break
   except (OSError,ValueError,KeyError):pass
   assert time.monotonic()<until,'App did not become ready';time.sleep(.1)
  save('oracle.json',model);save('before.json',before)
  active,_=drag.runner_call(args,'/wda/activeAppInfo');assert active['value']['bundleId']==BUNDLE and active['value']['pid']==before['pid']
  env={k:v for k,v in os.environ.items() if not k.startswith(('IPHONE_USE_','PHONE_REMOTE_','IPU_','TEST_RUNNER_')) and k not in ('GH_TOKEN','GITHUB_TOKEN')}
  env.update(IPHONE_USE_STATE_DIR=str(args.out/'client-state'),IPHONE_USE_URL=args.base,IPHONE_USE_OWNER=args.owner,IPHONE_USE_MCP_PREWARM='0',IPHONE_USE_FLOWS_DIR=str(args.out/'flows'),IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1',IPHONE_USE_MCP_STRUCTURED='full')
  peer=Peer([str(args.mcp.resolve())],env,args.out/'mcp.log');assert peer.initialization['result']['serverInfo']['version']==args.product_version
  status=peer.request('tools/call',{'name':'phone_status','arguments':{}});_,_,bound=payload(status)
  assert bound.get('target')=='simulator' and bound.get('udid','').lower()==args.udid.lower() and bound.get('drivable') is True and bound.get('owner') in (None,args.owner)
  seed=daemon.call('/agent/elements?ocr=off');save('seed.json',seed)
  for row in fixture.EXPECTED_ROWS:assert any(item.get('identifier')==row['identifier'] and item.get('label')==row['label'] for item in seed.get('elements',[])),'missing declared App control'
  for name,expect,expected,timeout in CASES:
   daemon.require_simulator(args.udid)
   response=peer.request('tools/call',{'name':'phone_run_steps','arguments':{'observe':False,'steps':[{'kind':'wait_for','expect':expect,'timeout_ms':timeout,'poll_ms':50}]}},timeout=15);save(name+'/response.json',response)
   after=json.loads((data/'selector-state.json').read_text());save(name+'/after.json',after)
   if expected is None:
    assert 'error' in response or response.get('result',{}).get('isError') is True,'invalid locator accepted'
    assert after.get('touches')==0 and after.get('pid')==before['pid'] and after.get('run_id')==nonce,'invalid request changed App'
    errors=[]
   else:
    tool,_,body=payload(response)
    assert 'error' not in response,'MCP protocol error, not wait verdict'
    assert (tool.get('isError') is True) is (not expected),'tool error classification differs'
    errors=verify_case(model,before,after,body,expect,expected,timeout)
   result['cases'].append({'name':name,'passed':not errors,'errors':errors});assert not errors,str(errors)
  daemon.require_simulator(args.udid)
  response=peer.request('tools/call',{'name':'phone_run_steps','arguments':{'observe':False,'steps':[{'kind':'tap_locator','locator':{'kind':'Button','identifier':'selector.unicode','label_contains':"保存\n'O\\K'"}}]}},timeout=90);save('tap-contains/response.json',response)
  tool,_,body=payload(response);assert 'error' not in response and tool.get('isError') is not True,'action not acknowledged; never replay'
  until=time.monotonic()+5
  while True:
   after=json.loads((data/'selector-state.json').read_text())
   if after.get('touches',0)>0:break
   assert time.monotonic()<until,'App did not receive the action; no replay';time.sleep(.05)
  samples=[];quiet_started=time.monotonic();quiet_until=quiet_started+3
  while True:
   after=json.loads((data/'selector-state.json').read_text());errors=verify_action(model,before,after,body)
   samples.append({'at_ms':(time.monotonic()-quiet_started)*1000,'state':after});assert not errors,str(errors)
   if time.monotonic()>=quiet_until:break
   time.sleep(.05)
  result['action_quiet_window_secs']=3;save('tap-contains/quiet-window.json',samples)
  save('tap-contains/after.json',after)
  result['cases'].append({'name':'tap-contains','passed':True,'errors':[]})
  daemon.require_simulator(args.udid)
  capped,headers=drag.runner_call(args,'/source?format=json&max_nodes=1&extension_calls=0');save('native-truncation-header/source.json',capped)
  quality={'truncated':headers.get('X-IPU-Truncated'),'nodes':headers.get('X-IPU-Nodes')};save('native-truncation-header/quality.json',quality)
  assert quality['truncated']=='1','node cap must be reported incomplete by the real Native producer'
  result['cases'].append({'name':'native-truncation-header','passed':True,'errors':[]})
  active,_=drag.runner_call(args,'/wda/activeAppInfo');assert active['value']['bundleId']==BUNDLE and active['value']['pid']==before['pid']
  result['passed']=True
 except Exception as error:
  result['error']=type(error).__name__+': '+str(error);raise
 finally:
  if peer:peer.close()
  if installed:
   try:drag.command(['xcrun','simctl','uninstall',args.udid,BUNDLE])
   except Exception as error:result['cleanup_errors'].append(str(error))
  if result['cleanup_errors']:result['passed']=False
  save('result.json',result)
if __name__=='__main__':main()
