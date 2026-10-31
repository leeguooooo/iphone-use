#!/usr/bin/env python3
"""Isolate hosted CI Simulator/XCTest startup from the product CLI. Never selects a phone."""
import http.client,importlib.util,json,os,re,secrets,signal,socket,subprocess,tempfile,time,urllib.request,uuid
from pathlib import Path
from runner_auth import authorization
ROOT=Path(__file__).resolve().parents[1]
assert os.environ.get('GITHUB_ACTIONS')=='true' and os.environ.get('RUNNER_ENVIRONMENT')=='github-hosted','hosted CI only'
OUT=Path(os.environ['RUNNER_TEMP'])/'simulator-platform-diagnostic';OUT.mkdir(exist_ok=False)
PLATFORMS={'xcode16.2-ios18.2':('16.2','18.2'),'xcode16.4-ios18.5':('16.4','18.5')}
PLATFORM=os.environ.get('IPU_DIAGNOSTIC_PLATFORM','xcode16.2-ios18.2')
assert PLATFORM in PLATFORMS,'unsupported diagnostic platform'
XCODE,IOS=PLATFORMS[PLATFORM]
RUNTIME='com.apple.CoreSimulator.SimRuntime.iOS-'+IOS.replace('.','-')
NAME='ipu-platform-'+str(uuid.uuid4());sim=None;process=None;token=secrets.token_hex(32)
result={'premise':f'macos15/Xcode{XCODE}/runtime{IOS} can boot an owned Simulator and directly launch the Native XCTest runner','platform':PLATFORM,'passed':False,'scope':'hostedCI only;not product or physical acceptance;does not establish other runtime lanes','stages':[],'cleanup_errors':[]}
def save(): (OUT/'result.json').write_text(json.dumps(result,indent=2)+'\n')
def stop_owned_group(child):
    report={'pid':child.pid,'signals':[],'errors':[],'scope':'owned group signals and parent exit; escaped descendants not checked'}
    for sig in [signal.SIGTERM,signal.SIGKILL]:
        try:os.killpg(child.pid,sig);report['signals'].append(int(sig))
        except ProcessLookupError:pass
        except OSError as error:report['errors'].append(type(error).__name__+': '+str(error))
        try:child.wait(timeout=5)
        except subprocess.TimeoutExpired:pass
        except OSError as error:report['errors'].append(type(error).__name__+': '+str(error))
    report['parent_exit']=child.poll()
    if report['parent_exit'] is None:report['errors'].append('parent exit unconfirmed')
    result['cleanup_errors'].extend(report['errors'])
    return report

def command(label,args,timeout=60,env=None):
    stage={'label':label,'argv':list(map(str,args)),'state':'started'};result['stages'].append(stage);save()
    start=time.monotonic();failure=None
    # Files, not pipes: a grandchild retaining stdout must not defeat timeout.
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process=subprocess.Popen(stage['argv'],env=env,stdout=stdout,stderr=stderr,start_new_session=True)
        stage['pid']=process.pid;save()
        try:process.wait(timeout=timeout)
        except BaseException as error:
            failure=error
            stage['group_cleanup']=stop_owned_group(process)
        stdout.seek(0);stderr.seek(0)
        output=stdout.read(8<<20).decode(errors='replace');errors=stderr.read(8<<20).decode(errors='replace')
        stage['log_truncated']=bool(stdout.read(1) or stderr.read(1))
    (OUT/(label+'.log')).write_text((output+errors).replace(token,'REDACTED'))
    kind='timeout' if isinstance(failure,subprocess.TimeoutExpired) else 'interrupted'
    state=kind+('_client_stopped' if process.poll() is not None else '_unconfirmed') if failure else 'completed'
    stage.update(state=state,code=process.poll(),seconds=time.monotonic()-start);save()
    if isinstance(failure,subprocess.TimeoutExpired):raise subprocess.TimeoutExpired(stage['argv'],timeout,output=output,stderr=errors)
    if failure:raise failure
    if process.returncode:raise RuntimeError(f'{label} exit{process.returncode}')
    return output.strip()

