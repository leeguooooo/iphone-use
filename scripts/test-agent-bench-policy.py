#!/usr/bin/env python3
"""Bounded fake-agent controls. Never contacts a model or device."""
import contextlib,hashlib,importlib.util,io,json,os,pathlib,stat,subprocess,sys,tempfile,time
from unittest.mock import patch
HERE=pathlib.Path(__file__).resolve().parent
spec=importlib.util.spec_from_file_location('policy_bench',HERE/'agent-bench/run.py');bench=importlib.util.module_from_spec(spec);spec.loader.exec_module(bench)
with tempfile.TemporaryDirectory(prefix='bench-policy-') as folder:
 root=pathlib.Path(folder).resolve();task=root/'custom.md';task.write_text('---\nname: controlled\nmax_tool_calls: 2\n---\nRead a synthetic marker from the owned Simulator only.\n')
 config=root/'mcp.json';config.write_text(json.dumps({'mcpServers':{'test':{'command':sys.executable,'args':['synthetic-backend.py']}}}))
 backend=bench.backend_spec(None,config=str(config),allowed_mcp_tools=['probe'])
 assert backend['allowed']==['mcp__test__probe','ToolSearch','WaitForMcpServers']
 limits=bench.execution_limits(3,'0.25',2)
 out=root/'prepared';out.mkdir()
 _,_,_,_,_,command=bench.task_invocation(task,out,None,'pinned',backend,limits)
 assert command[command.index('--max-turns')+1]=='3'
 assert command[command.index('--max-budget-usd')+1]=='0.25'
 assert '--no-session-persistence' in command and 'mcp__test__*' not in command
 for kwargs in [{'max_turns':True},{'max_turns':0},{'max_turns':1001},{'timeout_seconds':0},{'max_budget_usd':'NaN'},{'max_budget_usd':'Infinity'},{'max_budget_usd':'-1'},{'max_budget_usd':'101'}]:
  try:bench.execution_limits(**kwargs)
  except ValueError:pass
  else:raise AssertionError(kwargs)
 for names in [[],['*'],['probe','probe'],['mcp__other__probe'],[{}],'probe']:
  try:bench.backend_spec(None,config=str(config),allowed_mcp_tools=names)
  except ValueError:pass
  else:raise AssertionError(names)
 for args in [['--task','x','--task-file',str(task)],['--task-file',str(task),'--max-turns','0'],['--from-jsonl','x','--allow-mcp-tool','probe']]:
  with patch.object(bench,'execute_agent',side_effect=AssertionError('invalid policy launched agent')),contextlib.redirect_stderr(io.StringIO()):
   try:bench.main(args)
   except SystemExit as error:assert error.code==2
   else:raise AssertionError(args)
 # An external task file is prepared under the same strict policy, without any agent.
 with patch.object(bench,'HERE',root/'portable'),patch.object(bench,'execute_agent',side_effect=AssertionError('prepare launched agent')),contextlib.redirect_stdout(io.StringIO()) as printed:
  assert bench.main(['--task-file',str(task),'--mcp-config',str(config),'--model','pinned','--prepare-only','--allow-mcp-tool','probe','--max-turns','3','--max-budget-usd','0.25','--timeout-seconds','2','--json'])==0
 prepared=json.loads(printed.getvalue())[0]['command']
 assert prepared[prepared.index('--allowedTools')+1]=='mcp__test__probe'
 assert prepared[prepared.index('--max-turns')+1]=='3'
 # An actual subprocess exceeds the deadline; its PID is reaped and the task is never replayed.
 stdout=root/'hang.stdout';receipt=root/'hang.process.json'
 started=time.monotonic()
 with stdout.open('w') as sink:
  result=bench.execute_agent([sys.executable,'-c','import time; time.sleep(60)'],stdout=sink,cwd=root,env=os.environ.copy(),timeout=1,receipt=receipt)
 assert result.timed_out and result.returncode<0 and time.monotonic()-started<8
 process=json.loads(receipt.read_text());assert process['state']=='deadline_stopped'
 assert stat.S_IMODE(receipt.stat().st_mode)==0o600
 with patch.object(bench.subprocess,'Popen',side_effect=AssertionError('receipt collision launched process')):
  try:bench.execute_agent(['unused'],stdout=None,cwd=root,env={},timeout=1,receipt=receipt)
  except FileExistsError:pass
  else:raise AssertionError('reused process receipt')
 # Normal agent exit must also stop a same-group descendant, not leave it running.
 heartbeat=root/'heartbeat'
 child_code="import pathlib,sys,time; p=pathlib.Path(sys.argv[1]); i=0\nwhile True:\n p.write_text(str(i)); i+=1; time.sleep(0.01)"
 parent_code="import pathlib,subprocess,sys,time; subprocess.Popen([sys.executable,'-c',sys.argv[1],sys.argv[2]]); p=pathlib.Path(sys.argv[2]); end=time.monotonic()+3\nwhile not p.exists() and time.monotonic()<end: time.sleep(0.01)\nassert p.exists()"
 with stdout.open('w') as sink:
  result=bench.execute_agent([sys.executable,'-c',parent_code,child_code,str(heartbeat)],stdout=sink,cwd=root,env=os.environ.copy(),timeout=5,receipt=root/'parent.process.json')
 assert result.returncode==0 and not result.timed_out
 previous=heartbeat.read_bytes();time.sleep(0.15);assert heartbeat.read_bytes()==previous
 assert result.process_record['state']=='completed' and result.process_record['group_stop_sent']
 # Cleanup that cannot be confirmed never becomes a completed run.
 with patch.object(bench.subprocess,'Popen') as launched,patch.object(bench.os,'killpg'),patch.object(bench.select,'select',return_value=([],[],[])):
  launched.return_value.pid=123456;launched.return_value.returncode=None;launched.return_value.wait.side_effect=subprocess.TimeoutExpired('fake',5)
  try:bench.execute_agent(['fake'],stdout=None,cwd=root,env={},timeout=1,receipt=root/'unverified.process.json')
  except RuntimeError as error:assert 'termination unverified' in str(error)
  else:raise AssertionError('unverified cleanup accepted')
 assert json.loads((root/'unverified.process.json').read_text())['state']=='termination_unverified'
 # Audit rejects a same-server tool outside the explicit allowlist, even with valid results.
 events=[{'type':'system','subtype':'init','model':'pinned'},
  {'type':'assistant','message':{'model':'pinned','content':[{'type':'tool_use','id':'one','name':'mcp__test__delete','input':{}}]}},
  {'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'one','content':'done'}]}},
  {'type':'result','result':'OK','is_error':False,'num_turns':1}]
 # Claude emits textual system messages for permission denials; audit them
 # without crashing and still correlate the denied tool result as a failure.
 denial=[events[0],events[1],{'type':'system','subtype':'permission_denied','message':'Permission denied'},
         {'type':'user','message':{'content':[{'type':'tool_result','tool_use_id':'one','content':'denied','is_error':True}]}},events[-1]]
 assert bench.audit(map(json.dumps,denial),0)['failed_calls']==['mcp__test__delete']
 trace=root/'controlled.jsonl';trace.write_text('\n'.join(map(json.dumps,events))+'\n')
 context={'task':'controlled','task_header':bench.parse_task(task)[0],'interface':'mcp','tool_prefix':'mcp__test__','framework_tools':bench.MCP_FRAMEWORK_TOOLS,'allowed_backend_tools':['mcp__test__probe'],'allowed_tools':backend['allowed'],'entrypoint_unchanged':True,'trace_sha256':hashlib.sha256(trace.read_bytes()).hexdigest(),'execution_policy_version':1,'execution_limits':limits,'agent_timed_out':False}
 ctx=root/'context.json';ctx.write_text(json.dumps(context))
 checked=bench.assess(context['task_header'],trace,0,context=ctx);assert not checked['checks_passed'] and any('outside' in reason for reason in checked['reasons'])
 events[1]['message']['content'][0]['name']='mcp__test__probe';trace.write_text('\n'.join(map(json.dumps,events))+'\n');context['trace_sha256']=hashlib.sha256(trace.read_bytes()).hexdigest();context['agent_timed_out']=True;ctx.write_text(json.dumps(context))
 checked=bench.assess(context['task_header'],trace,0,context=ctx);assert checked['passed'] is False and any('deadline' in reason for reason in checked['reasons'])
print('agent policy: limits/strict tool names/custom task validation/deadline/no replay/post-run subset audit passed;fake agents only')
