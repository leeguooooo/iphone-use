#!/usr/bin/env python3
"""Read literal selectors against an App-owned model on an explicit owned Simulator."""
import argparse, hashlib, importlib.util, json, os, platform, plistlib, shutil, time, uuid
from pathlib import Path
from mcp_probe_peer import Peer, payload
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('selector_transport',ROOT/'scripts/test-drag-simulator.py')
drag=importlib.util.module_from_spec(spec);spec.loader.exec_module(drag)
BUNDLE='com.leeguoo.iphone-use.selector-oracle'
EXPECTED_ROWS=[
 {'identifier':'selector.primary','label':'Save','value':'stable','enabled':True,'kind':'Button'},
 {'identifier':'selector.secondary','label':'Save','value':None,'enabled':True,'kind':'Button'},
 {'identifier':'selector.disabled','label':'Save','value':None,'enabled':False,'kind':'Button'},
 {'identifier':'selector.unicode','label':"保存\n'O\\K'🙂",'value':None,'enabled':True,'kind':'Button'},
 {'identifier':'selector.empty','label':'Explicit empty','value':'','enabled':True,'kind':'StaticText'},
 {'identifier':'selector.missing-value','label':'Missing value','value':None,'enabled':True,'kind':'StaticText'},
]
CASES=[
 ('primary',{'selector':{'kind':'XCUIElementTypeButton','identifier':'selector.primary','label':'Save','value':'stable','enabled':True}},['selector.primary'],1),
 ('disabled',{'selector':{'kind':'Button','enabled':False}},['selector.disabled'],1),
 ('duplicate-label',{'selector':{'kind':'Button','label':'Save'},'limit':1},['selector.primary'],3),
 ('unicode',{'selector':{'label':"保存\n'O\\K'🙂",'label_contains':'保存\n'}},['selector.unicode'],1),
 ('empty',{'selector':{'value':''}},['selector.empty'],1),
 ('wrong-value',{'selector':{'identifier':'selector.primary','value':'wrong'}},[],0),
 ('wrong-case',{'selector':{'kind':'Button','label':'save'}},[],0),
 ('query-and-selector',{'selector':{'kind':'Button','enabled':False},'query':'SAVE'},['selector.disabled'],1),
 ('missing-identifier',{'selector':{'identifier':'selector.absent'}},[],0),
 ('missing-is-not-empty',{'selector':{'identifier':'selector.missing-value','value':''}},[],0),
]

def verify_case(model,seed,before,after,body,expected,matched,arguments=None):
 errors=[]
 if model.get('rows')!=EXPECTED_ROWS:errors.append('App fixture model differs from declared controls')
 seed_rows={r.get('identifier'):r for r in seed.get('elements',[]) if r.get('identifier')}
 for expected_row in EXPECTED_ROWS:
  seed_row=seed_rows.get(expected_row['identifier'])
  if not seed_row or any(seed_row.get(k)!=expected_row[k] for k in ('label','kind')):errors.append('seed missing a declared control')
 if before.get('run_id')!=model.get('run_id') or after.get('run_id')!=model.get('run_id') or before.get('pid')!=after.get('pid'):errors.append('stale App identity')
 if after.get('touches')!=0 or after.get('application_state')!=0 or after.get('key_window') is not True:errors.append('App changed or unexpected input')
 rows=body.get('elements')
 if not isinstance(rows,list):return errors+['missing element rows']
 if [r.get('identifier') for r in rows]!=expected:errors.append('identifiers differ from independent case')
 if body.get('filter',{}).get('matched')!=matched or body.get('filter',{}).get('omitted')!=matched-len(expected):errors.append('match/omission counts differ')
 original={r.get('identifier'):(r.get('index',i),r) for i,r in enumerate(seed.get('elements',[])) if r.get('identifier')}
 declared={r['identifier']:r for r in model['rows']}
 for row in rows:
  ident=row.get('identifier')
  if ident not in original or row.get('index')!=original[ident][0]:errors.append('original tree index was lost')
  if ident not in declared or row.get('label')!=declared[ident]['label'] or row.get('kind')!=declared[ident]['kind']:errors.append('row differs from App model')
 selector=(arguments or {}).get('selector',{})
 for row in rows:
  for field in ('enabled','value'):
   if field in selector:
    expected_value=declared.get(row.get('identifier'),{}).get(field)
    if (row.get(field) is not expected_value if field=='enabled' else row.get(field)!=expected_value):errors.append('requested '+field+' differs from App model')
    if (row.get(field) is not selector[field] if field=='enabled' else row.get(field)!=selector[field]):errors.append('requested '+field+' not preserved')
  if 'value' in selector and row.get('value_truncated') is True:errors.append('truncated preview matched an exact value')
 if not body.get('snapshot') or body.get('snapshot')!=seed.get('snapshot'):errors.append('filtered snapshot differs from full static seed')
 return errors

