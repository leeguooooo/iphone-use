#!/usr/bin/env python3
"""Isolate hosted CI Simulator/XCTest startup from the product CLI. Never selects a phone."""
import http.client,importlib.util,json,os,secrets,signal,socket,subprocess,time,urllib.request,uuid
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
def command(label,args,timeout=60,env=None):
    stage={'label':label,'argv':list(map(str,args)),'state':'started'};result['stages'].append(stage);save()
    start=time.monotonic()
    try:
        p=subprocess.run(stage['argv'],env=env,capture_output=True,text=True,timeout=timeout)
        (OUT/(label+'.log')).write_text((p.stdout+p.stderr).replace(token,'REDACTED'))
        stage.update(state='completed',code=p.returncode,seconds=time.monotonic()-start);save()
        if p.returncode:raise RuntimeError(f'{label} exit{p.returncode}')
        return p.stdout.strip()
    except subprocess.TimeoutExpired as error:
        (OUT/(label+'.log')).write_bytes((error.stdout or b'')+(error.stderr or b''))
        stage.update(state='timeout_stopped',seconds=time.monotonic()-start);save();raise
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
finally:
    if process is not None and process.poll() is None:
        os.killpg(process.pid,signal.SIGTERM)
        try:process.wait(timeout=10)
        except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=10)
    if process is not None:result['xctest_exit_after_cleanup']=process.poll()
    for path in OUT.glob('*.log'):path.write_text(path.read_text(errors='replace').replace(token,'REDACTED'))
    if sim:
        try:
            spec=importlib.util.spec_from_file_location('owned_cleanup',ROOT/'scripts/run-collect-simulator.py');module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
            result['simulator_cleanup']=module.cleanup_owned_simulator(sim,NAME,dict(os.environ),OUT)
        except BaseException as error:result['cleanup_errors'].append(str(error))
    if result['cleanup_errors']:result['passed']=False
    save();print(json.dumps(result),flush=True)
if not result['passed']:raise SystemExit(1)
