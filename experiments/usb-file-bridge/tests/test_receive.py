import importlib.util
import json
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import types
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).parents[1] / 'mac'))

spec = importlib.util.spec_from_file_location('bridge', Path(__file__).parents[1] / 'mac/common.py')
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)

def lease_status(owner='me', udid='phone'):
    return {'drivable':True,'owner':owner,'udid':udid,'control_lease':{
        'owner':owner,'daemon_instance':'a'*64,'owner_epoch':'1','privacy_epoch':'1','lifecycle_stamp':'0','private':False}}

def metadata(data=b'file bytes', **changes):
    return {'version': 1, 'id': str(uuid.uuid4()), 'name': 'invoice.pdf', 'bytes': len(data),
            'sha256': hashlib.sha256(data).hexdigest(), **changes}

class MetadataTests(unittest.TestCase):
    def test_invalid_fields(self):
        for change in ({'version': 2}, {'id': '../bad'}, {'name': '../bad'}, {'name': 'a\\b'},
                       {'name': '\x00'}, {'name': '.'}, {'name': 'x'*241}, {'bytes': True},
                       {'bytes': -1}, {'sha256': 'wrong'}):
            with self.subTest(change=change), self.assertRaises(b.TransferError):
                b.validate_manifest(metadata(**change), 1024)

    def test_json_metadata_limit(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d)/'metadata'
            path.write_text(' ' * 65537)
            with self.assertRaises(b.TransferError): b.read_json(path)

