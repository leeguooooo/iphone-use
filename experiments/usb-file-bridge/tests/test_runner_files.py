import hashlib
import json
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).parents[1]/'mac'))
import runner_files as r
import common as c
from enable_runner_files import file_sharing_info

class NativeFilesTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.scratch=self.root/'scratch';self.scratch.mkdir()
        self.args=types.SimpleNamespace(folder='Documents/Send to Mac',filename='sample.pdf',max_bytes=100,
                                       expect_sha256=None,destination=self.root/'out')
        self.session=c.Session(self.root/'state',10)
        self.payload=b'saved-file';self.copies=0;self.metadata_calls=0
        self.changed=False;self.bad_second=False;self.released=False;self.fail_guard=False
        self.device=types.SimpleNamespace(guard=self.guard,metadata=self.metadata,copy=self.copy)
    def guard(self):
        self.session.check()
        if self.fail_guard or (self.released and self.copies):raise c.TransferError('lease ended')
    def metadata(self):
        self.metadata_calls+=1
        return {'size':len(self.payload),'modified':'changed' if self.changed and self.metadata_calls>1 else 'fixed'}
    def copy(self,direction,source,target):
        self.copies+=1
        target.write_bytes(b'different' if self.bad_second and self.copies==2 else self.payload)
    def receive(self):
        with self.session.acquire():return r.receive_saved(self.args,self.session,self.device,self.scratch,lambda _:None)
    def test_native_file_success(self):
        result=self.receive()
        self.assertTrue(result['committed']);self.assertEqual(result['sha256_origin'],'mac')
        self.assertEqual((self.args.destination/'sample.pdf').read_bytes(),self.payload)
        self.assertFalse(c.Session.status(self.session.root)['active'])
    def test_source_changes_abort(self):
        self.changed=True
        with self.assertRaisesRegex(c.TransferError,'changed'):self.receive()
        self.assertFalse(self.args.destination.exists())
    def test_two_reads_differ_abort(self):
        self.bad_second=True
        with self.assertRaisesRegex(c.TransferError,'differ'):self.receive()
        self.assertFalse(self.args.destination.exists())
    def test_expected_hash_checked(self):
        self.args.expect_sha256='0'*64
        with self.assertRaisesRegex(c.TransferError,'SHA-256'):self.receive()
    def test_owner_lost_before_commit(self):
        self.released=True
        with self.assertRaisesRegex(c.TransferError,'lease'):self.receive()
        self.assertFalse(self.args.destination.exists())
    def test_different_destination_not_overwritten(self):
        self.args.destination.mkdir();target=self.args.destination/'sample.pdf';target.write_bytes(b'original')
        with self.assertRaises(c.TransferError):self.receive()
        self.assertEqual(target.read_bytes(),b'original')
    def test_symlink_destination_rejected(self):
        self.args.destination.mkdir();target=self.args.destination/'sample.pdf';target.symlink_to(self.root/'other')
        with self.assertRaises(c.TransferError):self.receive()
    def test_existing_identical_file_idempotent(self):
        self.args.destination.mkdir();(self.args.destination/'sample.pdf').write_bytes(self.payload)
        self.assertEqual(self.receive()['result'],'already_identical')
    def test_commit_race_does_not_overwrite(self):
        real=c.os.link
        def link(stage,target):target.write_bytes(b'other');return real(stage,target)
        with patch.object(c.os,'link',side_effect=link),self.assertRaises(FileExistsError):self.receive()
        self.assertEqual((self.args.destination/'sample.pdf').read_bytes(),b'other')
        self.assertEqual(list(self.args.destination.glob('*.partial')),[])
    def test_receipt_failure_after_commit_reported(self):
        real=c.atomic_json
        def write(path,value):
            if path.parent.name=='receipts':raise OSError('full')
            return real(path,value)
        with patch.object(c,'atomic_json',side_effect=write):result=self.receive()
        self.assertTrue(result['committed']);self.assertIn('receipt_error',result)
    def test_size_limit(self):
        self.args.max_bytes=1
        with self.assertRaises(c.TransferError):self.receive()
    def test_folder_traversal_rejected(self):
        for folder in ['/Documents/a','Documents/../a','Documents//a','Documents/a/','Library/a','Documents/./a']:
            with self.subTest(folder=folder),self.assertRaises(c.TransferError):r.relative_folder(folder)
    def test_remote_metadata_contract(self):
        item={'name':'sample.pdf','relativePath':'sample.pdf','metadata':{'size':10,'lastModDate':'date'},
              'resources':{'isDirectory':False,'isSymbolicLink':False,'isReadable':True}}
        value={'result':{'files':[item]}}
        self.assertEqual(r.file_metadata(value,'sample.pdf',100)['size'],10)
        for resource in ['isDirectory','isSymbolicLink']:
            bad=json.loads(json.dumps(value));bad['result']['files'][0]['resources'][resource]=True
            with self.assertRaises(c.TransferError):r.file_metadata(bad,'sample.pdf',100)
        with self.assertRaises(c.TransferError):r.file_metadata(value,'missing.pdf',100)
        with self.assertRaises(c.TransferError):r.file_metadata(value,'sample.pdf',1)
    def test_plist_patch_preserves_identity_and_icon(self):
        source={'CFBundleIdentifier':'example.runner.xctrunner','CFBundleIcons':{'original':'icon'},'Other':True}
        result=file_sharing_info(source)
        self.assertTrue(result['UIFileSharingEnabled']);self.assertTrue(result['LSSupportsOpeningDocumentsInPlace'])
        self.assertEqual(result['CFBundleIcons'],source['CFBundleIcons'])
        self.assertNotIn('UIFileSharingEnabled',source)
        with self.assertRaises(ValueError):file_sharing_info({'CFBundleIdentifier':'other.app'})
if __name__=='__main__':unittest.main()
