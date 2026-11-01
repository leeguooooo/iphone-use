"""Resolve one owned UIKit launch using independent state, never by replaying it."""
import math,re,subprocess

def launch_once(command,sim,bundle,nonce):
    try:
        output=command('ordinary-app-launch',['xcrun','simctl','launch',sim,bundle,'--run-id',nonce,'--mode','unique'],90)
    except (subprocess.TimeoutExpired,RuntimeError) as error:
        return None,{'state':'unconfirmed','error':type(error).__name__+': '+str(error)}
    pids=re.findall(r'^'+re.escape(bundle)+r':\s*(\d+)\s*$',output,re.MULTILINE)
    if len(pids)==1 and int(pids[0])>0:return int(pids[0]),{'state':'confirmed'}
    return None,{'state':'unconfirmed','error':'launch output did not identify one positive PID'}

def state_reasons(state,nonce,expected_pid=None):
    if not isinstance(state,dict):return ['invalid_receipt']
    reasons=[]
    checks=[('nonce_mismatch',state.get('run_id')==nonce),
            ('invalid_pid',type(state.get('pid')) is int and state['pid']>0),
            ('not_active',type(state.get('application_state')) is int and state['application_state']==0),
            ('window_not_key',state.get('key_window') is True),
            ('empty_viewport',type(state.get('width')) in (int,float) and math.isfinite(state['width']) and state['width']>0),
            ('empty_height',type(state.get('height')) in (int,float) and math.isfinite(state['height']) and state['height']>0),
            ('no_visible_rows',isinstance(state.get('visible_indices'),list) and bool(state['visible_indices']) and all(type(i) is int and i>=0 for i in state['visible_indices']))]
    if expected_pid is not None:checks.append(('pid_mismatch',state.get('pid')==expected_pid))
    return [reason for reason,valid in checks if not valid]

def process_matches(command,executable,nonce):
    return (command==executable or command.startswith(executable+' ')) and re.search(r'(?:^|\s)--run-id\s+'+re.escape(nonce)+r'(?:\s|$)',command) is not None

def observe_process(command,pid,executable,nonce,timeout):
    try:
        identity=command('ordinary-app-process',['ps','-ww','-p',str(pid),'-o','command='],timeout)
    except (RuntimeError,subprocess.TimeoutExpired) as error:
        return False,{'reason':'kernel_process_unavailable','error':type(error).__name__+': '+str(error)}
    if not process_matches(identity,executable,nonce):return False,{'reason':'kernel_process_mismatch'}
    return True,{}
