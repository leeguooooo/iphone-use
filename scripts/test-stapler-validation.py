#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import subprocess
import unittest
spec=importlib.util.spec_from_file_location('stapler',Path(__file__).with_name('validate-stapled-artifact.py'));stapler=importlib.util.module_from_spec(spec);spec.loader.exec_module(stapler)
class Validation(unittest.TestCase):
 def execute(self,results):
  calls=[]
  def run(argv,**kwargs):
   calls.append(argv);code,text=results[min(len(calls)-1,len(results)-1)]
   return subprocess.CompletedProcess(argv,code,text,'')
  code=stapler.validate('/owned/iPhoneUse.app',run=run,sleep=lambda _:None)
  return code,len(calls)
 def test_clean_pass(self):self.assertEqual(self.execute([(0,'ticket valid')]),(0,1))
 def test_network_read_recovers(self):self.assertEqual(self.execute([(68,'Error Domain=NSURLErrorDomain Code=-1005 network connection lost'),(0,'valid')]),(0,2))
 def test_invalid_ticket_is_not_retried(self):self.assertEqual(self.execute([(68,'ticket data is invalid')]),(68,1))
 def test_network_failure_is_bounded_and_never_accepted(self):self.assertEqual(self.execute([(68,'Error Domain=NSURLErrorDomain Code=-1001 timeout')]),(68,3))
 def test_tls_or_other_error_does_not_masquerade_as_transient(self):
  for code,text in [(68,'Error Domain=NSURLErrorDomain Code=-1202 untrusted certificate'),(65,'NSURLErrorDomain Code=-1005')]:self.assertEqual(self.execute([(code,text)]),(code,1))
if __name__=='__main__':unittest.main()