def build(out):
 project=out/'fixture-src';shutil.copytree(ROOT/'runner/gesture-fixture',project)
 (project/'main.swift').write_bytes((ROOT/'scripts/fixtures/element-selector/main.swift').read_bytes())
 pbx=project/'GestureFixture.xcodeproj/project.pbxproj';pbx.write_text(pbx.read_text().replace('com.leeguoo.iphone-use.gesture-fixture',BUNDLE))
 info=project/'Info.plist';data=plistlib.loads(info.read_bytes());data.update(CFBundleIdentifier=BUNDLE,CFBundleName='Selector Oracle',CFBundleDisplayName='Selector Oracle')
 data['UIApplicationSceneManifest']['UISceneConfigurations']['UIWindowSceneSessionRoleApplication'][0]['UISceneDelegateClassName']='$(PRODUCT_MODULE_NAME).SelectorSceneDelegate';info.write_bytes(plistlib.dumps(data))
 result=drag.command(['xcodebuild','-project',project/'GestureFixture.xcodeproj','-scheme','GestureFixture','-configuration','Release','-sdk','iphonesimulator','-destination','generic/platform=iOS Simulator','-derivedDataPath',out/'fixture-build','-jobs','1','OTHER_SWIFT_FLAGS=$(inherited) -j1','ARCHS='+platform.machine(),'CODE_SIGNING_ALLOWED=NO','build'],check=False,timeout=180)
 (out/'fixture-build.log').write_text(result.stdout+result.stderr);assert result.returncode==0,'fixture build failed'
 app=out/'fixture-build/Build/Products/Release-iphonesimulator/GestureFixture.app';assert plistlib.loads((app/'Info.plist').read_bytes())['CFBundleIdentifier']==BUNDLE
 drag.command(['codesign','--force','--sign','-',app]);return app

def main():
 parser=argparse.ArgumentParser(description=__doc__)
 for name in ['base','udid','owner','product-version']:parser.add_argument('--'+name,required=True)
 for name in ['mcp','runner-record','out']:parser.add_argument('--'+name,type=Path,required=True)
 args=parser.parse_args();args.out.mkdir(mode=0o700,parents=True,exist_ok=False)
 daemon=drag.Daemon(args.base,args.owner);daemon.require_simulator(args.udid)
 assert drag.command(['xcrun','simctl','get_app_container',args.udid,BUNDLE,'data'],check=False).returncode!=0,'do not overwrite an existing fixture'
 result={'scope':'read-only selector queries on owned Simulator','passed':False,'cases':[],'cleanup_errors':[], 'mcp_sha256':hashlib.sha256(args.mcp.read_bytes()).hexdigest()};installed=False;peer=None
 def save(name,value):
  p=args.out/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n')
 try:
  app=build(args.out);drag.command(['xcrun','simctl','install',args.udid,app]);installed=True
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
  home=args.out/'mcp-home';home.mkdir(mode=0o700)
  env={k:v for k,v in os.environ.items() if not k.startswith(('IPHONE_USE_','PHONE_REMOTE_','IPU_','TEST_RUNNER_')) and k not in ('GH_TOKEN','GITHUB_TOKEN')}
  env.update(HOME=str(home),IPHONE_USE_URL=args.base,IPHONE_USE_OWNER=args.owner,IPHONE_USE_MCP_PREWARM='0',IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1',IPHONE_USE_MCP_STRUCTURED='full')
  peer=Peer([str(args.mcp.resolve())],env,args.out/'mcp.log');assert peer.initialization['result']['serverInfo']['version']==args.product_version
  status=peer.request('tools/call',{'name':'phone_status','arguments':{}});_,_,bound=payload(status)
  assert bound.get('target')=='simulator' and bound.get('udid','').lower()==args.udid.lower() and bound.get('drivable') is True and bound.get('owner') in (None,args.owner)
  seed=daemon.call('/agent/elements?ocr=off');save('seed.json',seed)
  raw,_=drag.runner_call(args,'/source?format=json&max_depth=12&max_nodes=300&extension_calls=0');save('raw-source.json',raw)
  for name,params,expected,matched in CASES:
   daemon.require_simulator(args.udid)
   response=peer.request('tools/call',{'name':'phone_elements','arguments':{**params,'ocr':'off','include_hidden':True}},timeout=90);save(name+'/response.json',response)
   tool,_,body=payload(response);assert 'error' not in response and tool.get('isError') is not True,'selector failed;no replay'
   after=json.loads((data/'selector-state.json').read_text());save(name+'/after.json',after)
   errors=verify_case(model,seed,before,after,body,expected,matched,params)
   result['cases'].append({'name':name,'passed':not errors,'errors':errors});assert not errors,str(errors)
  invalid=peer.request('tools/call',{'name':'phone_elements','arguments':{'selector':['Button',None,None,None,None,None]}});save('invalid-array/response.json',invalid)
  assert 'error' in invalid or invalid.get('result',{}).get('isError') is True,'position array was accepted'
  result['cases'].append({'name':'invalid-array','passed':True,'errors':[]})
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
