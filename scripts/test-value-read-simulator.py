#!/usr/bin/env python3
"""Short native value-read regression against App-owned seeded state, no personal phone."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import subprocess
import time
import traceback
import urllib.request
import urllib.error
from runner_auth import authorization

ROOT=Path(__file__).resolve().parent.parent
spec=importlib.util.spec_from_file_location('owned_input_fixture',ROOT/'scripts/test-input-target-simulator.py')
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--xctestrun',required=True,type=Path)
    parser.add_argument('--out',required=True,type=Path)
    parser.add_argument('--daemon',type=Path)
    parser.add_argument('--mcp',type=Path)
    parser.add_argument('--runtime',default='com.apple.CoreSimulator.SimRuntime.iOS-27-0')
    parser.add_argument('--replacement',action='store_true',help='also exercise native/daemon bound replacement and interrupted tails')
    parser.add_argument('--probe',action='store_true',help='record baseline without accepting completeness')
    args=parser.parse_args()
    if args.replacement and not args.daemon:parser.error('--replacement requires --daemon')
    out=args.out.resolve();out.mkdir(mode=0o700,parents=True,exist_ok=False)
    xctestrun=args.xctestrun.resolve(strict=True);token=secrets.token_hex(32)
    with socket.socket() as listener:listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
    env={k:v for k,v in os.environ.items() if not k.startswith(('IPHONE_USE_','PHONE_REMOTE_','IPU_','TEST_RUNNER_IPU_','SIMCTL_CHILD_'))}
    env.update(TEST_RUNNER_IPU_RUNNER_PORT=str(port),TEST_RUNNER_IPU_RUNNER_MJPEG_PORT='0',TEST_RUNNER_IPU_RUNNER_TOKEN=token,TEST_RUNNER_IPU_DAEMON_PORT='0')
    opener=urllib.request.build_opener(urllib.request.ProxyHandler({}));process=None;daemon=None;peer=None;udid=None;result={'runtime':args.runtime};cleanup=[]
    def call(path,body=None,expected=200):
        method='GET' if body is None else 'POST';data=b'' if body is None else json.dumps(body).encode()
        request=urllib.request.Request(f'http://127.0.0.1:{port}{path}',data=None if body is None else data,method=method,
            headers={'Content-Type':'application/json','Authorization':authorization(token,method,path,data)})
        try:
            with opener.open(request,timeout=10) as response:status=response.status;value_result=json.load(response)['value'];headers=dict(response.headers)
        except urllib.error.HTTPError as error:status=error.code;value_result=json.load(error)['value'];headers=dict(error.headers)
        if status!=expected:
            diagnostic={'path':path,'status':status,'reply':value_result,'reads':{}}
            if '/ipu/element/' in path and path.endswith('/replace'):
                base,tail=path.split('/ipu/element/',1);element=tail.split('/')[0]
                for suffix in ['attribute/value','attribute/placeholderValue','value?max_utf16=32','attribute/selectedTextRange']:
                    read_path=base+'/element/'+element+'/'+suffix
                    request=urllib.request.Request(f'http://127.0.0.1:{port}'+read_path,headers={'Authorization':authorization(token,'GET',read_path,b'')})
                    try:
                        with opener.open(request,timeout=10) as response:diagnostic['reads'][suffix]=json.load(response)
                    except Exception as error:diagnostic['reads'][suffix]={'error_type':type(error).__name__}
                try:
                    time.sleep(0.3)
                    documents=Path(fixture.command('xcrun','simctl','get_app_container',udid,fixture.BUNDLE,'data'))/'Documents'
                    diagnostic['app_receipt']=json.loads((documents/'field-a-state.json').read_text())
                except Exception as error:diagnostic['app_receipt_error_type']=type(error).__name__
            result.setdefault('unexpected_native_responses',[]).append(diagnostic)
        assert status==expected,{"path":path,"status":status,"expected":expected}
        return value_result,headers
    try:
        app=fixture.build_fixture(out)
        udid=fixture.command('xcrun','simctl','create','ipu-value-'+secrets.token_hex(4),'com.apple.CoreSimulator.SimDeviceType.iPhone-17',args.runtime)
        fixture.command('xcrun','simctl','boot',udid);fixture.command('xcrun','simctl','bootstatus',udid,'-b',timeout=180)
        fixture.command('xcrun','simctl','install',udid,app)
        with (out/'runner.log').open('w') as log:
            process=subprocess.Popen(['xcodebuild','test-without-building','-xctestrun',str(xctestrun),'-destination',f'id={udid}','-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe','-parallel-testing-enabled','NO'],env=env,stdout=log,stderr=log,start_new_session=True)
        until=time.monotonic()+120
        while True:
            try:
                status,_=call('/status')
                if status.get('ready'):break
            except (OSError,ValueError):pass
            assert process.poll() is None,'runner exited before readiness'
            assert time.monotonic()<until,'readiness deadline'
            time.sleep(0.2)
        fixture.command('xcrun','simctl','launch',udid,fixture.BUNDLE,'--long-value-fixture','--continuous-value-receipt')
        oracle_path=Path(fixture.command('xcrun','simctl','get_app_container',udid,fixture.BUNDLE,'data'))/'Documents/field-a-state.json'
        until=time.monotonic()+10
        while not oracle_path.exists():
            assert time.monotonic()<until,'fixture oracle deadline';time.sleep(0.05)
        oracle=json.loads(oracle_path.read_text());expected='queue-deadline-'+'字'*6000
        assert oracle['characters']==len(expected) and oracle['sha256']==hashlib.sha256(expected.encode()).hexdigest()
        session,_=call('/session',{});prefix='/session/'+session['sessionId']
        node,_=call(prefix+'/element',{'using':'predicate string','value':"identifier == 'field-a'"})
        element=node.get('ELEMENT',node.get('element-6066-11e4-a52e-4f735466cecf'))
        value,headers=call(prefix+'/element/'+element+'/attribute/value')
        full=bounded=emoji={};native_field={}
        if not args.probe:
            full,_=call(prefix+'/element/'+element+'/value')
            bounded,_=call(prefix+'/element/'+element+'/value?max_utf16=128')
            source,_=call(prefix+'/source?format=json')
            def nodes(value):
                if isinstance(value,dict):
                    yield value
                    for child in value.values():yield from nodes(child)
                elif isinstance(value,list):
                    for child in value:yield from nodes(child)
            native_field=next(node for node in nodes(source) if node.get('rawIdentifier')=='field-a')
            emoji_node,_=call(prefix+'/element',{'using':'predicate string','value':"identifier == 'field-b'"})
            emoji_id=emoji_node.get('ELEMENT',emoji_node.get('element-6066-11e4-a52e-4f735466cecf'))
            emoji,_=call(prefix+'/element/'+emoji_id+'/value?max_utf16=3')
            secure_node,_=call(prefix+'/element',{'using':'predicate string','value':"identifier == 'field-secure'"})
            secure_id=secure_node.get('ELEMENT',secure_node.get('element-6066-11e4-a52e-4f735466cecf'))
            secure,_=call(prefix+'/element/'+secure_id+'/value',expected=400)
            result['native_observations']={'full_len':len(full.get('value') or ''),'full_complete':full.get('complete'),'bounded_len':len(bounded.get('value') or ''),'bounded_complete':bounded.get('complete'),'bounded_total':bounded.get('utf16_length'),'emoji':emoji,'secure':secure,'bulk_flag':native_field.get('value_truncated')}
            assert 'fixture-secure-value' not in json.dumps(secure),{'secure_response_contains_value':True}
            assert full['value']==expected and full['complete'] is True and full['truncated'] is False,result['native_observations']
            assert bounded['value']==expected[:128] and bounded['complete'] is False and bounded['utf16_length']==len(expected),result['native_observations']
            assert emoji['value']=='🙂' and emoji['returned_utf16_length']==2 and emoji['truncated'] is True,result['native_observations']
            assert native_field.get('value_truncated') is True,{'flag':native_field.get('value_truncated'),'preview_chars':len(native_field.get('value') or '')}

        result={'scope':'actual native runner on owned Simulator; no real-host/physical acceptance','simulator':udid,'expected_chars':len(expected),'observed_chars':len(value or ''),'oracle':oracle,'value_sha256':hashlib.sha256((value or '').encode()).hexdigest(),'value_headers':{k:v for k,v in headers.items() if k.lower().startswith('x-value')},'snapshot_limits':status.get('axSnapshotValueLimits'),'full_value_matches':value==expected,'structured_full_matches':full.get('value')==expected,'bounded_prefix_chars':len(bounded.get('value','')),'emoji_scalar_boundary':emoji.get('value')=='🙂','secure_refused':True,'bulk_preview_chars':len(native_field.get('value') or ''),'bulk_value_truncated':native_field.get('value_truncated'),'probe_only':args.probe}
        if args.daemon:
            assert not args.probe
            state=out/'daemon-state';runtime=out/'daemon-runtime';state.mkdir(mode=0o700);runtime.mkdir(mode=0o700)
            (state/'runner-token').write_text(token);(state/'runner-token').chmod(0o600)
            with socket.socket() as listener:listener.bind(('127.0.0.1',0));daemon_port=listener.getsockname()[1]
            daemon_env=dict(env);daemon_env.update(IPHONE_USE_HOST='127.0.0.1',IPHONE_USE_PORT=str(daemon_port),IPHONE_USE_INSTANCE='value-'+udid[:8].lower(),TMPDIR=str(runtime),IPHONE_USE_STATE_DIR=str(state),IPHONE_USE_SIMULATOR=udid,IPHONE_USE_RUNNER_URL=f'http://127.0.0.1:{port}',IPHONE_USE_RUNNER_MJPEG_URL=f'http://127.0.0.1:{port}',IPHONE_USE_RUNNER_MANAGED='0',IPHONE_USE_AGENT_TOKEN=token,IPHONE_USE_PASSWORD='fixture-'+token,IPHONE_USE_AUTO_FOCUS='0',IPHONE_USE_IDLE_RELEASE_SECS='0',IPHONE_USE_FLOWS_NO_AUTO_UPDATE='1')
            with (out/'daemon.log').open('w') as log:daemon=subprocess.Popen([str(args.daemon.resolve()),'serve'],env=daemon_env,stdout=log,stderr=log,start_new_session=True)
            def api(path,body=None,control=False):
                data=None if body is None else json.dumps(body).encode()
                headers={'Authorization':'Bearer '+token,'Content-Type':'application/json','X-Phone-Owner':'value-reader'}
                if control:headers['X-Phone-Control']='1'
                request=urllib.request.Request(f'http://127.0.0.1:{daemon_port}{path}',data=data,headers=headers)
                try:
                    with opener.open(request,timeout=20) as response:return response.status,json.load(response)
                except urllib.error.HTTPError as error:return error.code,json.load(error)
            until=time.monotonic()+30
            while True:
                try:
                    code,status=api('/agent/status')
                    if code==200 and status.get('drivable'):break
                except OSError:pass
                assert time.monotonic()<until,'daemon readiness';time.sleep(0.1)
            owner_before=status.get('owner')
            code,whole=api('/agent/value',{'identifier':'field-a'})
            assert code==200 and whole['value']==expected and whole['complete'] is True,whole
            code,cut=api('/agent/value',{'identifier':'field-a','max_utf16':128})
            assert code==200 and cut['value']==expected[:128] and cut['truncated'] is True,cut
            code,ambiguous=api('/agent/value',{'label':'Draft'})
            assert code==422 and ambiguous['error']=='ambiguous_element',ambiguous
            code,secure=api('/agent/value',{'identifier':'field-secure'})
            assert code==403 and 'fixture-secure-value' not in json.dumps(secure),secure
            code,tree=api('/agent/elements?ocr=off')
            assert code==200
            rows=tree.get('elements',[])
            row=next(row for row in rows if row.get('identifier')=='field-a')
            assert row.get('value_truncated') is True and len(row.get('value') or '')==512,row
            if args.mcp:
                mcp_env=dict(env);mcp_env.update(IPHONE_USE_URL=f'http://127.0.0.1:{daemon_port}',IPHONE_USE_TOKEN=token,IPHONE_USE_OWNER='value-reader',IPHONE_USE_MCP_TOOLS='readonly',IPHONE_USE_MCP_STRUCTURED='full')
                peer=fixture.VideoPeer(args.mcp.resolve(),mcp_env,out/'mcp.log');peer.initialize()
                rpc=peer.request('tools/call',{'name':'phone_read_value','arguments':{'identifier':'field-a'}})['result']
                assert not rpc.get('isError'),rpc
                texts=[block.get('text','') for block in rpc['content'] if block.get('type')=='text']
                rendered=json.loads(texts[0]);assert rendered['value']==expected and rendered['complete'] is True
                assert 'value' not in rpc.get('structuredContent',{})
                result['mcp_single_copy_full_value']=True
            _,status=api('/agent/status');assert status.get('owner')==owner_before
            # The daemon owns a different Runner session; re-resolve in that live session.
            session,_=call('/session',{})
            check_prefix='/session/'+session['sessionId']
            focused,_=call(check_prefix+'/element',{'using':'predicate string','value':"identifier == 'field-a'"})
            focused_id=focused.get('ELEMENT',focused.get('element-6066-11e4-a52e-4f735466cecf'))
            assert call(check_prefix+'/element/'+focused_id+'/attribute/focused')[0] is True
            result['daemon_full_and_bounded_matches']=True;result['owner_lease_unchanged']=True;result['focus_unchanged']=True
            if args.replacement:
                def find(identifier):
                    node,_=call(check_prefix+'/element',{'using':'predicate string','value':"identifier == '"+identifier+"'"})
                    return node.get('ELEMENT',node.get('element-6066-11e4-a52e-4f735466cecf'))
                def read_field(identifier):
                    return call(check_prefix+'/element/'+find(identifier)+'/attribute/value')[0] or ''
                first=find('field-a')
                bound,_=call(check_prefix+'/ipu/element/'+first+'/replace',{'text':'native-prefix'})
                call(check_prefix+'/wda/keys',{'value':['后续'],'inputTarget':bound['inputTarget']})
                assert read_field('field-a')=='native-prefix后续'
                call(check_prefix+'/ipu/element/'+find('field-a')+'/replace',{'text':'invalid\n'},expected=400)
                assert read_field('field-a')=='native-prefix后续'
                call(check_prefix+'/ipu/element/'+find('field-secure')+'/replace',{'text':'blocked'},expected=400)
                assert call(check_prefix+'/element/'+find('field-a')+'/attribute/focused')[0] is True
                bound,_=call(check_prefix+'/ipu/element/'+find('field-a')+'/replace',{'text':'original'})
                call(check_prefix+'/element/'+find('field-b')+'/click',{})
                refused,_=call(check_prefix+'/wda/keys',{'value':['blocked'],'inputTarget':bound['inputTarget']},expected=400)
                assert read_field('field-a')=='original' and read_field('field-b')=='🙂🙂value'
                result['replacement_native']={'prefix_and_tail':True,'newline_before_clear_refused':True,'secure_focus_unchanged':True,'external_command_invalidates_target':True}
                def replacement(identifier,text):
                    code,screen=api('/agent/elements?ocr=off');assert code==200
                    index=next(index for index,row in enumerate(screen['elements']) if row.get('identifier')==identifier)
                    return api('/agent/input',{'type':'set_value','snapshot':screen['snapshot'],'element':index,'value':text},control=True)
                # Literal placeholder text and an empty field must remain distinct,
                # even with max_utf16=1 and a raw AX placeholder fallback.
                placeholder='Draft placeholder'
                assert replacement('field-a',placeholder)[0]==200
                until=time.monotonic()+5
                while True:
                    literal_oracle=json.loads(oracle_path.read_text())
                    if literal_oracle.get('sha256')==hashlib.sha256(placeholder.encode()).hexdigest():break
                    assert time.monotonic()<until,'literal placeholder App receipt mismatch';time.sleep(0.05)
                code,literal=api('/agent/value',{'identifier':'field-a','max_utf16':1})
                assert code==200 and literal['value']=='D' and literal['utf16_length']==17 and literal['truncated'] is True and literal.get('placeholder_shown') is not True,literal
                assert replacement('field-a','')[0]==200
                until=time.monotonic()+5
                while True:
                    empty_oracle=json.loads(oracle_path.read_text())
                    if empty_oracle.get('characters')==0 and empty_oracle.get('sha256')==hashlib.sha256(b'').hexdigest():break
                    assert time.monotonic()<until,'empty placeholder App receipt mismatch';time.sleep(0.05)
                code,empty=api('/agent/value',{'identifier':'field-a','max_utf16':1})
                assert code==200 and empty.get('value') in ['',None] and empty['utf16_length']==0 and empty['complete'] is True and empty['truncated'] is False,empty
                assert '_value_matches_placeholder' not in empty and '_value_matches_placeholder' not in literal
                result['placeholder_cases']={'literal_one_unit_read':literal,'literal_oracle':literal_oracle,'empty_one_unit_read':empty,'empty_oracle':empty_oracle}
                text='queue-deadline-'+'字'*486
                assert len(text)==501
                before=(out/'runner.log').read_text().count('/replace ->')
                code,receipt=replacement('field-a',text)
                assert code==200 and receipt.get('ok') is True,receipt
                # Daemon uses its own Runner session; resolve a fresh probe session for observations.
                session,_=call('/session',{});check_prefix='/session/'+session['sessionId']
                assert read_field('field-a')==text
                until=time.monotonic()+5
                while True:
                    replacement_oracle=json.loads(oracle_path.read_text())
                    if replacement_oracle.get('characters')==501 and replacement_oracle.get('sha256')==hashlib.sha256(text.encode()).hexdigest():break
                    assert time.monotonic()<until,'App-owned replacement oracle mismatch'
                    time.sleep(0.05)
                result['replacement_oracle']=replacement_oracle
                assert (out/'runner.log').read_text().count('/replace ->')==before+1
                # Clear both fields, then switch focus inside the first native prefix.
                assert replacement('field-a','')[0]==200
                assert replacement('field-b','')[0]==200
                before=(out/'runner.log').read_text().count('/wda/keys ->')
                code,changed=replacement('field-a','shift-focus-test-'+'x'*484)
                assert code!=200 and changed.get('outcome')=='unknown',changed
                assert (out/'runner.log').read_text().count('/wda/keys ->')==before
                session,_=call('/session',{});check_prefix='/session/'+session['sessionId']
                assert len(read_field('field-a'))==20 and len(read_field('field-b'))==180
                until=time.monotonic()+15
                while not api('/agent/status')[1].get('drivable'):
                    assert time.monotonic()<until,'fresh work requires recovered drivable status'
                    time.sleep(0.1)
                assert replacement('field-a','')[0]==200
                # A real App input event starts the hold, never a guessed sleep.
                marker=oracle_path.parent/'input-started.count';marker.unlink(missing_ok=True)
                before=(out/'runner.log').read_text().count('/wda/keys ->')
                with ThreadPoolExecutor(max_workers=1) as pool:
                    pending=pool.submit(replacement,'field-a','privacy-prefix-'+'字'*486)
                    until=time.monotonic()+15
                    while not marker.exists():
                        assert not pending.done() and time.monotonic()<until,'replacement did not reach fixture input event'
                        time.sleep(0.01)
                    assert api('/agent/hold',{'secs':60,'private':True},control=True)[0]==200
                    code,private=pending.result(timeout=20)
                assert private.get('native_mutations_confirmed')==1 and private.get('native_mutations_uncertain')==0,private
                assert private.get('outcome')=='partial' and private.get('ok') is False,private
                assert (out/'runner.log').read_text().count('/wda/keys ->')==before
                assert api('/agent/hold',{'secs':0},control=True)[0]==200
                session,_=call('/session',{});check_prefix='/session/'+session['sessionId']
                assert len(read_field('field-a'))==200
                result['replacement_cases']={'native_prefix_and_bound_tail':True,'native_newline_before_clear_refused':True,'native_secure_before_focus_refused':True,'native_target_invalidation_refused':True,'daemon_501_character_replacement':True,'daemon_empty_clear':True,'mid_prefix_focus_change_unknown_without_tail':True,'private_after_app_input_event_preserves_one_ack_without_tail':True,'private_partial_receipt':{k:private.get(k) for k in ['ok','outcome','native_mutations_confirmed','native_mutations_uncertain','retry_safe']}}
        if not args.probe:assert value==expected,'selected value is incomplete'
    except Exception as error:
        result.update(error_type=type(error).__name__,error=str(error),traceback=traceback.format_exc(limit=5))
    finally:
        if peer:
            try:peer.close()
            except Exception as error:cleanup.append(str(error))
        if daemon and daemon.poll() is None:
            os.killpg(daemon.pid,signal.SIGTERM)
            try:daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:os.killpg(daemon.pid,signal.SIGKILL);daemon.wait(timeout=5)
        for directory in [out/'daemon-state',out/'daemon-runtime']:
            if directory.exists():shutil.rmtree(directory)
        if process and process.poll() is None:
            os.killpg(process.pid,signal.SIGTERM)
            try:process.wait(timeout=15)
            except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)
        if udid:
            for operation in ['shutdown','delete']:
                try:fixture.command('xcrun','simctl',operation,udid)
                except Exception as error:cleanup.append(str(error))
        for name in ['runner.log','daemon.log','mcp.log']:
            log=out/name
            if log.exists():log.write_text(log.read_text(errors='replace').replace(token,'[fixture token redacted]'))
        result['cleanup_errors']=cleanup
        result['sources']={name:hashlib.sha256((ROOT/name).read_bytes()).hexdigest() for name in ['scripts/test-value-read-simulator.py','scripts/test-input-target-simulator.py','runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.m','runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.h','runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerElements.swift','runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerRoutes.swift','crates/server/src/http.rs','crates/server/src/runner.rs','crates/mcp/src/client.rs','crates/mcp/src/server.rs','crates/mcp/src/compact.rs']}
        for key,path in [('daemon_binary_sha256',args.daemon),('mcp_binary_sha256',args.mcp)]:
            if path:result[key]=hashlib.sha256(path.resolve().read_bytes()).hexdigest()
        for key,path in [('fixture_binary_sha256',out/'InputGuard.app/InputGuard'),('fixture_source_sha256',out/'fixture.swift')]:
            if path.exists():result[key]=hashlib.sha256(path.read_bytes()).hexdigest()
        result['xctestrun_sha256']=hashlib.sha256(xctestrun.read_bytes()).hexdigest()
        result['runner_binary_sha256']=hashlib.sha256((xctestrun.parent/'Debug-iphonesimulator/iPhoneUse-Runner.app/PlugIns/iPhoneUse.xctest/iPhoneUse').read_bytes()).hexdigest()
        (out/'results.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))
    return 0 if 'error_type' not in result and not cleanup and (args.probe or result.get('full_value_matches')) and (not args.replacement or result.get('replacement_cases')) else 1

if __name__=='__main__':raise SystemExit(main())
