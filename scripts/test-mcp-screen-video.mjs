// Actual panel script, emulated MCP host/decoder. No rendering or device claim.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const html=fs.readFileSync(new URL('../crates/mcp/src/screen.html',import.meta.url),'utf8');
for (const resources of [false,true]) for (const instance of [null,"i13"]) {
  const elements=new Map(),events=new Map(),timers=new Map(),messages=[];
  let timerId=0,now=0,privateScreen=false,unavailable=false,scope='boot-0',draws=0,closed=0,noOutput=false,landscape=false;
  const outputs=[];let configured=0,currentDecoder=null,packetsPerBatch=1,ppsVersion=1;
  const element=id=>{
    if(!elements.has(id))elements.set(id,{hidden:true,dataset:{},textContent:'',
      addEventListener(name,handler){this[name]=handler;},removeAttribute(name){delete this[name];},
      getContext(){return {drawImage(){draws++;}};}});
    return elements.get(id);
  };
  class Decoder {
    static async isConfigSupported(){return {supported:true};}
    constructor(callbacks){this.callbacks=callbacks;this.decodeQueueSize=0;currentDecoder=this;}
    configure(){configured++;}
    close(){closed++;}
    decode(){const w=landscape?32:16,h=landscape?16:32;const run=()=>this.callbacks.output({displayWidth:w,displayHeight:h,close(){}});outputs.push(run);if(!noOutput)queueMicrotask(run);}
  }
  const window={innerWidth:320,addEventListener(name,handler){events.set(name,handler);},parent:{postMessage(request){
    messages.push(request);if(!request.id)return;
    let result;
    if(request.method==='ui/initialize')result={hostContext:{}};
    else {
      let video,reset;
      if(resources){assert.equal(request.method,'resources/read');assert.equal(request.params.uri.includes('/instance-v1/i13/'),instance==='i13');video=request.params.uri.includes('/video-v1/');reset=request.params.uri.includes('/video-v1/1/');}
      else {assert.equal(request.method,'tools/call');assert.equal(request.params.name,'phone_screen_frame');video=request.params.arguments.video;reset=request.params.arguments.reset;}
      let body={drivable:true,private_secs:privateScreen?60:null};
      if(privateScreen)body={...body,clear_frame:true};
      else if(video && unavailable)body={...body,clear_frame:true,video_unavailable:true,error:'not available'};
      else if(video)body={...body,video:{format:'iphone-use-h264-annexb-v1',scope,reset,packets:Array.from({length:packetsPerBatch},(_,index)=>({keyframe:true,pts_micros:index+1,data:Buffer.from([0,0,0,1,0x67,0x42,0,0x1f,landscape?2:1,0,0,0,1,0x68,0xce,ppsVersion,0,0,0,1,0x65]).toString('base64')}))}};
      else body={...body,frame:{mime_type:'image/png',data:'fixture'}};
      result=resources?{contents:[{text:JSON.stringify(body)}]}:{structuredContent:body};
    }
    queueMicrotask(()=>events.get('message')({source:window.parent,data:{jsonrpc:'2.0',id:request.id,result}}));
  }}};
  const document={hidden:false,title:'iPhone screen',body:{},documentElement:{dataset:{},getBoundingClientRect(){return {height:680};}},
    querySelectorAll(){return [];},getElementById:element,addEventListener(name,handler){events.set(name,handler);}};
  const script=html.match(/<script>([\s\S]*?)<\/script>/)[1]
    .replaceAll('__IU_RESOURCE_FRAMES__',String(resources)).replaceAll('__IU_FRAME_RESOURCE__','ui://iphone-use/frame-v1/')
    .replaceAll('__IU_VIDEO_RESOURCE__','ui://iphone-use/video-v1/').replaceAll('__IU_FRAME_TOOL__','phone_screen_frame').replaceAll('__IU_VERSION__','test');
  vm.runInNewContext(script,{window,document,navigator:{language:'en'},performance:{now:()=>now},VideoDecoder:Decoder,
    EncodedVideoChunk:class {constructor(data){Object.assign(this,data);}},Uint8Array,atob:value=>Buffer.from(value,'base64').toString('binary'),
    setTimeout(handler,delay){timers.set(++timerId,{handler,delay,due:now+delay});return timerId;},clearTimeout(id){timers.delete(id);},console});
  const flush=async()=>{for(let i=0;i<8;i++)await new Promise(resolve=>setImmediate(resolve));};
  const poll=async()=>{const item=[...timers].filter(([,value])=>value.delay<8000).sort((a,b)=>a[1].due-b[1].due)[0];assert(item);timers.delete(item[0]);now=Math.max(now,item[1].due);item[1].handler();await flush();};
  await flush();
  assert.equal(messages.filter(m=>m.method==='resources/read'||m.method==='tools/call').length,0);
  events.get('message')({source:window.parent,data:{jsonrpc:'2.0',method:'ui/notifications/tool-result',params:{_meta:{'iphone-use/instance':instance}}}});
  await poll();assert.equal(draws,1);assert.equal(element('video').hidden,false);assert.equal(element('image').hidden,true);
  await poll();assert.equal(draws,2);assert.equal(configured,1,'identical SPS/PPS keyframes must reuse the decoder');
  const priorOutput=outputs[outputs.length-1];ppsVersion=2;await poll();assert.equal(configured,2,'PPS changes reconfigure even with the same SPS/codec');
  const afterPps=draws;priorOutput();assert.equal(draws,afterPps,'old configuration output cannot restore pixels');
  const beforeQueue=draws;currentDecoder.decodeQueueSize=7;packetsPerBatch=8;
  await poll();assert.equal(draws,beforeQueue,'backpressure waits before dropping any batch packets');
  currentDecoder.decodeQueueSize=0;await poll();assert.equal(draws,beforeQueue+8);assert.equal(configured,2,'a burst of IDR frames does not restart decoding');
  packetsPerBatch=1;

  privateScreen=true;await poll();assert.equal(element('video').hidden,true);assert.equal(element('video').width,1);
  const previous=draws;for(const output of outputs)output();assert.equal(draws,previous,'late decoder callbacks after private hold must not render');
  privateScreen=false;scope='boot-2';await poll();assert.equal(draws,previous+1);assert(closed>0);
  const oldOutput=outputs[outputs.length-1];
  landscape=true;await poll();assert.equal(element('video').width,32);assert.equal(element('video').height,16);
  const rendered=draws;oldOutput();assert.equal(draws,rendered,'a closed portrait decoder cannot overwrite the landscape frame');
  noOutput=true;await poll();
  const watchdog=[...timers].find(([,timer])=>timer.delay===3000);assert(watchdog);timers.delete(watchdog[0]);now=watchdog[1].due;watchdog[1].handler();await flush();
  assert.equal(element('video').hidden,true,'decoder stalls must clear stale canvas even if packets keep arriving');
  const stalledDraws=draws;outputs[outputs.length-1]();assert.equal(draws,stalledDraws,'late stalled decoder cannot restore pixels');
  await poll();assert.equal(element('image').src,'data:image/png;base64,fixture');
  noOutput=false;now+=31000;
  unavailable=true;await poll();assert.equal(element('video').hidden,true);await poll();assert.equal(element('image').src,'data:image/png;base64,fixture');
  unavailable=false;now+=31000;await poll();currentDecoder.decodeQueueSize=7;
  await poll();now+=1001;await poll();assert.equal(element('video').hidden,true,'backpressure timeout clears video');
  await poll();assert.equal(element('image').src,'data:image/png;base64,fixture');
  events.get('pagehide')();const reads=messages.length;for(const [id,timer]of [...timers])if(timer.delay<8000){timers.delete(id);timer.handler();}await flush();assert.equal(messages.length,reads);
  assert.equal(element('video').hidden,true);assert(!JSON.stringify(messages).includes('Bearer'));
  console.log(`ok - ${resources?'WorkBuddy resources':'generic/Doubao tools'}: decoder reuse, SPS/PPS changes, queue pressure/timeout, private resume, rotation, stalled decoder, late callbacks, PNG fallback, hidden teardown`);
}
