#!/usr/bin/env python3
"""Shared lease, transport, integrity, and atomic-publication helpers."""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time
import uuid

CHUNK = 1024 * 1024

class TransferError(RuntimeError):
    pass

class Cancelled(TransferError):
    pass

def atomic_json(path, value):
    fd, temporary = tempfile.mkstemp(prefix='.json-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, ensure_ascii=False)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        Path(temporary).unlink(missing_ok=True)

def sync_directory(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def read_json(path):
    if path.stat().st_size > 64 * 1024:
        raise TransferError('JSON metadata exceeds 64 KiB')
    return json.loads(path.read_text())

def validate_manifest(value, max_bytes):
    if not isinstance(value, dict) or value.get('version') != 1:
        raise TransferError('Unsupported transfer manifest')
    ident = value.get('id')
    if not isinstance(ident, str):
        raise TransferError('Missing transfer id')
    try:
        if str(uuid.UUID(ident)).lower() != ident.lower():
            raise ValueError()
    except ValueError:
        raise TransferError('Invalid transfer id') from None
    name = value.get('name')
    if (not isinstance(name, str) or name in ('', '.', '..')
            or any(c in name for c in '/\\') or len(name.encode('utf-8')) > 240
            or any(ord(c) < 32 or ord(c) == 127 for c in name)):
        raise TransferError('Invalid file name')
    size = value.get('bytes')
    if type(size) is not int or not 0 <= size <= max_bytes:
        raise TransferError('Invalid or oversized file')
    if not isinstance(value.get('sha256'), str) or not re.fullmatch('[0-9a-f]{64}', value['sha256']):
        raise TransferError('Invalid SHA-256')
    return {k: value[k] for k in ('version', 'id', 'name', 'bytes', 'sha256')}

def digest_stream(stream, maximum):
    digest = hashlib.sha256()
    total = 0
    while data := stream.read(CHUNK):
        total += len(data)
        if total > maximum:
            raise TransferError('File exceeds size limit')
        digest.update(data)
    return total, digest.hexdigest()


def digest_file(path, maximum):
    with path.open('rb') as stream:
        return digest_stream(stream, maximum)

class Session:
    """OS lock is authoritative; cancellation is token-based, never kill-by-PID."""
    def __init__(self, root, seconds):
        self.root = root
        self.seconds = seconds
        self.token = str(uuid.uuid4())
        self.deadline = time.monotonic() + seconds
        self.lock = None

    @contextlib.contextmanager
    def acquire(self):
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        with (self.root / 'lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise TransferError('Another receiver is active for this device/bundle/instance') from None
            self.lock = lock
            atomic_json(self.root / 'session.json', {
                'token': self.token, 'pid': os.getpid(), 'route': 'usb',
                'expires': time.time() + self.seconds, 'system_clipboard': 'unchanged',
            })
            try:
                yield self
            finally:
                (self.root / 'session.json').unlink(missing_ok=True)
                (self.root / ('cancel-' + self.token)).unlink(missing_ok=True)
                self.lock = None

    def check(self):
        if (self.root / ('cancel-' + self.token)).exists():
            raise Cancelled('USB receive cancelled')
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TransferError('USB receive timed out')
        return remaining

    @staticmethod
    def status(root):
        normal = {'active': False, 'route': 'default', 'system_clipboard': 'unchanged'}
        if not root.exists():
            return normal
        with (root / 'lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return normal  # stale state/PID cannot keep a route active
            except BlockingIOError:
                try:
                    state = read_json(root / 'session.json')
                    token = str(uuid.UUID(state['token']))
                    return {**state, 'active': True,
                            'cancelling': (root / ('cancel-' + token)).exists()}
                except (OSError, ValueError, KeyError):
                    return {'active': True, 'route': 'usb', 'starting': True}

    @staticmethod
    def cancel(root):
        state = Session.status(root)
        if not state.get('active'):
            return False
        if 'token' not in state:
            raise TransferError('Receiver is starting; retry stop shortly')
        token = str(uuid.UUID(state['token']))
        (root / ('cancel-' + token)).touch(mode=0o600, exist_ok=True)
        return True

class Device:
    def __init__(self, args, session, scratch):
        self.args, self.session, self.scratch = args, session, scratch

    def command(self, args, limit=15):
        timeout = min(limit, self.session.check())
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            raise TransferError('Device command timed out; no success inferred') from None
        self.session.check()
        if result.returncode:
            raise TransferError(result.stderr.strip() or result.stdout.strip() or 'Device command failed')
        return result.stdout

    def check_lease(self):
        state = json.loads(self.command(['iphone-use', 'status', '--instance', self.args.instance, '--json'], 10))
        if not state.get('drivable') or state.get('owner') != self.args.owner:
            raise TransferError('iPhone Use control lease ended or belongs to another caller')
        if state.get('udid') != self.args.device:
            raise TransferError('iPhone Use instance targets a different UDID')

    def guard(self):
        self.check_lease()
        path = self.scratch / 'device.json'
        self.command(['xcrun', 'devicectl', 'device', 'info', 'details', '--device', self.args.device,
                      '--timeout', '10', '--json-output', str(path)])
        obj = read_json(path)['result']
        conn = obj.get('connectionProperties') or obj.get('properties', {}).get('connection', {})
        if conn.get('transportType') != 'wired':
            raise TransferError('Wired transport required; refusing wireless fallback')
        # Transport discovery blocks: its original lease observation may be stale.
        # Keep this as the final check before callers copy or publish a file.
        self.check_lease()

    def copy(self, direction, source, destination):
        # Metadata reads/hashing since the caller's last guard can also block.
        self.check_lease()
        self.command(['xcrun', 'devicectl', 'device', 'copy', direction,
                      '--device', self.args.device, '--domain-type', 'appDataContainer',
                      '--domain-identifier', self.args.bundle, '--source', str(source),
                      '--destination', str(destination), '--timeout', '15'])


def commit_file(source, meta, destination, session, guard):
    import publication
    destination.mkdir(parents=True, exist_ok=True)
    target = destination / meta['name']
    if target.is_symlink():
        raise TransferError('Refusing symlink destination')
    if target.exists():
        if not target.is_file() or digest_file(target, meta['bytes']) != (meta['bytes'], meta['sha256']):
            raise TransferError('Destination differs; refusing overwrite')
        guard()
        path, record = publication.prepare(session, meta, target, publication.identity(target, existing=True))
        publication.mark_committed(path, record)
        return target, 'already_identical'
    fd, name = tempfile.mkstemp(prefix='.usb-', suffix='.partial', dir=destination)
    stage = Path(name)
    path = record = None
    attempted = False
    try:
        with os.fdopen(fd, 'wb') as output, source.open('rb') as incoming:
            while chunk := incoming.read(CHUNK):
                session.check()
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        if digest_file(stage, meta['bytes']) != (meta['bytes'], meta['sha256']):
            raise TransferError('Staging file integrity mismatch')
        path, record = publication.prepare(session, meta, target, publication.identity(stage))
        publication.mark_committing(path, record)
        guard()  # Recheck after persisting intent as well as after the transport probe.
        attempted = True
        os.link(stage, target)
        sync_directory(destination)
        publication.mark_committed(path, record)
        return target, 'copied'
    except Exception as error:
        if path is not None:
            if not attempted:
                record['state'] = 'aborted'
                try:
                    atomic_json(path, record)
                except Exception:
                    pass
            try:
                outcome = publication.reconcile(session.root, meta['id'])
            except Exception:
                outcome = {'committed': None}
            if outcome['committed'] is True:
                # A signal/cleanup error after link must not become a false failed copy.
                session.publication_warning = str(error)
                try:
                    publication.mark_committed(path, record, warning=str(error))
                except Exception:
                    pass  # The durable intent still permits a later read-only result query.
                return target, 'copied'
        raise
    finally:
        try:
            stage.unlink(missing_ok=True)
        except OSError as error:
            session.publication_warning = 'Staging cleanup failed: ' + str(error)


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('action', choices=['receive', 'status', 'stop', 'result'])
    p.add_argument('--device', required=True, help='Exact UDID; must match the iPhone Use instance')
    p.add_argument('--bundle', required=True, help='Exact installed XCTest runner bundle identifier')
    p.add_argument('--instance', default='default')
    p.add_argument('--owner', default=os.environ.get('IPHONE_USE_OWNER'), help='Exact caller control lease owner; required for receive')
    p.add_argument('--destination', type=Path)
    p.add_argument('--state-dir', type=Path, default=Path.home() / '.iphone-use' / 'usb-file-bridge')
    p.add_argument('--timeout', type=float, default=180)
    p.add_argument('--max-bytes', type=int, default=64 * 1024 * 1024)
    p.add_argument('--expect-sha256')
    return p
