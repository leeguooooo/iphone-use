#!/usr/bin/env python3
"""Stage a file-sharing-enabled copy of an already signed XCTest runner app.

Never overwrites the source, changes its bundle identity, or installs anything.
Rebuilds/upgrades can replace these flags; this is an opt-in experiment.
"""
import argparse
import plistlib
from pathlib import Path
import subprocess
import tempfile

def file_sharing_info(info):
    bundle = info.get('CFBundleIdentifier', '')
    if not isinstance(bundle, str) or not bundle.endswith('.xctrunner'):
        raise ValueError('Source must be an existing .xctrunner app')
    return {**info, 'UIFileSharingEnabled': True, 'LSSupportsOpeningDocumentsInPlace': True}

def run(args):
    return subprocess.run(args, capture_output=True, check=True)

def stage(source, output):
    source, output = source.resolve(), output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError('Output must not exist')
    output = output.parent.resolve() / output.name
    if not source.is_dir() or not list((source/'PlugIns').glob('*.xctest')):
        raise ValueError('Source must contain its XCTest bundle')
    if output.exists() or output.is_symlink() or source in output.parents:
        raise ValueError('Output must be a new path outside the source app')
    info = file_sharing_info(plistlib.loads((source/'Info.plist').read_bytes()))
    run(['codesign','--verify','--deep','--strict',str(source)])
    details = run(['codesign','-dvv',str(source)]).stderr.decode()
    identity = next((line.split('=',1)[1] for line in details.splitlines() if line.startswith('Authority=')),None)
    if not identity:
        raise ValueError('Source has no development signing identity')
    output.parent.mkdir(parents=True,exist_ok=True)
    # Work in a temporary staging directory; only a verified complete app is published.
    with tempfile.TemporaryDirectory(prefix='runner-files-',dir=output.parent) as tmp:
        folder=Path(tmp);app=folder/'iPhoneUse-Runner.app'
        run(['ditto',str(source),str(app)])
        entitlements=run(['codesign','-d','--entitlements','-','--xml',str(source)]).stdout
        plistlib.loads(entitlements)
        ent=folder/'entitlements.plist';ent.write_bytes(entitlements);ent.chmod(0o600)
        (app/'Info.plist').write_bytes(plistlib.dumps(info,fmt=plistlib.FMT_BINARY))
        # Nested executables are byte-for-byte unchanged; only the outer seal changes.
        run(['codesign','-f','-s',identity,'--entitlements',str(ent),str(app)])
        run(['codesign','--verify','--deep','--strict',str(app)])
        if output.exists():raise FileExistsError(output)
        app.rename(output)
    return output

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source',required=True,type=Path)
    p.add_argument('--output',required=True,type=Path)
    a=p.parse_args()
    print(stage(a.source,a.output))