try:
    assert command('xcode',['xcodebuild','-version']).splitlines()[0]=='Xcode '+XCODE,'unexpected Xcode'
    assert command('sdk-version',['xcrun','--sdk','iphonesimulator','--show-sdk-version'])==IOS,'unexpected Simulator SDK'
    command('simctl-location',['xcrun','--find','simctl'])
    command('processes-before',['ps','-Ao','pid,ppid,etime,comm'])
    command('runtime',['bash',ROOT/'scripts/ensure-simulator-runtime.sh',RUNTIME,IOS,OUT/'runtime'],900)
    command('full-inventory-before',['xcrun','simctl','list'],90)
    env=dict(os.environ,IPU_RUNNER_DERIVED_DATA=str(OUT/'runner-build'))
    command('runner-build',['bash',ROOT/'runner/build.sh','--simulator'],300,env)
    recipes=list((OUT/'runner-build/Build/Products').glob('*.xctestrun'));assert len(recipes)==1
    sim=command('create',['xcrun','simctl','create',NAME,'com.apple.CoreSimulator.SimDeviceType.iPhone-15',RUNTIME]);uuid.UUID(sim)
    result.update(simulator=sim,simulator_name=NAME);save()
    command('boot',['xcrun','simctl','boot',sim],120)
    command('bootstatus',['xcrun','simctl','bootstatus',sim,'-b'],240)
    command('full-inventory-after-boot',['xcrun','simctl','list'],90)
    command('processes-after-boot',['ps','-Ao','pid,ppid,etime,comm'])
    # Test an independent ordinary UIKit app before attributing launch failure
    # to XCTest or to this product's runner. No automation runner is used here.
    result['ordinary_app']={'ready':False,'phase':'build','timeout_seconds':60};save()
    fixture_out=OUT/'ordinary-app-build';fixture_out.mkdir()
    fixture_code="""import contextlib,importlib.util,json,sys
from pathlib import Path
root=Path(sys.argv[1]);sys.path.insert(0,str(root/'scripts'))
spec=importlib.util.spec_from_file_location('ordinary_fixture',root/'scripts/test-collect-simulator.py')
module=importlib.util.module_from_spec(spec);sys.modules[spec.name]=module
with contextlib.redirect_stdout(sys.stderr):
    spec.loader.exec_module(module)
    app=module.build_fixture(Path(sys.argv[2]))
print(json.dumps({'app':str(app),'bundle':module.BUNDLE}))
"""
    fixture=json.loads(command('ordinary-app-build',['python3','-c',fixture_code,ROOT,fixture_out],300))
    result['ordinary_app'].update(bundle=fixture['bundle'],phase='install');save()
    command('ordinary-app-install',['xcrun','simctl','install',sim,fixture['app']],90)
    nonce=str(uuid.uuid4());result['ordinary_app'].update(phase='launch',run_id=nonce);save()
    launched=command('ordinary-app-launch',['xcrun','simctl','launch',sim,fixture['bundle'],'--run-id',nonce,'--mode','unique'],90)
    pids=re.findall(r'^'+re.escape(fixture['bundle'])+r':\s*(\d+)\s*$',launched,re.MULTILINE)
    assert len(pids)==1,'ordinary launch must identify one app PID'
    launch_pid=int(pids[0]);result['ordinary_app'].update(phase='readback',launch_pid=launch_pid);save()
    data=Path(command('ordinary-app-data',['xcrun','simctl','get_app_container',sim,fixture['bundle'],'data'],60))/'Documents'
    ordinary_start=time.monotonic();ordinary_deadline=ordinary_start+60
    while True:
        try:
            ordinary=json.loads((data/'list-state.json').read_text())
            (OUT/'ordinary-app-state.json').write_text(json.dumps(ordinary,indent=2)+'\n')
            result['ordinary_app']['last_state']=ordinary
            if not isinstance(ordinary,dict):raise ValueError('ordinary receipt must be an object')
            result['ordinary_app'].pop('last_read_error',None)
            reasons=[]
            for reason,valid in [
                ('nonce_mismatch',ordinary.get('run_id')==nonce),
                ('not_active',ordinary.get('application_state')==0),
                ('window_not_key',ordinary.get('key_window') is True),
                ('empty_viewport',ordinary.get('width',0)>0),
                ('no_visible_rows',bool(ordinary.get('visible_indices'))),
                ('pid_mismatch',ordinary.get('pid')==launch_pid),
            ]:
                if not valid:reasons.append(reason)
            result['ordinary_app'].update(rejection_reasons=reasons,elapsed_seconds=time.monotonic()-ordinary_start);save()
            if not reasons and time.monotonic()<=ordinary_deadline:break
        except (OSError,ValueError) as error:
            result['ordinary_app'].update(last_read_error=str(error),rejection_reasons=['receipt_unreadable'],elapsed_seconds=time.monotonic()-ordinary_start);save()
        if time.monotonic()>=ordinary_deadline:raise RuntimeError('ordinary UIKit app did not publish fresh active window state within60s')
        time.sleep(.1)
    result['ordinary_app'].update(ready=True,phase='ready',state=ordinary,observation_scope='before terminate command');save()
    command('ordinary-app-stop',['xcrun','simctl','terminate',sim,fixture['bundle']],30)
    result['ordinary_app']['terminate_command']='completed; process absence not independently checked'
    result['xctest_start_context']='warm, after ordinary UIKit readiness and completed terminate command';save()
    with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
    args=['xcodebuild','-destination','id='+sim,'test-without-building','-xctestrun',str(recipes[0]),'-only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe']
    env=dict(os.environ,TEST_RUNNER_IPU_RUNNER_PORT=str(port),TEST_RUNNER_IPU_RUNNER_TOKEN=token)
    log=OUT/'direct-xctest.log'
    with log.open('w') as sink:process=subprocess.Popen(args,env=env,stdout=sink,stderr=subprocess.STDOUT,start_new_session=True)
    result['xctest_pid']=process.pid;result['xctest_started_at']=time.time();save();deadline=time.monotonic()+180
    opener=urllib.request.build_opener(urllib.request.ProxyHandler({}));observations=[]
    while time.monotonic()<deadline:
        if process.poll() is not None:raise RuntimeError(f'direct XCTest exited {process.returncode}')
        try:
            request=urllib.request.Request(f'http://127.0.0.1:{port}/status',headers={'Authorization':authorization(token,'GET','/status',b'')})
            with opener.open(request,timeout=3) as reply:status=json.load(reply)
            observations.append({'at':time.time(),'status':status})
            if not isinstance(status,dict) or not isinstance(status.get('value'),dict):
                raise ValueError('status reply must contain an object value')
            if status.get('value',{}).get('ready') is True:result['passed']=True;break
        except (OSError,ValueError,http.client.HTTPException) as error:
            observations.append({'at':time.time(),'error':type(error).__name__+': '+str(error)})
        finally:
            (OUT/'status-observations.json').write_text(json.dumps(observations,indent=2)+'\n')
        time.sleep(1)
    if not result['passed']:
        # A failed optional stack sample must not replace the readiness failure.
        result['readiness_failure']='direct XCTest status never became ready'
        result['xctest_exit_at_deadline']=process.poll();save()
        try:command('sample-xctest',['sample',str(process.pid),'3','-file',OUT/'xctest.sample'],20)
        except Exception as error:result['sample_error']=type(error).__name__+': '+str(error);save()
        raise RuntimeError('direct XCTest status never became ready')
    command('full-inventory-after-xctest',['xcrun','simctl','list'],90)
