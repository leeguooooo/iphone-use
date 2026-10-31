#!/usr/bin/env python3
"""Receive an explicitly saved file from the EXISTING XCTest runner's Files folder."""
import json
from pathlib import Path, PurePosixPath
import signal
import sys
import tempfile
import uuid
import common as core
import publication

def relative_folder(value):
    parts = PurePosixPath(value).parts
    if not parts or parts[0] != 'Documents' or any(p in ('.', '..') for p in parts) or '\\' in value or '\x00' in value:
        raise core.TransferError('Source folder must be below Documents without traversal')
    if value != '/'.join(parts):
        raise core.TransferError('Source folder must be canonical and relative')
    return value

def file_metadata(value, filename, maximum):
    if not isinstance(value, dict) or not isinstance(value.get('result', {}).get('files'), list):
        raise core.TransferError('Unrecognized CoreDevice file listing')
    matches = [f for f in value['result']['files'] if f.get('name') == filename]
    if len(matches) != 1:
        raise core.TransferError('Expected exactly one explicitly named saved file')
    item = matches[0]
    resources, meta = item.get('resources', {}), item.get('metadata', {})
    if (item.get('relativePath') != filename or resources.get('isDirectory') is not False
            or resources.get('isSymbolicLink') is not False or resources.get('isReadable') is not True):
        raise core.TransferError('Only a readable regular saved file is supported')
    size = meta.get('size')
    if type(size) is not int or not 0 <= size <= maximum:
        raise core.TransferError('Invalid or oversized saved file')
    if not isinstance(meta.get('lastModDate'), str):
        raise core.TransferError('Missing source modification date')
    return {'size': size, 'modified': meta['lastModDate']}

class RunnerFiles(core.Device):
    def metadata(self):
        path = self.scratch/'files.json'
        self.command(['xcrun','devicectl','device','info','files','--device',self.args.device,
                      '--domain-type','appDataContainer','--domain-identifier',self.args.bundle,
                      '--subdirectory',self.args.folder,'--no-recurse','--json-output',str(path),'--timeout','15'])
        return file_metadata(core.read_json(path), self.args.filename, self.args.max_bytes)

def receive_saved(args, session, device, scratch, emit):
    device.guard()
    before = device.metadata()
    source = args.folder + '/' + args.filename
    first, second = scratch/'first', scratch/'second'
    device.copy('from', source, first)
    device.guard()
    if device.metadata() != before:
        raise core.TransferError('Source changed during first read')
    one = core.digest_file(first, args.max_bytes)
    if one[0] != before['size']:
        raise core.TransferError('Remote/local byte count mismatch')
    # No new on-phone hashing service: require two stable CoreDevice reads instead.
    device.copy('from', source, second)
    device.guard()
    if device.metadata() != before or core.digest_file(second, args.max_bytes) != one:
        raise core.TransferError('Source changed or USB reads differ')
    if args.expect_sha256 and one[1] != args.expect_sha256:
        raise core.TransferError('Expected SHA-256 mismatch')
    meta = {'version':1, 'id':getattr(args, 'transfer_id', None) or str(uuid.uuid4()), 'name':args.filename, 'bytes':one[0], 'sha256':one[1]}
    core.validate_manifest(meta, args.max_bytes)
    target, result = core.commit_file(first, meta, args.destination, session, device.guard)
    receipt = {**meta,'event':'received','committed':True,'result':result,'destination':str(target),
               'transport':'wired','clipboard_used':False,'receiver_app':'existing-xctest-runner',
               'verification':'remote-size-and-two-stable-reads','sha256_origin':'mac'}
    receipt['transfer_id'] = meta['id']
    if getattr(session, 'publication_warning', None):
        receipt['publication_warning'] = session.publication_warning
    try:
        logs = session.root/'receipts';logs.mkdir(exist_ok=True,mode=0o700)
        core.atomic_json(logs/(meta['id']+'.json'),receipt)
    except Exception as error:
        receipt['receipt_error'] = str(error)
    emit(receipt)
    return receipt

def main():
    p = core.parser()
    p.description = __doc__
    p.add_argument('--transfer-id', help='UUID for result reconciliation; reuse only to query result')
    p.add_argument('--folder', default='Documents/Send to Mac')
    p.add_argument('--filename', help='Exact filename selected only AFTER the native Save dialog completed')
    args = p.parse_args()
    if not args.bundle.endswith('.xctrunner'):
        p.error('This route only targets the existing XCTest runner (.xctrunner)')
    relative_folder(args.folder)
    if not 1 <= args.timeout <= 3600 or not 0 <= args.max_bytes <= 64*1024*1024:
        p.error('Invalid timeout or size limit')
    key = core.hashlib.sha256(json.dumps([args.device,args.bundle,args.instance]).encode()).hexdigest()[:24]
    root = args.state_dir.expanduser()/key
    emit = lambda data: print(json.dumps(data,ensure_ascii=False),flush=True)
    if args.action == 'result':
        if not args.transfer_id: p.error('result requires --transfer-id')
        emit(publication.reconcile(root, args.transfer_id));return
    if args.action == 'status':emit(core.Session.status(root));return
    if args.action == 'stop':emit({'cancel_requested':core.Session.cancel(root)});return
    if not args.owner or not args.destination or not args.filename:
        p.error('receive requires --owner, --destination and --filename after Save has completed')
    core.validate_manifest({'version':1,'id':str(uuid.uuid4()),'name':args.filename,'bytes':0,'sha256':'0'*64},args.max_bytes)
    if args.expect_sha256 and not core.re.fullmatch('[0-9a-f]{64}',args.expect_sha256):
        p.error('Invalid expected SHA-256')
    args.transfer_id = str(uuid.UUID(args.transfer_id)) if args.transfer_id else str(uuid.uuid4())
    if publication.record_path(root, args.transfer_id).exists():
        p.error('Transfer ID already exists; query result before starting a new transfer')
    args.destination = args.destination.expanduser().resolve()
    session = core.Session(root,args.timeout)
    def cancelled(*_):raise core.Cancelled('Receiver interrupted')
    for sig in (signal.SIGTERM,signal.SIGINT):signal.signal(sig,cancelled)
    acquired = False
    try:
        with session.acquire():
            acquired = True
            emit({'event':'started','transfer_id':args.transfer_id})
            with tempfile.TemporaryDirectory(prefix='runner-files-',dir=root) as temp:
                scratch=Path(temp)
                receive_saved(args,session,RunnerFiles(args,session,scratch),scratch,emit)
    except Exception as error:
        if getattr(session, 'publication_id', None):
            try:
                outcome = publication.reconcile(root, session.publication_id)
            except Exception:
                outcome = {'committed':None,'outcome':'unknown','transfer_id':session.publication_id}
            emit({'event':'publication-result', **outcome, 'interrupted_by':str(error)})
            if outcome['committed'] is True:
                return
        raise
    finally:
        if acquired:emit({'event':'session-ended','receiver':core.Session.status(root),'system_clipboard':'unchanged'})

if __name__=='__main__':
    try:main()
    except Exception as error:
        print(json.dumps({'event':'error','detail':str(error)}),file=sys.stderr)
        sys.exit(1)
