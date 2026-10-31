import hashlib
import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid
sys.path.insert(0, str(Path(__file__).parents[1]/'mac'))
import common as c
import publication as p

class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root/'source'; self.source.write_bytes(b'synthetic bytes')
        self.meta = {'version':1,'id':str(uuid.uuid4()),'name':'sample.pdf','bytes':15,
                     'sha256':hashlib.sha256(b'synthetic bytes').hexdigest()}
        self.session = c.Session(self.root/'state',10)
        self.dest = self.root/'out'

    def test_cancel_after_link_reports_committed(self):
        real = c.os.link
        def interrupt(a,b):
            real(a,b)
            raise c.Cancelled('cancel after successful link')
        with self.session.acquire(), patch.object(c.os,'link',side_effect=interrupt):
            target,result = c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)
        self.assertEqual(result,'copied')
        self.assertTrue(p.reconcile(self.session.root,self.meta['id'])['committed'])
        self.assertIn('cancel',self.session.publication_warning)
        self.assertEqual(target.read_bytes(),b'synthetic bytes')
        self.assertEqual(list(self.dest.glob('*.partial')),[])

    def test_abort_before_publish_reports_not_published(self):
        def refuse():raise c.TransferError('lease revoked')
        with self.session.acquire(),self.assertRaises(c.TransferError):
            c.commit_file(self.source,self.meta,self.dest,self.session,refuse)
        outcome=p.reconcile(self.session.root,self.meta['id'])
        self.assertFalse(outcome['committed'])
        self.assertEqual(outcome['outcome'],'not_published')
        self.assertFalse((self.dest/'sample.pdf').exists())

    def test_crash_after_link_is_queryable_from_new_process(self):
        # A hard process exit skips all Python cleanup and receipt writing.
        key=hashlib.sha256(json.dumps(['test','test.xctrunner','default']).encode()).hexdigest()[:24]
        state=self.root/'states'/key
        code='''
import sys,os,json
from pathlib import Path
sys.path.insert(0,sys.argv[1])
import common as c
root=Path(sys.argv[2]);meta=json.loads(sys.argv[3])
session=c.Session(root,10)
real=c.os.link
def crash(a,b):
    real(a,b)
    os._exit(87)
c.os.link=crash
with session.acquire():
    c.commit_file(Path(sys.argv[4]),meta,Path(sys.argv[5]),session,lambda:None)
'''
        run=subprocess.run([sys.executable,'-c',code,str(Path(c.__file__).parent),str(state),
                            json.dumps(self.meta),str(self.source),str(self.dest)],timeout=10)
        self.assertEqual(run.returncode,87)
        queried=subprocess.run([sys.executable,str(Path(c.__file__).with_name('runner_files.py')),
            'result','--device','test','--bundle','test.xctrunner','--state-dir',str(self.root/'states'),
            '--transfer-id',self.meta['id']],capture_output=True,text=True,timeout=5)
        self.assertEqual(queried.returncode,0,queried.stderr)
        self.assertTrue(json.loads(queried.stdout)['committed'])
        self.assertFalse(c.Session.status(state)['active'])

    def test_journal_completion_failure_does_not_erase_success(self):
        with self.session.acquire(),patch.object(p,'mark_committed',side_effect=OSError('disk full')):
            target,result=c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)
        self.assertEqual(result,'copied')
        self.assertTrue(p.reconcile(self.session.root,self.meta['id'])['committed'])

    def test_replaced_target_is_unknown_not_success(self):
        with self.session.acquire():
            target,_=c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)
        target.rename(self.dest/'keep-original')
        target.write_bytes(b'synthetic bytes')
        self.assertIsNone(p.reconcile(self.session.root,self.meta['id'])['committed'])

    def test_modified_in_place_is_unknown(self):
        with self.session.acquire():
            target,_=c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)
        with target.open('ab') as stream:stream.write(b'changed later')
        self.assertIsNone(p.reconcile(self.session.root,self.meta['id'])['committed'])

    def test_transfer_id_reuse_is_not_silently_overwritten(self):
        with self.session.acquire():
            c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)
        with self.session.acquire(),self.assertRaisesRegex(c.TransferError,'already has'):
            c.commit_file(self.source,self.meta,self.dest,self.session,lambda:None)

    def test_main_recovers_cancel_between_commit_and_receipt(self):
        import runner_files as runner
        def completed_then_cancelled(args,session,device,scratch,emit):
            c.commit_file(self.source,self.meta,self.dest,session,lambda:None)
            raise c.Cancelled('cancel before success receipt')
        argv=['runner_files.py','receive','--device','test','--bundle','test.xctrunner',
              '--owner','test','--filename','sample.pdf','--destination',str(self.dest),
              '--state-dir',str(self.root/'cli-state'),'--transfer-id',self.meta['id']]
        output=io.StringIO()
        with patch.object(sys,'argv',argv),patch.object(runner,'receive_saved',side_effect=completed_then_cancelled), \
                patch.object(runner.signal,'signal'),contextlib.redirect_stdout(output):
            runner.main()
        events=[json.loads(line) for line in output.getvalue().splitlines()]
        recovered=next(e for e in events if e['event']=='publication-result')
        self.assertTrue(recovered['committed'])
        self.assertFalse(events[-1]['receiver']['active'])

    def test_uncertain_attempt_does_not_claim_not_published(self):
        self.dest.mkdir()
        path,record=p.prepare(self.session,self.meta,self.dest/'sample.pdf',p.identity(self.source))
        p.mark_committing(path,record)
        outcome=p.reconcile(self.session.root,self.meta['id'])
        self.assertIsNone(outcome['committed'])
        self.assertEqual(outcome['outcome'],'unknown')

    def test_result_id_path_traversal_refused(self):
        with self.assertRaises(ValueError):p.record_path(self.root,'../escape')


class ReconciliationRaceTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.source=self.root/'source';self.source.write_bytes(b'synthetic bytes')
        self.meta={'version':1,'id':str(uuid.uuid4()),'name':'sample.pdf','bytes':15,
                   'sha256':hashlib.sha256(self.source.read_bytes()).hexdigest()}
        self.session=c.Session(self.root/'state',10)
        with self.session.acquire():self.target,_=c.commit_file(self.source,self.meta,self.root/'out',self.session,lambda:None)
        self.target=self.target.resolve()

    def test_replacement_between_identity_check_and_hash_is_unknown(self):
        self.target.write_bytes(b'corrupted bytes')
        original=Path.lstat;swapped=[]
        def swap_after_stat(path,*args,**kwargs):
            info=original(path,*args,**kwargs)
            if path==self.target and not swapped:
                swapped.append(True);path.rename(self.root/'old-corrupt');path.write_bytes(self.source.read_bytes())
            return info
        with patch.object(Path,'lstat',swap_after_stat):result=p.reconcile(self.session.root,self.meta['id'])
        self.assertTrue(swapped)
        self.assertIsNone(result['committed'])

    def test_replacement_during_digest_is_unknown(self):
        original=c.digest_stream;swapped=[]
        def swap_after_read(stream,maximum):
            result=original(stream,maximum)
            if not swapped:
                swapped.append(True);self.target.rename(self.root/'old-target');self.target.write_bytes(self.source.read_bytes())
            return result
        with patch.object(c,'digest_stream',side_effect=swap_after_read):result=p.reconcile(self.session.root,self.meta['id'])
        self.assertTrue(swapped)
        self.assertIsNone(result['committed'])

    def test_in_place_change_during_digest_is_unknown(self):
        original=c.digest_stream;changed=[]
        def change_after_read(stream,maximum):
            result=original(stream,maximum)
            changed.append(True);self.target.write_bytes(b'changed during the digest')
            return result
        with patch.object(c,'digest_stream',side_effect=change_after_read):result=p.reconcile(self.session.root,self.meta['id'])
        self.assertTrue(changed)
        self.assertIsNone(result['committed'])

if __name__=='__main__':unittest.main()
