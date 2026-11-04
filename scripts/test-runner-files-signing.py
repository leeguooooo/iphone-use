#!/usr/bin/env python3
"""Owned GitHub-hosted signing fixture. Never modifies a personal keychain.

Runner guard: https://docs.github.com/en/actions/reference/workflows-and-actions/variables
Uses a disposable identity; this is not Apple development signing/device acceptance.
"""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import secrets
import signal
import shlex
import subprocess
import sys
import tempfile
import uuid


def require_owned_ci(env):
    if sys.platform != 'darwin' or env.get('GITHUB_ACTIONS') != 'true' or env.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('requires an owned GitHub-hosted macOS runner; no commands executed')
    root = Path(env['RUNNER_TEMP']).resolve(strict=True)
    if not root.is_dir():
        raise RuntimeError('missing owned runner temporary directory')
    return root


def main():
    runner_temp = require_owned_ci(os.environ)
    if not __debug__:
        raise RuntimeError('assertions required')
    secret_values = []

    def command(argv, timeout=60, env=None):
        process = subprocess.Popen(list(map(str, argv)), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, env=env, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            try: os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError: pass
            except OSError:
                raise RuntimeError(Path(argv[0]).name + ' command group termination unverified; no replay') from None
            try:
                process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                process.stdout.close()
                process.stderr.close()
                raise RuntimeError(Path(argv[0]).name + ' termination/stream closure unverified; no replay') from None
            raise RuntimeError(Path(argv[0]).name + ' timed out; owned command group stopped; no replay') from None
        result = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
        if result.returncode:
            detail = result.stderr[-1800:]
            for value in secret_values:
                detail = detail.replace(value, '<fixture-secret>')
            raise RuntimeError(Path(argv[0]).name + ' failed: ' + detail)
        return result.stdout

    original_search = shlex.split(command(['/usr/bin/security', 'list-keychains', '-d', 'user']))
    original_default = shlex.split(command(['/usr/bin/security', 'default-keychain', '-d', 'user']))
    assert len(original_default) == 1
    receipt = {'passed': False, 'personal_keychain': False, 'physical_actions': False, 'trust_settings_modified': False,
               'scope': 'self-signed macOS bundle transaction through production apply; not Apple signing/install/reconnect/upgrade acceptance'}
    with tempfile.TemporaryDirectory(prefix='ipu-signing-fixture-', dir=runner_temp) as directory:
        root = Path(directory)
        keychain = root / 'fixture.keychain-db'
        cert = root / 'certificate.pem'
        created = False
        cleanup_errors = []
        try:
            password = secrets.token_hex(24)
            secret_values.append(password)
            config = root / 'openssl.cnf'
            config.write_text('[req]\nprompt=no\ndistinguished_name=subject\nx509_extensions=signing\n[subject]\nCN=iphone-use fixture ' + str(uuid.uuid4()) + '\n[signing]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,digitalSignature,keyCertSign\nextendedKeyUsage=critical,codeSigning\n')
            command(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1', '-config', config, '-keyout', root/'private.pem', '-out', cert])
            (root/'private.pem').chmod(0o600)
            command(['openssl', 'x509', '-in', cert, '-outform', 'DER', '-out', root/'certificate.der'])
            identity = hashlib.sha1((root/'certificate.der').read_bytes()).hexdigest().upper()
            command(['openssl', 'pkcs12', '-export', '-inkey', root/'private.pem', '-in', cert,
                     '-out', root/'identity.p12', '-passout', 'pass:'+password,
                     '-certpbe', 'PBE-SHA1-3DES', '-keypbe', 'PBE-SHA1-3DES', '-macalg', 'sha1'])
            created = True
            command(['/usr/bin/security', 'create-keychain', '-p', password, keychain])
            command(['/usr/bin/security', 'unlock-keychain', '-p', password, keychain])
            command(['/usr/bin/security', 'import', root/'identity.p12', '-k', keychain, '-P', password, '-T', '/usr/bin/codesign'])
            command(['/usr/bin/security', 'set-key-partition-list', '-S', 'apple-tool:,apple:,codesign:', '-s', '-k', password, keychain])
            command(['/usr/bin/security', 'list-keychains', '-d', 'user', '-s', keychain, *original_search])
            # Self-signed code identifies this fixture; no global trust policy is changed.
            assert identity in command(['/usr/bin/security', 'find-identity', '-p', 'codesigning', keychain]).upper()
            app = root/'template/iPhoneUse-Runner.app'
            tests = app/'PlugIns/Fixture.xctest'
            tests.mkdir(parents=True)
            for bundle, identifier, executable, kind in [(app, 'owned.signing.fixture', 'FixtureRunner', 'APPL'), (tests, 'owned.signing.fixture.tests', 'FixtureTests', 'BNDL')]:
                (bundle/'Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier':identifier, 'CFBundleExecutable':executable, 'CFBundlePackageType':kind, 'CFBundleIconName':'FixtureIcon'}))
            env = os.environ.copy()
            env['SDKROOT'] = command(['xcrun', '--sdk', 'macosx', '--show-sdk-path']).strip()
            source = root/'fixture.c';source.write_text('int main(void){return 0;}\n')
            command(['xcrun', 'clang', source, '-o', app/'FixtureRunner'], env=env)
            source.write_text('int fixture(void){return 0;}\n')
            command(['xcrun', 'clang', '-bundle', source, '-o', tests/'FixtureTests'], env=env)
            entitlements = root/'entitlements.plist'
            entitlements.write_bytes(plistlib.dumps({'com.apple.security.get-task-allow':True, 'fixture-array':['first','second']}))
            command(['/usr/bin/codesign', '-f', '-s', identity, tests])
            command(['/usr/bin/codesign', '-f', '-s', identity, '--entitlements', entitlements, app])
            command(['/usr/bin/codesign', '--verify', '--deep', '--strict', app])
            cargo_env = os.environ.copy()
            cargo_env.update(IPU_SIGNING_FIXTURE_ROOT=str(root), IPU_SIGNING_FIXTURE_IDENTITY=identity)
            result = command(['cargo', 'test', '-p', 'server', '--lib',
                              'setup::runner_files::tests::signed_product_transaction_preserves_bundle_and_fails_without_private_key',
                              '--', '--exact', '--ignored', '--test-threads=1'], timeout=300, env=cargo_env)
            (runner_temp/'runner-files-signing-test.log').write_text(result)
            assert '1 passed; 0 failed' in result and '0 passed;' not in result
            receipt.update(passed=True, identity_sha1=identity, applied_twice=True, missing_private_key_preserved_original=True)
        finally:
            operations = [(['/usr/bin/security','list-keychains','-d','user','-s',*original_search], True),
                          (['/usr/bin/security','default-keychain','-d','user','-s',original_default[0]], True),
                          (['/usr/bin/security','delete-keychain',keychain], created)]
            for argv, needed in operations:
                if needed:
                    try:command(argv)
                    except RuntimeError as error:cleanup_errors.append(str(error))
            try:
                assert shlex.split(command(['/usr/bin/security','list-keychains','-d','user'])) == original_search
                assert shlex.split(command(['/usr/bin/security','default-keychain','-d','user'])) == original_default
            except (AssertionError, RuntimeError):cleanup_errors.append('keychain preferences did not restore')
            receipt['cleanup_errors'] = cleanup_errors
            receipt['keychain_preferences_restored'] = not cleanup_errors
            if cleanup_errors:receipt['passed'] = False
            (runner_temp/'runner-files-signing.json').write_text(json.dumps(receipt, indent=2))
            if cleanup_errors:raise RuntimeError('owned signing fixture cleanup failed')
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
