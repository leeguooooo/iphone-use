#!/usr/bin/env python3
"""Cleanup must observe ambiguous outcomes without replaying SDK mutations."""
import importlib.util,json,sys
from pathlib import Path
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parent))
spec=importlib.util.spec_from_file_location('owned_cleanup',Path(__file__).with_name('run-collect-simulator.py'))
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
from tempfile import TemporaryDirectory

UDID='11111111-2222-4333-8444-555555555555';NAME='owned-cleanup-fixture'

def exercise(states, failures=(), expected=('shutdown','delete'), success=True):
    actions=[];current=[0.0];sequence=iter(states);last=states[-1]
    def command(argv,**kwargs):
        assert argv==['xcrun','simctl','list','devices','--json']
        value=next(sequence,last)
        devices=[] if value is None else [{'udid':UDID,'name':value[1] if isinstance(value,tuple) else NAME,'state':value[0] if isinstance(value,tuple) else value}]
        return json.dumps({'devices':{'runtime':devices}})
    def logged(argv,*args,**kwargs):
        action=argv[2];actions.append(action)
        assert argv==['xcrun','simctl',action,UDID]
        if action in failures:
            error=failures[action] if isinstance(failures,dict) else TimeoutError
            raise error(action+' outcome unknown')
    def sleep(seconds):current[0]+=max(seconds,60)
    with TemporaryDirectory() as folder,patch.object(m,'command',command),patch.object(m,'logged',logged),patch.object(m.time,'monotonic',lambda:current[0]),patch.object(m.time,'sleep',sleep):
        try:r=m.cleanup_owned_simulator(UDID,NAME,{},Path(folder))
        except RuntimeError:
            assert not success
            r=json.loads((Path(folder)/'simulator-cleanup.json').read_text())
        else:assert success and r['passed'] and r['final_state']=='absent'
        assert tuple(actions)==expected,actions
        assert r['shutdown_attempts']<=1 and r['delete_attempts']<=1

exercise(['Booted','Shutdown',None])
exercise(['Shutting Down','Shutting Down','Shutdown',None],expected=('delete',))
exercise(['Booted','Shutting Down','Shutdown',None],failures=('shutdown',))
exercise(['Shutdown',None],expected=('delete',))
exercise([None],expected=())
exercise(['Shutdown',None],failures=('delete',),expected=('delete',))
exercise(['Booted'],failures=('shutdown',),expected=('shutdown',),success=False)
exercise(['Booted',None],failures={'shutdown':m.CommandTerminationUnverified},expected=('shutdown',),success=False)
exercise(['Booted',('Shutdown','someone-else')],expected=('shutdown',),success=False)
exercise(['Shutdown'],failures=('delete',),expected=('delete',),success=False)
exercise(['Shutdown',None],failures={'delete':m.CommandTerminationUnverified},expected=('delete',),success=False)
exercise([('Booted','someone-else')],expected=(),success=False)
exercise([(None,NAME)],expected=(),success=False)
print('Simulator cleanup: single shutdown/delete dispatch, ambiguous outcomes observed, foreign identity refused;fake SDK only')
