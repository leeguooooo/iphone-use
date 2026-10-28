"""Bounded MCP stdio probe: one pending handle, metadata journal, no mutation replay."""
import json,os,select,signal,socket,subprocess,time
from pathlib import Path

def port():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]


def stop(process):
    if process and process.poll() is None:
        os.killpg(process.pid,signal.SIGTERM)
        try:process.wait(timeout=20)
        except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)


class Peer:
    def __init__(self,argv,env,log):
        self.events=log.with_name(log.stem+'-requests.jsonl')
        os.close(os.open(self.events,os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o600))
        with log.open('w') as out:self.process=subprocess.Popen(argv,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=out,start_new_session=True)
        self.buffer=b'';self.serial=0;self.pending=None
        self.pending_started=None;self.pending_method=None;self.pending_tool=None
        self.request('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'paired-scripted-operations','version':'1'}})
        self.process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n');self.process.stdin.flush()
    def event(self,kind,**extra):
        # Metadata only: never persist arguments, responses, text or tokens.
        entry={'event':kind,'at_ms':time.time_ns()//1_000_000,'id':self.pending,
               'method':self.pending_method,'tool':self.pending_tool}
        if self.pending_started is not None:entry['elapsed_ms']=round((time.monotonic()-self.pending_started)*1000,3)
        entry.update(extra)
        with self.events.open('a') as out:out.write(json.dumps(entry)+'\n')
    def request(self,method,params,timeout=60):
        assert self.pending is None,'do not send another request while delivery is unknown'
        self.serial+=1;self.pending=self.serial
        self.pending_started=time.monotonic();self.pending_method=method
        self.pending_tool=params.get('name') if method=='tools/call' and isinstance(params,dict) else None
        self.event('sending')
        self.process.stdin.write((json.dumps({'jsonrpc':'2.0','id':self.serial,'method':method,'params':params},ensure_ascii=False)+'\n').encode());self.process.stdin.flush()
        self.event('sent')
        return self.wait(timeout)
    def wait(self,timeout):
        until=time.monotonic()+timeout
        while time.monotonic()<until:
            while b'\n' in self.buffer:
                line,self.buffer=self.buffer.split(b'\n',1);reply=json.loads(line)
                if reply.get('id')==self.pending:
                    result=reply.get('result')
                    self.event('received',is_error='error' in reply or (isinstance(result,dict) and result.get('isError') is True))
                    self.pending=None;self.pending_started=None;self.pending_method=None;self.pending_tool=None
                    return reply
            if select.select([self.process.stdout],[],[],max(0,until-time.monotonic()))[0]:
                data=os.read(self.process.stdout.fileno(),65536)
                if not data:
                    self.event('process_eof')
                    raise RuntimeError('MCP process exited with a pending response')
                self.buffer+=data
        self.event('observation_timeout')
        raise TimeoutError('MCP operation deadline; do not replay')
    def close(self):
        if self.pending is not None:
            try:self.wait(60)
            except Exception:pass
        if self.process.poll() is None:
            self.process.stdin.close()
            try:self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:stop(self.process)
        self.process.stdin.close();self.process.stdout.close()
        self.event('process_stopped',returncode=self.process.returncode)


def payload(reply):
    result=reply.get('result',{});text='\n'.join(x.get('text','') for x in result.get('content',[]) if x.get('type')=='text')
    try:body=json.loads(text)
    except ValueError:body=result.get('structuredContent',{})
    return result,text,body

