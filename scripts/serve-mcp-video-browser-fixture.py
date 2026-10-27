#!/usr/bin/env python3
"""Serve the actual panel with a synthetic MCP host and a captured single H.264 AU.

No daemon/phone credentials or personal state. Use chrome-use for actual browser
checks; this proves browser codec/canvas behavior, not a real MCP host product.
"""
import argparse
import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--packet", required=True, type=Path, help="single Annex-B keyframe with SPS/PPS")
parser.add_argument("--interval-ms", type=int, default=100, help="synthetic host response delay, not a latency measurement")
parser.add_argument("--batch-packets", type=int, default=1, choices=range(1, 9))
parser.add_argument("--png", type=Path, help="synthetic fallback PNG from the same owned fixture")
args = parser.parse_args()
if not 0 <= args.interval_ms <= 1000:parser.error("interval-ms must be 0...1000")
packet = base64.b64encode(args.packet.read_bytes()).decode()
png = base64.b64encode(args.png.read_bytes()).decode() if args.png else ""
page = (Path(__file__).resolve().parent.parent / "crates/mcp/src/screen.html").read_text()
host = '''<!doctype html><title>Native panel video fixture</title>
<button id="hide">Hide screen</button><button id="show">Show screen</button>
<button id="png">Use PNG</button><button id="video">Use video</button>
<output id="state">public</output><iframe id="panel" width="360" height="800" sandbox="allow-scripts allow-same-origin"></iframe>
<script>
const frame=document.getElementById('panel');let hidden=false,scope=0,serial=0,unavailable=false;
frame.src='/screen'+location.search;
document.getElementById('hide').onclick=()=>{hidden=true;scope++;document.getElementById('state').textContent='private';};
document.getElementById('show').onclick=()=>{hidden=false;scope++;document.getElementById('state').textContent='public';};
document.getElementById('png').onclick=()=>{unavailable=true;};
document.getElementById('video').onclick=()=>{unavailable=false;};
window.addEventListener('message',event=>{
 if(event.source!==frame.contentWindow || !event.data || event.data.id===undefined)return;
 const request=event.data;let result;
 if(request.method==='ui/initialize')result={hostContext:{}};
 else if(request.method==='tools/call'||request.method==='resources/read') {
  const video=request.method==='tools/call'?request.params.arguments.video:request.params.uri.includes('/video-v1/');
  const reset=request.method==='tools/call'?request.params.arguments.reset:request.params.uri.includes('/video-v1/1/');
  const body=hidden?{private_secs:60,clear_frame:true,frame:null}:{drivable:true,video:{format:'iphone-use-h264-annexb-v1',scope:'fixture-'+scope,reset,packets:Array.from({length:__BATCH__},()=>({keyframe:true,pts_micros:++serial*33333,data:'__PACKET__'}))}};
  if(!hidden && video && unavailable){delete body.video;body.video_unavailable=true;body.clear_frame=true;}
  if(!hidden && !video){delete body.video;if('__PNG__')body.frame={mime_type:'image/png',data:'__PNG__'};else{body.error='fixture supplies native video only';body.frame=null;}}
  result=request.method==='tools/call'?{structuredContent:body}:{contents:[{text:JSON.stringify(body)}]};
 } else return;
 setTimeout(()=>frame.contentWindow.postMessage({jsonrpc:'2.0',id:request.id,result},'*'),__INTERVAL__);
});
</script>'''.replace('__PACKET__', packet).replace('__PNG__',png).replace('__BATCH__',str(args.batch_packets)).replace('__INTERVAL__',str(args.interval_ms))

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith('/screen'):
            resources = 'resources=1' in self.path
            text = page.replace('__IU_VERSION__', 'fixture').replace('__IU_FRAME_TOOL__', 'phone_screen_frame')
            text = text.replace('__IU_RESOURCE_FRAMES__', str(resources).lower())
            text = text.replace('__IU_FRAME_RESOURCE__', 'ui://iphone-use/frame-v1/').replace('__IU_VIDEO_RESOURCE__', 'ui://iphone-use/video-v1/')
        elif self.path == '/' or self.path.startswith('/?'):
            text = host
        else:
            self.send_error(404)
            return
        data = text.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(data)))
        if self.path.startswith('/screen'):
            self.send_header('Content-Security-Policy', "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'none'")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_):
        pass

server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(f'http://127.0.0.1:{server.server_port}/', flush=True)
try:
    server.serve_forever()
except KeyboardInterrupt:
    pass
finally:
    server.server_close()