class SessionTests(unittest.TestCase):
    def test_lock_cancel_cleanup_and_stale_state(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            first, second = b.Session(root, 10), b.Session(root, 10)
            with first.acquire():
                self.assertTrue(b.Session.status(root)['active'])
                with self.assertRaisesRegex(b.TransferError, 'Another receiver'):
                    with second.acquire(): pass
                self.assertTrue(b.Session.cancel(root))
                with self.assertRaises(b.Cancelled): first.check()
            self.assertFalse(b.Session.status(root)['active'])
            (root/'session.json').write_text('{"pid":1}')
            self.assertFalse(b.Session.status(root)['active'])
            self.assertFalse(b.Session.cancel(root))

    def test_old_cancel_token_does_not_stop_new_session(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root/('cancel-'+str(uuid.uuid4()))).touch()
            with b.Session(root, 10).acquire() as session: self.assertGreater(session.check(), 0)

class DeviceTests(unittest.TestCase):
    def test_guard_transport_owner_and_target(self):
        for transport, owner, udid, accepted in [('wired','me','phone',True),
                ('wireless','me','phone',False),('wired','other','phone',False),('wired','me','other',False)]:
            with self.subTest(transport=transport,owner=owner,udid=udid), tempfile.TemporaryDirectory() as d:
                root=Path(d)
                args=types.SimpleNamespace(device='phone',owner='me',instance='default')
                dev=b.Device(args,b.Session(root,10),root)
                def command(argv, *rest):
                    if argv[0]=='iphone-use':return json.dumps(lease_status(owner, udid))
                    (root/'device.json').write_text(json.dumps({'result':{'connectionProperties':{'transportType':transport}}}))
                    return ''
                with patch.object(dev,'command',side_effect=command):
                    if accepted: dev.guard()
                    else:
                        with self.assertRaises(b.TransferError):dev.guard()

    def test_lease_change_during_transport_probe_is_rejected(self):
        changes = ({'owner': 'new-owner'}, {'owner': None},
                   {'udid': 'other-phone'}, {'drivable': False},
                   {'control_lease':{**lease_status()['control_lease'],'owner_epoch':'2'}})
        for change in changes:
            with self.subTest(change=change), tempfile.TemporaryDirectory() as d:
                root = Path(d)
                state = lease_status()
                args = types.SimpleNamespace(device='phone', owner='me', instance='default')
                device = b.Device(args, b.Session(root, 10), root)
                def command(argv, *rest):
                    if argv[0] == 'iphone-use':
                        return json.dumps(state)
                    state.update(change)  # Ownership changes while the blocking probe runs.
                    (root/'device.json').write_text(json.dumps({
                        'result': {'connectionProperties': {'transportType': 'wired'}}}))
                    return ''
                with patch.object(device, 'command', side_effect=command):
                    with self.assertRaises(b.TransferError):
                        device.guard()

    def test_commit_rechecks_real_guard_after_transport_probe(self):
        for existing in (False, True):
            with self.subTest(existing=existing), tempfile.TemporaryDirectory() as d:
                root = Path(d)
                source = root/'source'
                source.write_bytes(b'file bytes')
                dest = root/'output'
                dest.mkdir()
                target = dest/'invoice.pdf'
                if existing:
                    target.write_bytes(source.read_bytes())
                state = lease_status()
                args = types.SimpleNamespace(device='phone', owner='me', instance='default')
                session = b.Session(root/'state', 10)
                device = b.Device(args, session, root)
                def command(argv, *rest):
                    if argv[0] == 'iphone-use':
                        return json.dumps(state)
                    state['owner'] = 'new-owner'
                    (root/'device.json').write_text(json.dumps({
                        'result': {'connectionProperties': {'transportType': 'wired'}}}))
                    return ''
                with session.acquire(), patch.object(device, 'command', side_effect=command):
                    with self.assertRaises(b.TransferError):
                        b.commit_file(source, metadata(), dest, session, device.guard)
                self.assertEqual(target.exists(), existing)
                if existing:
                    self.assertEqual(target.read_bytes(), b'file bytes')
                self.assertEqual(list(dest.glob('*.partial')), [])
                self.assertFalse(b.Session.status(session.root)['active'])

    def test_copy_boundary_checks_lease_before_device_access(self):
        for change in ({'owner': 'new-owner'}, {'udid': 'other-phone'}, {'drivable': False}):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as d:
                root = Path(d)
                state = {**lease_status(), **change}
                args = types.SimpleNamespace(device='phone', owner='me', instance='default', bundle='test.xctrunner')
                device = b.Device(args, b.Session(root, 10), root)
                operations = []
                def command(argv, *rest):
                    if argv[0] == 'iphone-use':
                        return json.dumps(state)
                    operations.append(argv)
                    return ''
                with patch.object(device, 'command', side_effect=command):
                    with self.assertRaises(b.TransferError):
                        device.copy('from', 'Documents/sample.pdf', root/'received')
                self.assertEqual(operations, [])

    def test_command_timeout_bounded_and_cancel_checked(self):
        with tempfile.TemporaryDirectory() as d:
            session=b.Session(Path(d),2)
            dev=b.Device(None,session,Path(d))
            with patch.object(b.subprocess,'run',side_effect=subprocess.TimeoutExpired('cmd',2)) as run:
                with self.assertRaisesRegex(b.TransferError,'timed out'):dev.command(['cmd'])
                self.assertLessEqual(run.call_args.kwargs['timeout'],2)


class CliTests(unittest.TestCase):
    def test_busy_error_is_json_at_actual_entrypoint(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            key = hashlib.sha256(json.dumps(['test-device','test-bundle.xctrunner','default']).encode()).hexdigest()[:24]
            with b.Session(root/key, 10).acquire():
                command = [sys.executable, str(Path(b.__file__).with_name('runner_files.py')), 'receive', '--device', 'test-device',
                           '--bundle', 'test-bundle.xctrunner', '--owner', 'test-owner',
                           '--destination', str(root/'out'), '--filename', 'sample.pdf', '--state-dir', str(root)]
                result = subprocess.run(command, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(json.loads(result.stderr)['event'], 'error')
                self.assertNotIn('Traceback', result.stderr)
                self.assertFalse((root/'out').exists())

    def test_idle_status_does_not_contact_device(self):
        with tempfile.TemporaryDirectory() as d:
            result = subprocess.run([sys.executable, str(Path(b.__file__).with_name('runner_files.py')), 'status',
                '--device', 'test-device', '--bundle', 'test-bundle.xctrunner', '--state-dir', d],
                capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0)
            self.assertFalse(json.loads(result.stdout)['active'])


class PackagedHostTests(unittest.TestCase):
    def test_lease_query_uses_explicit_current_binary(self):
        args=types.SimpleNamespace(instance='chosen',owner='me',device='phone')
        device=b.Device(args,None,None)
        with patch.dict(os.environ, {'IPHONE_USE_FILES_HOST_BIN':'/owned/current iphone-use'}), \
             patch.object(device,'command',return_value=json.dumps(lease_status())) as command:
            device.check_lease()
        self.assertEqual(command.call_args.args[0], ['/owned/current iphone-use','status','--instance','chosen','--json'])

    def test_environment_instance_and_explicit_override(self):
        base=['status','--device','phone','--bundle','owned.xctrunner']
        with patch.dict(os.environ, {'IPHONE_USE_INSTANCE':'chosen'}):
            self.assertEqual(b.parser().parse_args(base).instance,'chosen')
            self.assertEqual(b.parser().parse_args(base+['--instance','other']).instance,'other')

class LeaseGenerationTests(unittest.TestCase):
    def test_same_owner_new_epoch_and_restarted_daemon_are_refused(self):
        for field,new in [('owner_epoch','2'),('daemon_instance','b'*64),('privacy_epoch','2'),('lifecycle_stamp','256')]:
            with self.subTest(field=field):
                args=types.SimpleNamespace(instance='default',owner='me',device='phone')
                state={'drivable':True,'owner':'me','udid':'phone', 'control_lease':{
                    'owner':'me','daemon_instance':'a'*64,'owner_epoch':'1','privacy_epoch':'1','lifecycle_stamp':'0','private':False}}
                device=b.Device(args,None,None)
                with patch.object(device,'command',side_effect=lambda *a:json.dumps(state)):
                    device.check_lease()
                    state['control_lease'][field]=new
                    with self.assertRaises(b.TransferError):device.check_lease()

    def test_unknown_identity_or_private_state_is_not_a_live_lease(self):
        for change in [{'control_lease':None},{'control_lease':{}}, {'drivable':'true'},
                       {'control_lease':{'owner':'me','daemon_instance':'a'*64,'owner_epoch':'1','privacy_epoch':'1','private':True}}]:
            with self.subTest(change=change):
                state=lease_status();state.update(change)
                device=b.Device(types.SimpleNamespace(instance='default',owner='me',device='phone'),None,None)
                with patch.object(device,'command',return_value=json.dumps(state)), self.assertRaises(b.TransferError):
                    device.check_lease()

class InvalidStatusTests(unittest.TestCase):
    def test_malformed_or_nonobject_status_refuses(self):
        for response in ['not-json','null','[]','true']:
            device=b.Device(types.SimpleNamespace(instance='default',owner='me',device='phone'),None,None)
            with self.subTest(response=response), patch.object(device,'command',return_value=response), self.assertRaises(b.TransferError):
                device.check_lease()

if __name__ == '__main__': unittest.main()
