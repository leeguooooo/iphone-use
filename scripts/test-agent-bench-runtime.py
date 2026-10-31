#!/usr/bin/env python3
"""Executable/environment isolation controls;fake programs only,no model or phone."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('runtime_bench', HERE / 'agent-bench/run.py')
bench = importlib.util.module_from_spec(spec); spec.loader.exec_module(bench)
BACKEND = '''import json,os,sys
from pathlib import Path
Path(os.environ['BACKEND_RECEIPT']).write_text(json.dumps({k:os.environ.get(k) for k in ['PATH','HOME','ALT_SESSION','SECRET_SAMPLE','DO_NOT_TRACK']} | {'argv':sys.argv[1:]}))
'''
AGENT = '''import json,os,re,shlex,subprocess,sys
from pathlib import Path
args=sys.argv[1:]
Path(os.environ['AGENT_RECEIPT']).write_text(json.dumps({k:os.environ.get(k) for k in ['PATH','HOME','ALT_SESSION']} | {'marker':'parent-agent'}))
config=json.loads(Path(args[args.index('--mcp-config')+1]).read_text())
if config['mcpServers']:
 name,server=next(iter(config['mcpServers'].items()));env=os.environ.copy();env.update(server.get('env',{}))
 subprocess.run([server['command'],*server.get('args',[])],env=env,check=True)
 tool='mcp__'+name+'__probe';tool_input={}
else:
 rule=args[args.index('--allowedTools')+1];launch=re.fullmatch(r'Bash\\((.*)\\)',rule).group(1)
 if os.environ.get('MUTATE_LAUNCHER'):
  with open(launch,'a') as f:f.write('# changed\\n')
 subprocess.run([launch,'space arg','$(literal)','semi;colon'],check=True)
 tool='Bash';tool_input={'command':shlex.join([launch,'space arg','$(literal)','semi;colon'])}
for key,path in [('MUTATE_AGENT',__file__),('MUTATE_DRIVER',os.environ.get('DRIVER_COPY'))]:
 if os.environ.get(key):
  with open(path,'a') as f:f.write('# changed\\n')
print(json.dumps({'type':'system','subtype':'init','model':'pinned-model'}))
print(json.dumps({'type':'assistant','message':{'model':'pinned-model','content':[{'type':'tool_use','id':'one','name':tool,'input':tool_input}]}}))
print(json.dumps({'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'one','content':'synthetic backend completed'}]}}))
print(json.dumps({'type':'result','result':'OK','is_error':False,'duration_ms':1,'num_turns':1,'total_cost_usd':0.01}))
'''


def executable(path, text):
    path.write_text('#!' + sys.executable + '\n' + text); path.chmod(0o700); return path


with tempfile.TemporaryDirectory(prefix='bench-runtime-') as tmp:
    root = Path(tmp).resolve(); parent = root / 'parent'; other = root / 'backend-path'; parent.mkdir(); other.mkdir()
    agent = executable(parent / 'claude', AGENT)
    executable(other / 'claude', 'raise RuntimeError("backend PATH replaced the agent")\n')
    backend = executable(root / 'backend', BACKEND)
    reference = root / 'reference.md'; reference.write_text('Synthetic backend;no device.')
    task = root / 'task.md'; task.write_text('---\nname: runtime\nexpect: OK\nmax_tool_calls: 2\n---\nSynthetic isolation control.\n')
    secret = "spaces $() `literal` ; 'quoted'\nnext line"
    environment = {'PATH': str(other) + ':/usr/bin:/bin', 'HOME': '{run_dir}/backend-home',
                   'ALT_SESSION': '', 'BACKEND_RECEIPT': '{run_dir}/backend.json', 'SECRET_SAMPLE': secret}
    for interface in ['mcp', 'cli']:
        out = root / interface; out.mkdir(mode=0o700)
        config = root / 'backend.mcp.json'
        config.write_text(json.dumps({'mcpServers': {'isolated': {'command': str(backend)}}}))
        definition = bench.backend_spec(None, config=str(config), env=environment) if interface == 'mcp' else bench.backend_spec(None, cli=str(backend), reference=str(reference), env=environment)
        parent_env = {'PATH': str(parent) + ':/usr/bin:/bin', 'HOME': str(root / 'agent-home'),
                      'ALT_SESSION': 'parent-session', 'AGENT_RECEIPT': str(out / 'agent.json')}
        with patch.dict(os.environ, parent_env):
            row = bench.run_task(task, out, None, 'pinned-model', definition)
        got_agent = json.loads((out / 'agent.json').read_text()); got_backend = json.loads((out / 'backend.json').read_text())
        assert got_agent == {**{k:parent_env[k] for k in ['PATH','HOME','ALT_SESSION']}, 'marker':'parent-agent'}
        assert got_backend['PATH'] == environment['PATH'] and got_backend['HOME'] == str(out / 'backend-home')
        assert got_backend['ALT_SESSION'] == '' and got_backend['SECRET_SAMPLE'] == secret
        assert got_backend['argv'] == ([] if interface == 'mcp' else ['space arg','$(literal)','semi;colon'])
        assert row['checks_passed'] and row['runtime_check']['state'] == 'verified' and row['passed'] is None
        context = row['context']; assert context['agent_entrypoint'] == str(agent.resolve())
        context_path = out / 'runtime.context.json'; assert secret not in context_path.read_text()
        assert context['agent_entrypoint_sha256'] == hashlib.sha256(agent.read_bytes()).hexdigest()
        assert stat.S_IMODE((out / 'runtime.mcp.json').stat().st_mode) == 0o600
        if interface == 'cli': assert stat.S_IMODE((out / 'runtime.backend').stat().st_mode) == 0o700
        proof = out / 'proof.json'; proof.write_text(json.dumps({'task':'runtime','trace_sha256':row['trace_sha256'],
            'context_sha256':row['context_sha256'],'verified':True,'verifier':'synthetic executable controls','evidence':'fake agent and backend receipts'}))
        checked = bench.assess(bench.parse_task(task)[0], out / 'runtime.jsonl', 0, proof, context_path)
        assert checked['passed'] is True and checked['comparison_eligible'] is True
        assert checked['execution_policy_check']['state']=='observed_within_limits'
        # Reported usage is required and must fit the requested policy. Merely
        # recording CLI flags cannot qualify an incomplete or over-budget trace.
        original_trace=(out/'runtime.jsonl').read_text()
        for usage in [{'num_turns':13,'total_cost_usd':0.01}, {'num_turns':1,'total_cost_usd':0.51},
                      {'num_turns':True,'total_cost_usd':0.01}, {'num_turns':1,'total_cost_usd':float('nan')},
                      {'num_turns':1}, {'num_turns':1,'total_cost_usd':-1}]:
            events=[json.loads(line) for line in original_trace.splitlines()]
            events[-1].pop('num_turns',None);events[-1].pop('total_cost_usd',None);events[-1].update(usage)
            altered=out/'usage.jsonl';altered.write_text('\n'.join(map(json.dumps,events))+'\n')
            altered_context={**context,'trace_sha256':hashlib.sha256(altered.read_bytes()).hexdigest()}
            altered_path=out/'usage.context.json';altered_path.write_text(json.dumps(altered_context))
            assert not bench.assess(bench.parse_task(task)[0],altered,0,context=altered_path)['comparison_eligible']
        broken={**context,'process_receipt_sha256':'0'*64};changed=out/'bad-process.json';changed.write_text(json.dumps(broken))
        try:bench.assess(bench.parse_task(task)[0],out/'runtime.jsonl',0,context=changed)
        except ValueError as error:assert 'receipt' in str(error)
        else:raise AssertionError('process receipt tampering accepted')
        for field in ['max_turns','max_budget_usd','argv']:
            broken=copy.deepcopy(context)
            if field=='argv':
                broken.pop('agent_argv_sha256');broken['process_record'].pop('argv_sha256')
                broken['process_receipt_sha256']=hashlib.sha256((json.dumps(broken['process_record'])+'\n').encode()).hexdigest()
            else:broken['execution_limits'][field]=100 if field=='max_turns' else '10.00'
            changed=out/(field+'-tamper.json');changed.write_text(json.dumps(broken))
            try:bench.assess(bench.parse_task(task)[0],out/'runtime.jsonl',0,context=changed)
            except ValueError:pass
            else:raise AssertionError('execution argument binding lost: '+field)
        for flag in ['agent_entrypoint_unchanged','driver_unchanged','launcher_unchanged','backend_env_isolated','mcp_config_unchanged']:
            broken = {**context, flag:False}; changed = out / (flag + '.json'); changed.write_text(json.dumps(broken))
            assert bench.assess(bench.parse_task(task)[0], out / 'runtime.jsonl', 0, context=changed)['passed'] is False
    # Requests outside the launcher, shell expansion, and calls without a result
    # cannot become comparable even with a valid independent task receipt.
    cli_out = root / 'cli'
    base_trace = [json.loads(line) for line in (cli_out/'runtime.jsonl').read_text().splitlines()]
    base_context = json.loads((cli_out/'runtime.context.json').read_text())
    for command in [str(backend)+' probe', '/bin/echo bypass', base_context['launcher_path']+' probe; /bin/echo bypass',
                    base_context['launcher_path']+' "$(/bin/echo expansion)"']:
        events = copy.deepcopy(base_trace)
        for event in events:
            if event.get('type') == 'assistant':
                event['message']['content'][0]['input']['command'] = command
        trace = root/'bypass.jsonl'; trace.write_text('\n'.join(map(json.dumps,events))+'\n')
        context = root/'bypass.context.json'; context.write_text(json.dumps({**base_context,'trace_sha256':hashlib.sha256(trace.read_bytes()).hexdigest()}))
        checked = bench.assess(bench.parse_task(task)[0],trace,0,context=context)
        assert checked['passed'] is False and not checked['comparison_eligible']
    for fault in ['missing-id','duplicate-id']:
        events=copy.deepcopy(base_trace)
        extra=copy.deepcopy(next(event for event in events if event.get('type')=='assistant'))
        if fault=='missing-id': extra['message']['content'][0].pop('id')
        events.insert(1,extra)
        trace=root/'invalid-id.jsonl';trace.write_text('\n'.join(map(json.dumps,events))+'\n')
        context=root/'invalid-id.context.json';context.write_text(json.dumps({**base_context,'trace_sha256':hashlib.sha256(trace.read_bytes()).hexdigest()}))
        checked=bench.assess(bench.parse_task(task)[0],trace,0,context=context)
        assert checked['passed'] is False and checked['invalid_tool_call_ids'] and not checked['comparison_eligible']
    for mode in ['no-result','error-result']:
        events = copy.deepcopy(base_trace)
        if mode == 'no-result': events=[event for event in events if event.get('type')!='user']
        else:
            for event in events:
                if event.get('type')=='user': event['message']['content'][0]['is_error']=True
        trace=root/'incomplete.jsonl';trace.write_text('\n'.join(map(json.dumps,events))+'\n')
        context=root/'incomplete.context.json';context.write_text(json.dumps({**base_context,'trace_sha256':hashlib.sha256(trace.read_bytes()).hexdigest()}))
        receipt=root/'incomplete.proof.json';receipt.write_text(json.dumps({'task':'runtime','trace_sha256':hashlib.sha256(trace.read_bytes()).hexdigest(),'context_sha256':hashlib.sha256(context.read_bytes()).hexdigest(),'verified':True,'verifier':'synthetic-check','evidence':'offline control'}))
        checked=bench.assess(bench.parse_task(task)[0],trace,0,receipt,context)
        assert not checked['comparison_eligible'], 'request alone is not successful backend work'
        if mode == 'no-result': assert checked['passed'] is False and checked['unresolved_tool_calls'] == ['Bash']
    assert bench.literal_cli_call('/safe/launcher "literal;arg" \'$(literal)\'', '/safe/launcher')
    assert not bench.literal_cli_call('/safe/launcher `expand`', '/safe/launcher')

    for fault in ['MUTATE_AGENT', 'MUTATE_LAUNCHER', 'MUTATE_DRIVER']:
        agent = executable(parent / 'claude', AGENT)
        out = root / fault; out.mkdir(mode=0o700)
        driver_copy = out / 'driver.py'; driver_copy.write_bytes((HERE / 'agent-bench/run.py').read_bytes())
        copied_spec = importlib.util.spec_from_file_location('driver_copy', driver_copy)
        copied = importlib.util.module_from_spec(copied_spec); copied_spec.loader.exec_module(copied)
        definition = copied.backend_spec(None, cli=str(backend), reference=str(reference), env=environment)
        with patch.dict(os.environ, {'PATH':str(parent)+':/usr/bin:/bin','AGENT_RECEIPT':str(out/'agent.json'),fault:'1','DRIVER_COPY':str(driver_copy)}):
            row = copied.run_task(task, out, None, 'pinned-model', definition)
        assert row['passed'] is False and row['runtime_check']['state'] == 'failed', (fault,row)
    driver_copy=root/'prepare-driver.py';driver_copy.write_bytes((HERE/'agent-bench/run.py').read_bytes())
    prepare_spec=importlib.util.spec_from_file_location('prepare_driver',driver_copy);preparing=importlib.util.module_from_spec(prepare_spec);prepare_spec.loader.exec_module(preparing)
    original_invocation=preparing.task_invocation
    def changed_during_prepare(*args,**kwargs):
        result=original_invocation(*args,**kwargs)
        with driver_copy.open('a') as stream:stream.write('# changed during preparation\n')
        return result
    out=root/'prepare-change';out.mkdir(mode=0o700)
    definition=preparing.backend_spec(None,cli=str(backend),reference=str(reference),env=environment)
    with patch.dict(os.environ,{'PATH':str(parent)+':/usr/bin:/bin'}), patch.object(preparing,'task_invocation',side_effect=changed_during_prepare), patch.object(preparing,'execute_agent',side_effect=AssertionError('changed driver started agent')):
        try:preparing.run_task(task,out,None,'pinned-model',definition)
        except ValueError as error:assert 'preparation' in str(error)
        else:raise AssertionError('changed driver was trusted')
    print('agent-bench runtime: parent agent pinned;backend env/argv isolated;private secrets and mutation controls passed')
