"""Recoverable publication intent. Does not claim atomic phone-lease fencing."""
import os
from pathlib import Path
import stat
import uuid

import common


def record_path(root, transfer_id):
    ident = str(uuid.UUID(transfer_id))
    return root / 'transactions' / (ident + '.json')


def prepare(session, meta, target, identity):
    path = record_path(session.root, meta['id'])
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    common.sync_directory(path.parent.parent)
    if path.exists():
        raise common.TransferError('Transfer ID already has a publication record; query result first')
    record = {'version': 1, 'state': 'prepared', 'meta': meta,
              'destination': str(target.resolve()), 'identity': identity,
              'result': 'already_identical' if identity['existing'] else 'copied'}
    common.atomic_json(path, record)
    session.publication_id = meta['id']
    return path, record


def file_stamp(info):
    return (info.st_dev, info.st_ino, info.st_mode, info.st_size,
            info.st_mtime_ns, info.st_ctime_ns)


def verified_contents(target, expected, meta):
    # Keep identity and digest attached to one opened regular file. A path can
    # change independently while it is being read, so check its binding too.
    fd = os.open(target, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        before = os.fstat(stream.fileno())
        if (not stat.S_ISREG(before.st_mode) or before.st_dev != expected['device']
                or before.st_ino != expected['inode']):
            return False
        if file_stamp(target.lstat()) != file_stamp(before):
            return False
        digest = common.digest_stream(stream, meta['bytes'])
        after = os.fstat(stream.fileno())
        return (digest == (meta['bytes'], meta['sha256'])
                and file_stamp(before) == file_stamp(after) == file_stamp(target.lstat()))


def reconcile(root, transfer_id):
    """Read-only observation: never copies, removes, or retries publication."""
    record = common.read_json(record_path(root, transfer_id))
    meta = common.validate_manifest(record['meta'], 64 * 1024 * 1024)
    target = Path(record['destination'])
    result = {'transfer_id': transfer_id, 'destination': str(target), 'name': meta['name'],
              'bytes': meta['bytes'], 'sha256': meta['sha256'], 'committed': None,
              'outcome': 'unknown', 'record_state': record['state']}
    try:
        info = target.lstat()
    except FileNotFoundError:
        # No commit syscall is attempted until 'committing' is persisted.
        if record['state'] in ('prepared', 'aborted'):
            result.update(committed=False, outcome='not_published')
        return result
    except OSError as error:
        result['inspection_error'] = str(error)
        return result
    expected = record['identity']
    if (stat.S_ISREG(info.st_mode) and info.st_dev == expected['device']
            and info.st_ino == expected['inode']):
        try:
            matches = verified_contents(target, expected, meta)
        except (OSError, common.TransferError) as error:
            result['inspection_error'] = str(error)
            matches = False
        if matches:
            result.update(committed=True, outcome=record['result'])
    return result


def mark_committing(path, record):
    record['state'] = 'committing'
    common.atomic_json(path, record)


def mark_committed(path, record, warning=None):
    record['state'] = 'committed'
    if warning:
        record['warning'] = warning
    common.atomic_json(path, record)


def identity(path, existing=False):
    info = path.stat()
    return {'device': info.st_dev, 'inode': info.st_ino, 'existing': existing}
