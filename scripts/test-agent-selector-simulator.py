#!/usr/bin/env python3
"""Validate the blind task oracle with faults on an explicit owned Simulator.

This is fixture acceptance, not an agent benchmark. Sends deliberate inputs
only to the newly installed fixture, then proves they invalidate read-only proof.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import secrets
import shutil
import stat
import time
import urllib.request
import uuid

from mcp_probe_peer import Peer, payload

ROOT = Path(__file__).resolve().parents[1]
def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    value = importlib.util.module_from_spec(spec); spec.loader.exec_module(value)
    return value

drag = module('agent_selector_transport', ROOT/'scripts/test-drag-simulator.py')
oracle = module('agent_selector_oracle', ROOT/'scripts/agent-bench/selector_oracle.py')
BUNDLE = oracle.BUNDLE
OPEN = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class OwnedDaemon(drag.Daemon):
    def call(self, path, body=None):
        assert body is None, 'fixture faults use only the explicit Runner record'
        request = urllib.request.Request(self.base + path, headers={'X-Phone-Owner': self.owner})
        with OPEN.open(request, timeout=30) as response:
            return json.load(response)


def runner(args, target, body=None):
    binding = json.loads(args.runner_record.read_text())
    assert binding['udid'].lower() == args.udid.lower()
    port = binding['port']; assert type(port) is int and 1 <= port <= 65535
    token = (args.runner_record.parent/'runner-token').read_text().strip()
    if not re.fullmatch(r'[0-9a-f]{32,128}', token):
        raise ValueError('invalid owned Runner token')
    data = None if body is None else json.dumps(body).encode()
    method = 'GET' if body is None else 'POST'
    request = urllib.request.Request(f'http://127.0.0.1:{port}'+target, data=data,
        headers={'Content-Type':'application/json', 'Authorization':drag.authorization(token,method,target,data or b'')})
    with OPEN.open(request, timeout=30) as response:
        return json.load(response)


def require_mcp_simulator(peer, args):
    response=peer.request('tools/call',{'name':'phone_status','arguments':{}})
    tool,_,bound=payload(response)
    if 'error' in response or tool.get('isError') is True or bound.get('target') != 'simulator' or bound.get('udid','').lower() != args.udid.lower() or bound.get('drivable') is not True or bound.get('owner') not in (None,args.owner):
        raise RuntimeError('MCP does not target the owned Simulator;no element read sent')
    return response


def build(out):
    project = out/'fixture-src'; shutil.copytree(ROOT/'runner/gesture-fixture', project)
    (project/'main.swift').write_bytes((ROOT/'scripts/fixtures/agent-selector/main.swift').read_bytes())
    pbx = project/'GestureFixture.xcodeproj/project.pbxproj'
    pbx.write_text(pbx.read_text().replace('com.leeguoo.iphone-use.gesture-fixture', BUNDLE))
    info = project/'Info.plist'; data = plistlib.loads(info.read_bytes())
    data.update(CFBundleIdentifier=BUNDLE, CFBundleName='Records', CFBundleDisplayName='Records')
    data['UIApplicationSceneManifest']['UISceneConfigurations']['UIWindowSceneSessionRoleApplication'][0]['UISceneDelegateClassName']='$(PRODUCT_MODULE_NAME).AgentSelectorSceneDelegate'
    info.write_bytes(plistlib.dumps(data))
    result = drag.command(['xcodebuild','-project',project/'GestureFixture.xcodeproj','-scheme','GestureFixture',
        '-configuration','Release','-sdk','iphonesimulator','-destination','generic/platform=iOS Simulator',
        '-derivedDataPath',out/'fixture-build','-jobs','1','OTHER_SWIFT_FLAGS=$(inherited) -j1',
        'ARCHS='+platform.machine(),'CODE_SIGNING_ALLOWED=NO','build'], check=False, timeout=180)
    (out/'fixture-build.log').write_text(result.stdout+result.stderr)
    assert result.returncode == 0, 'fixture build failed'
    app = out/'fixture-build/Build/Products/Release-iphonesimulator/GestureFixture.app'
    assert plistlib.loads((app/'Info.plist').read_bytes())['CFBundleIdentifier'] == BUNDLE
    drag.command(['codesign','--force','--sign','-',app])
    return app


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['base','udid','owner','product-version']:
        parser.add_argument('--'+name, required=True)
    for name in ['mcp','runner-record','out']:
        parser.add_argument('--'+name, type=Path, required=True)
    args = parser.parse_args(); args.out.mkdir(mode=0o700,parents=True,exist_ok=False)
    daemon = OwnedDaemon(args.base,args.owner); daemon.require_simulator(args.udid)
    assert drag.command(['xcrun','simctl','get_app_container',args.udid,BUNDLE,'data'],check=False).returncode != 0, 'never replace an existing fixture'
    installed = False; launched = False; peer = None
    result = {'scope':'fixture acceptance and deliberate fault injection;not an agent benchmark',
              'passed':False,'cases':[],'cleanup_errors':[],
              'sources':{str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in [Path(__file__),ROOT/'scripts/fixtures/agent-selector/main.swift',ROOT/'scripts/agent-bench/selector_oracle.py']}}
    def save(name,value):
        path=args.out/name; path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(json.dumps(value,indent=2)+'\n')
    try:
        app=build(args.out); result['app_files']={str(p.relative_to(app)):hashlib.sha256(p.read_bytes()).hexdigest() for p in app.rglob('*') if p.is_file()}
        drag.command(['xcrun','simctl','install',args.udid,app]); installed=True
        documents=Path(drag.command(['xcrun','simctl','get_app_container',args.udid,BUNDLE,'data']).stdout.strip())/'Documents'
        documents.mkdir(exist_ok=True)
        home=args.out/'mcp-home'; home.mkdir(mode=0o700)
        env={k:v for k,v in os.environ.items() if not k.startswith(('IPHONE_USE_','PHONE_REMOTE_','IPU_','TEST_RUNNER_')) and k not in ('GH_TOKEN','GITHUB_TOKEN')}
        env.update(HOME=str(home),IPHONE_USE_URL=args.base,IPHONE_USE_OWNER=args.owner,
            IPHONE_USE_MCP_PREWARM='0',IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1',IPHONE_USE_MCP_STRUCTURED='full')
        peer=Peer([str(args.mcp.resolve())],env,args.out/'mcp.log')
        assert peer.initialization['result']['serverInfo']['version'] == args.product_version
        save('mcp-target-binding.json',require_mcp_simulator(peer,args))
        for scenario in ['read-only','static-tap','disabled-tap','blank-tap','swipe','background-return','persistence-failure']:
            daemon.require_simulator(args.udid)
            if launched:
                drag.command(['xcrun','simctl','terminate',args.udid,BUNDLE])
            case=oracle.make_case(secrets.token_hex(32),str(uuid.uuid4()))
            (documents/'agent-selector-case.json').write_bytes(oracle.case_bytes(case)); save(scenario+'/case.json',case)
            drag.command(['xcrun','simctl','launch',args.udid,BUNDLE]); launched=True
            deadline=time.monotonic()+30
            while True:
                try:
                    model=json.loads((documents/'agent-selector-oracle.json').read_text())
                    before=json.loads((documents/'agent-selector-state.json').read_text())
                    if model['run_id']==before['run_id']==case['run_id'] and before['application_state']==0 and before['key_window'] is True:
                        active=runner(args,'/wda/activeAppInfo')['value']
                        if active['bundleId']==BUNDLE and active['pid']==before['pid']:break
                except (OSError,ValueError,KeyError):pass
                assert time.monotonic()<deadline, 'fixture did not become foreground ready'
                time.sleep(.2)
            save(scenario+'/oracle.json',model); save(scenario+'/before.json',before)
            active_before=active; save(scenario+'/active-before.json',active_before)
            response=peer.request('tools/call',{'name':'phone_elements','arguments':{'selector':{'kind':'StaticText','value':''},'ocr':'off','include_hidden':True}},timeout=90)
            save(scenario+'/read-response.json',response); tool,_,body=payload(response)
            target=oracle.target_identifier(model['rows'])
            assert 'error' not in response and tool.get('isError') is not True
            assert [r['identifier'] for r in body['elements']] == [target]
            # A synthetic trace envelope transports a real read result; it is
            # deliberately not labelled as model/agent execution evidence.
            events=[{'type':'assistant','message':{'content':[{'type':'tool_use','id':'one','name':'mcp__fixture__phone_elements'}]}},
                    {'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'one','content':json.dumps(response['result'])}]}}]
            def foreground():
                daemon.require_simulator(args.udid)
                active=runner(args,'/wda/activeAppInfo')['value']
                assert active['bundleId']==BUNDLE and active['pid']==before['pid'], 'foreign App;no fault sent'
            foreground()
            pre_fault=json.loads((documents/'agent-selector-state.json').read_text())
            save(scenario+'/pre-fault.json',pre_fault)
            clean=oracle.verify(case,model,before,pre_fault,json.dumps({'identifier':target}),events,['mcp__fixture__phone_elements'],active_before=active_before,active_after=runner(args,'/wda/activeAppInfo')['value'])
            assert not clean,'read itself or prior state invalidated oracle: '+str(clean)
            if scenario in ['static-tap','disabled-tap']:
                index=next(i for i,row in enumerate(case['rows']) if (row['kind']=='StaticText' if scenario=='static-tap' else row['enabled'] is False))
                reply=runner(args,'/tap',{'x':185,'y':110+index*75}); save(scenario+'/fault-response.json',reply)
            elif scenario=='blank-tap':
                save(scenario+'/fault-response.json',runner(args,'/tap',{'x':185,'y':710}))
            elif scenario=='swipe':
                save(scenario+'/fault-response.json',runner(args,'/swipe',{'x1':80,'y1':710,'x2':280,'y2':710,'duration_ms':700}))
            elif scenario=='background-return':
                save(scenario+'/fault-response.json',runner(args,'/home',{}))
                drag.command(['xcrun','simctl','launch',args.udid,BUNDLE])
            elif scenario=='persistence-failure':
                original_mode=stat.S_IMODE(documents.stat().st_mode)
                previous=(documents/'agent-selector-state.json').read_bytes()
                try:
                    documents.chmod(0o555)
                    try:save(scenario+'/fault-response.json',runner(args,'/tap',{'x':185,'y':710}))
                    except Exception as error:save(scenario+'/fault-error.json',{'type':type(error).__name__})
                    deadline=time.monotonic()+15
                    while True:
                        active=runner(args,'/wda/activeAppInfo')['value']
                        if active.get('bundleId')!=BUNDLE or active.get('pid')!=before['pid']:break
                        assert time.monotonic()<deadline,'persistence failure did not invalidate the fixture;no input replay'
                        time.sleep(.2)
                    assert (documents/'agent-selector-state.json').read_bytes()==previous,'fault must preserve the stale clean file'
                finally:documents.chmod(original_mode)
            deadline=time.monotonic()+10
            while True:
                after=json.loads((documents/'agent-selector-state.json').read_text())
                active=runner(args,'/wda/activeAppInfo')['value']
                if scenario=='persistence-failure':break
                ready=after['application_state']==0 and after['key_window'] is True and active['bundleId']==BUNDLE and active['pid']==before['pid']
                if scenario=='read-only':observed=True
                elif scenario=='background-return':observed=after['foreground_losses']>0
                else:observed=after['input_events']>0
                if ready and observed:break
                assert time.monotonic()<deadline,'fault outcome not observed;do not repeat action'
                time.sleep(.2)
            save(scenario+'/after.json',after); save(scenario+'/active-after.json',active); save(scenario+'/read-witness-envelope.json',events)
            errors=oracle.verify(case,model,before,after,json.dumps({'identifier':target}),events,['mcp__fixture__phone_elements'],active_before=active_before,active_after=active)
            if scenario=='read-only':assert not errors,str(errors)
            elif scenario=='persistence-failure':assert 'live App identity changed or missing' in errors,str(errors)
            else:assert 'fixture received input or lost foreground' in errors,str(errors)
            if scenario!='read-only':assert after['button_activations']==0,'fault must detect input without button activation'
            result['cases'].append({'name':scenario,'passed':True,'read_only_errors':errors})
        result['passed']=True
    except BaseException as error:
        result['error']=type(error).__name__+': '+str(error); raise
    finally:
        if peer:
            try:peer.close()
            except Exception as error:result['cleanup_errors'].append(str(error))
        if installed:
            try:drag.command(['xcrun','simctl','uninstall',args.udid,BUNDLE])
            except Exception as error:result['cleanup_errors'].append(str(error))
        if result['cleanup_errors']:result['passed']=False
        save('result.json',result)


if __name__=='__main__':main()