except BaseException as error:
    result['passed']=False
    result['error']=type(error).__name__+': '+str(error)
    if result.get('ordinary_app',{}).get('ready') is False:
        result['ordinary_app']['failure']=result['error'];save()
    if sim and isinstance(error,Exception):
        try:
            predicate='eventMessage CONTAINS "com.leeguoo" OR process CONTAINS "Runner"'
            if result.get('ordinary_app',{}).get('bundle'):predicate+=' OR eventMessage CONTAINS '+json.dumps(result['ordinary_app']['bundle'])
            command('simulator-launch-events',['xcrun','simctl','spawn',sim,'log','show','--last','5m','--style','compact','--predicate',predicate],30)
        except Exception as diagnostic_error:result['launch_events_error']=str(diagnostic_error)
finally:
    if process is not None:
        try:
            result['xctest_group_cleanup']=stop_owned_group(process)
            result['xctest_exit_after_cleanup']=process.poll()
        except BaseException as cleanup_error:result['cleanup_errors'].append(str(cleanup_error))
    for path in OUT.glob('*.log'):
        try:path.write_text(path.read_text(errors='replace').replace(token,'REDACTED'))
        except OSError as error:result['cleanup_errors'].append('log redaction: '+str(error))
    if sim:
        try:
            spec=importlib.util.spec_from_file_location('owned_cleanup',ROOT/'scripts/run-collect-simulator.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
            def cleanup_command(argv,*,env=None,timeout=120):
                return command('cleanup-read-'+str(len(result['stages'])),argv,env=env,timeout=timeout)
            def cleanup_stop(child):
                report=stop_owned_group(child)
                result.setdefault('cleanup_process_groups',[]).append(report)
                if report['errors']:raise RuntimeError('cleanup client termination unconfirmed')
            module.command=cleanup_command;module.stop=cleanup_stop
            result['simulator_cleanup']=module.cleanup_owned_simulator(sim,NAME,dict(os.environ),OUT)
        except BaseException as error:result['cleanup_errors'].append(str(error))
    if result['cleanup_errors']:result['passed']=False
    save();print(json.dumps(result),flush=True)
if not result['passed']:raise SystemExit(1)
