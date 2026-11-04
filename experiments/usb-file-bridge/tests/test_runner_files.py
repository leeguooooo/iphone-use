import hashlib
import io
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
    def test_staging_never_writes_beyond_declared_size(self):
        for maximum in [0, 3, c.CHUNK + 3]:
            with self.subTest(maximum=maximum):
                incoming=io.BytesIO(b'x'*(maximum+1));output=io.BytesIO()
                with self.assertRaisesRegex(c.TransferError, 'grew'):
                    c.copy_bounded(incoming,output,maximum,lambda:None)
                self.assertLessEqual(len(output.getvalue()),maximum)
                self.assertLessEqual(incoming.tell(),maximum+1)
    def test_staging_exact_boundary_and_cancellation(self):
        for size in [0, 3, c.CHUNK + 3]:
            output=io.BytesIO();payload=b'x'*size
            self.assertEqual(c.copy_bounded(io.BytesIO(payload),output,size,lambda:None),size)
            self.assertEqual(output.getvalue(),payload)
        output=io.BytesIO()
        def cancel():raise c.Cancelled('stopped')
        with self.assertRaises(c.Cancelled):c.copy_bounded(io.BytesIO(b'x'),output,1,cancel)
        self.assertEqual(output.getvalue(),b'')
    def test_source_growth_before_staging_never_publishes(self):
        source=self.scratch/'growing';source.write_bytes(b'x'*100)
        meta={'version':1,'id':str(c.uuid.uuid4()),'name':'growth.bin','bytes':3,
              'sha256':hashlib.sha256(b'xxx').hexdigest()}
        with self.session.acquire(),self.assertRaisesRegex(c.TransferError,'grew'):
            c.commit_file(source,meta,self.args.destination,self.session,self.guard)
        self.assertFalse((self.args.destination/'growth.bin').exists())
        self.assertEqual(list(self.args.destination.glob('.usb-*')),[])
        self.assertFalse((self.session.root/'transactions').exists())
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
class NativeProviderTests(unittest.TestCase):
    def setUp(self):
        self.args=types.SimpleNamespace(device='phone',bundle='owned.Runner.xctrunner',
            owner='me',instance='i13',max_bytes=100)
        self.device=r.RunnerFiles(self.args,None,None)
        self.device.lease_identity=('a'*64,'1','2','3')
    def test_native_copy_passes_exact_scope_to_current_packaged_binary(self):
        with patch.object(self.device,'check_lease') as guard,patch.object(self.device,'command',
                return_value=json.dumps({'provider':'native-usb-afc','bytes':5})) as command, \
                patch.dict(r.os.environ,{'IPHONE_USE_FILES_HOST_BIN':'/owned/iphone-use'}):
            self.device.copy('from','Documents/inbox/saved.pdf',Path('/private/first'))
        guard.assert_called_once()
        argv,timeout=command.call_args.args
        self.assertEqual(argv[:3],['/owned/iphone-use','files','__native-read'])
        self.assertEqual(argv[argv.index('--instance')+1],'i13')
        self.assertEqual(argv[argv.index('--owner')+1],'me')
        self.assertEqual(json.loads(argv[argv.index('--lease')+1]),
            {'daemon_instance':'a'*64,'owner_epoch':'1','privacy_epoch':'2','lifecycle_stamp':'3'})
        self.assertEqual(timeout,60)
    def test_native_failure_or_invalid_receipt_never_falls_back(self):
        for result in ['invalid','[]',json.dumps({'provider':'other','bytes':5}),
                json.dumps({'provider':'native-usb-afc','bytes':True}),
                json.dumps({'provider':'native-usb-afc','bytes':101})]:
            with self.subTest(result=result),patch.object(self.device,'check_lease'), \
                    patch.object(self.device,'command',return_value=result) as command:
                with self.assertRaises(c.TransferError):
                    self.device.copy('from','Documents/inbox/saved.pdf',Path('/private/first'))
                command.assert_called_once()
        with patch.object(self.device,'check_lease'),patch.object(self.device,'command',
                side_effect=c.TransferError('native refused')) as command:
            with self.assertRaises(c.TransferError):
                self.device.copy('from','Documents/inbox/saved.pdf',Path('/private/first'))
            command.assert_called_once()

if __name__=='__main__':unittest.main()
