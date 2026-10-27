"""Owned Simulator fixtures and bounded stdio MCP helpers. No external backend adapter."""
import importlib.util,json,os,select,signal,socket,subprocess,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('owned_input_fixture',ROOT/'scripts/test-input-target-simulator.py')
fixture=importlib.util.module_from_spec(spec);spec.loader.exec_module(fixture)

def port():
    with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]


def stop(process):
    if process and process.poll() is None:
        os.killpg(process.pid,signal.SIGTERM)
        try:process.wait(timeout=20)
        except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait(timeout=5)


class Peer:
    def __init__(self,argv,env,log):
        with log.open('w') as out:self.process=subprocess.Popen(argv,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=out,start_new_session=True)
        self.buffer=b'';self.serial=0;self.pending=None
        self.request('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'paired-scripted-operations','version':'1'}})
        self.process.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n');self.process.stdin.flush()
    def request(self,method,params,timeout=60):
        assert self.pending is None,'do not send another request while delivery is unknown'
        self.serial+=1;self.pending=self.serial
        self.process.stdin.write((json.dumps({'jsonrpc':'2.0','id':self.serial,'method':method,'params':params},ensure_ascii=False)+'\n').encode());self.process.stdin.flush()
        return self.wait(timeout)
    def wait(self,timeout):
        until=time.monotonic()+timeout
        while time.monotonic()<until:
            while b'\n' in self.buffer:
                line,self.buffer=self.buffer.split(b'\n',1);reply=json.loads(line)
                if reply.get('id')==self.pending:self.pending=None;return reply
            if select.select([self.process.stdout],[],[],max(0,until-time.monotonic()))[0]:
                data=os.read(self.process.stdout.fileno(),65536)
                if not data:raise RuntimeError('MCP process exited with a pending response')
                self.buffer+=data
        raise TimeoutError('MCP operation deadline; do not replay')
    def close(self):
        if self.pending is not None:
            try:self.wait(60)
            except Exception:pass
        if self.process.poll() is None:
            self.process.stdin.close()
            try:self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:stop(self.process)


def payload(reply):
    result=reply.get('result',{});text='\n'.join(x.get('text','') for x in result.get('content',[]) if x.get('type')=='text')
    try:body=json.loads(text)
    except ValueError:body=result.get('structuredContent',{})
    return result,text,body

