#!/usr/bin/env python3
"""Runtime preparation is CI-only and never treats failed observation as absence."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
ROOT=Path(__file__).resolve().parents[1]
RUNTIME='com.apple.CoreSimulator.SimRuntime.iOS-18-2'
FAKE='''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
root=Path(os.environ['FIXTURE_ROOT']);mode=os.environ['FIXTURE_MODE']
if Path(sys.argv[0]).name=='xcrun':
 if mode=='inventory-error':sys.exit(9)
 ready=mode=='ready' or (root/'installed').exists() and mode!='still-missing'
 rows=[] if mode not in ['ready','unavailable'] and not ready else [{'identifier':os.environ['FIXTURE_RUNTIME'],'isAvailable':mode!='unavailable'}]
 print(json.dumps({'runtimes':rows}));sys.exit(0)
with (root/'calls').open('a') as f:f.write(sys.argv[1]+'\\n')
if sys.argv[1]=='-downloadPlatform':
 dest=Path(sys.argv[sys.argv.index('-exportPath')+1]);dest.mkdir(parents=True);(dest/'runtime.dmg').touch()
elif sys.argv[1]=='-importPlatform':(root/'installed').touch()
else:sys.exit(99)
'''
class RuntimeSetup(unittest.TestCase):
 def invoke(self,mode,ci=True):
  with tempfile.TemporaryDirectory() as tmp:
   root=Path(tmp);bin=root/'bin';bin.mkdir()
   for name in ['xcrun','xcodebuild']:
    path=bin/name;path.write_text(FAKE);path.chmod(0o755)
   env=dict(os.environ,PATH=str(bin)+os.pathsep+os.environ['PATH'],FIXTURE_ROOT=tmp,FIXTURE_MODE=mode,FIXTURE_RUNTIME=RUNTIME,CI='true' if ci else 'false')
   p=subprocess.run(['bash',str(ROOT/'scripts/ensure-simulator-runtime.sh'),RUNTIME,'18.2',str(root/'out')],env=env,capture_output=True,text=True,timeout=10)
   return p.returncode,(root/'calls').read_text().splitlines() if (root/'calls').exists() else []
 def test_ready_runtime_is_not_reinstalled(self):self.assertEqual(self.invoke('ready'),(0,[]))
 def test_confirmed_absence_downloads_and_imports_once(self):self.assertEqual(self.invoke('missing'),(0,['-downloadPlatform','-importPlatform']))
 def test_error_or_unavailable_does_not_infer_reinstall(self):
  for mode in ['inventory-error','unavailable']:
   code,calls=self.invoke(mode);self.assertNotEqual(code,0);self.assertEqual(calls,[])
 def test_failed_readback_never_replays_install(self):
  code,calls=self.invoke('still-missing');self.assertNotEqual(code,0);self.assertEqual(calls,['-downloadPlatform','-importPlatform'])
 def test_personal_host_is_refused(self):self.assertEqual(self.invoke('missing',False),(2,[]))
if __name__=='__main__':unittest.main()
