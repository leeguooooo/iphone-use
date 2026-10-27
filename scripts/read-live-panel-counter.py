#!/usr/bin/env python3
"""Bounded read-only Chrome canvas/OCR observation for an owned live fixture.

Never writes the counter or sends phone input. Failed/stale observations are
retained; only an actual rendered PNG containing the expected marker passes.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time
import urllib.parse

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--fixture-out', required=True, type=Path)
parser.add_argument('--session', default='iphone-panel-video')
parser.add_argument('--ocr', required=True, type=Path)
parser.add_argument('--counter', required=True, type=int)
parser.add_argument('--name', required=True)
parser.add_argument('--seconds', type=float, default=10)
args = parser.parse_args()
if not re.fullmatch(r'[a-zA-Z0-9_-]+', args.name) or not 0 < args.seconds <= 30:
    parser.error('bounded seconds and a simple output name are required')
root = args.fixture_out.resolve(strict=True)
url = json.loads((root/'ready.json').read_text())['url']
origin = urllib.parse.urlsplit(url)
expected = f'{origin.scheme}://{origin.netloc}'
script = '''(() => {
 if(location.origin!==__ORIGIN__||document.title!=='Owned Simulator live MCP panel')throw new Error('not the owned fixture');
 const c=document.querySelector('#panel').contentDocument.getElementById('video');
 return JSON.stringify({hidden:c.hidden,width:c.width,height:c.height,png:!c.hidden&&c.width>1?c.toDataURL('image/png'):null});
})()'''.replace('__ORIGIN__', json.dumps(expected))
started = time.monotonic()
deadline = started + args.seconds
def remaining():
    budget = deadline - time.monotonic()
    if budget <= 0:
        raise TimeoutError('owned canvas observation deadline expired')
    return budget
records = []
result = {'scope':'actual browser pixels with bounded read-only OCR; not a counter write or latency benchmark','expected_counter':args.counter,'ok':False}
try:
    while time.monotonic() < deadline:
        read = subprocess.run(['chrome-use','--session',args.session,'eval',script,'--json'],check=True,capture_output=True,text=True,timeout=remaining())
        wire = json.loads(read.stdout)
        if not wire.get('success'):
            raise RuntimeError('owned Chrome read failed')
        body = json.loads(wire['data']['result'])
        record = {key:body[key] for key in ['hidden','width','height']}
        record['observed_ms'] = round((time.monotonic()-started)*1000)
        if body.get('png'):
            image = base64.b64decode(body['png'].split(',',1)[1],validate=True)
            assert image.startswith(b'\x89PNG\r\n\x1a\n')
            path = root/f'{args.name}-attempt-{len(records)+1}.png'
            path.write_bytes(image)
            lines = json.loads(subprocess.run([str(args.ocr.resolve()),str(path)],check=True,capture_output=True,text=True,timeout=remaining()).stdout)
            remaining()
            record['ocr'] = lines
            record['png_sha256'] = hashlib.sha256(image).hexdigest()
            if any(re.search(r'\bLive frame '+str(args.counter)+r'\b',line['text']) and line['confidence']>=0.9 for line in lines):
                (root/f'{args.name}.png').write_bytes(image)
                result.update(ok=True,observed_counter=args.counter,png_sha256=record['png_sha256'])
        records.append(record)
        if result['ok']:
            break
        time.sleep(min(0.1, remaining()))
except Exception as error:
    result.update(error_type=type(error).__name__,error=str(error))
result.update(attempts=records,elapsed_ms=round((time.monotonic()-started)*1000))
(root/f'{args.name}-observation.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({'ok':result['ok'],'counter':args.counter,'attempts':len(records),'elapsed_ms':result['elapsed_ms']}))
raise SystemExit(0 if result['ok'] and 'error_type' not in result else 1)
