// Run the real panel script with a synthetic MCP host. No browser or phone is contacted.
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../crates/mcp/src/screen.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1]
  .replaceAll('__IU_RESOURCE_FRAMES__', 'false')
  .replaceAll('__IU_FRAME_RESOURCE__', 'ui://iphone-use/frame-v1/')
  .replaceAll('__IU_VIDEO_RESOURCE__', 'ui://iphone-use/video-v1/')
  .replaceAll('__IU_FRAME_TOOL__', 'phone_screen_frame').replaceAll('__IU_VERSION__', 'test');
function panel(body={released:true,drivable:false,phone:'mock'}) {
  const elements = new Map();
  const element = (id) => {
    if (!elements.has(id)) elements.set(id, {dataset:{}, hidden:true, disabled:false,
      removeAttribute(name){delete this[name];},
      events:{}, addEventListener(name, callback) { this.events[name] = callback; }});
    return elements.get(id);
  };
  const listeners = {}, calls = [], timers = new Map();
  let nextTimer = 1;
  const parent = {postMessage(message) {
    if (message.method === 'tools/call') calls.push(message.params);
    if (message.id !== undefined && message.method) queueMicrotask(() => notify({
      jsonrpc:'2.0', id:message.id, ...(message.method === 'ui/initialize'
        ? {result:{hostContext:{}}} : body.transportError
          ? {error:{code:-32000,message:'lost host reply'}} : {result:{structuredContent:body}})
    }));
  }};
  const window = {parent, innerWidth:320, addEventListener(name, callback) {listeners[name] = callback;}};
  const notify = (data, source=parent) => listeners.message({data,source});
  const context = {window, navigator:{language:'en'}, performance:{now:()=>0},
    document:{hidden:false, documentElement:{dataset:{},getBoundingClientRect:()=>({height:600})},
      getElementById:element,addEventListener(){}},
    setTimeout(callback, delay) {const id=nextTimer++;timers.set(id,{callback,delay});return id;},
    clearTimeout(id) {timers.delete(id);}};
  vm.runInNewContext(script, context, {timeout:1000});
  async function flush() {
    for (let i=0;i<8;i++) {
      await new Promise(setImmediate);
      for (const [id,timer] of [...timers]) if (timer.delay===0) {
        timers.delete(id);timer.callback();
      }
    }
  }
  const input = (args) => notify({jsonrpc:'2.0',method:'ui/notifications/tool-input',params:{arguments:args}});
  return {calls,element,notify,input,flush};
}
(async () => {
  const p=panel();await p.flush();
  assert.equal(p.calls.length,0,'unbound panel must not poll a guessed phone');
  assert.equal(p.element('home').disabled,true);
  p.input({instance:null});await p.flush();assert.equal(p.calls.length,0,'invalid explicit null must not fall back');
  p.input({instance:'i13'});await p.flush();
  assert.equal(p.calls[0].name,'phone_screen_frame');
  assert.equal(p.calls[0].arguments.instance,'i13');
  await p.element('home').events.click();await p.flush();
  assert.equal(p.calls.filter(call=>call.name==='phone_shortcut').at(-1).arguments.instance,'i13');
  p.notify({jsonrpc:'2.0',method:'ui/notifications/tool-result',params:{_meta:{'iphone-use/instance':'default'}}});
  p.input({instance:'default'});
  await p.element('home').events.click();await p.flush();
  assert.equal(p.calls.at(-1).arguments.instance,'i13','another target must not redirect a bound panel');
  const before=p.calls.length;
  p.notify({jsonrpc:'2.0',id:100,method:'ui/resource-teardown'});
  await p.element('home').events.click();await p.flush();
  assert.equal(p.calls.length,before,'ended panel must not send input');
  const configured=panel();configured.input({});await configured.flush();
  assert.equal(Object.hasOwn(configured.calls[0].arguments,'instance'),false,'implicit target preserves startup config');
  const explicit=panel();explicit.input({instance:'default'});await explicit.flush();
  assert.equal(explicit.calls[0].arguments.instance,'default');
  const frame={drivable:true,frame:{mime_type:'image/png',data:'fixture'}};
  const privacy=panel(frame);privacy.input({});await privacy.flush();
  assert.equal(privacy.element('image').hidden,false);
  delete frame.frame;frame.clear_frame=true;frame.error="capture refused";
  await privacy.element('refresh').events.click();await privacy.flush();
  assert.equal(privacy.element('image').hidden,true,'capture refusal clears previously displayed pixels');
  assert.equal(privacy.element('image').src,undefined);
  assert(privacy.element('status-text').textContent.includes('capture refused'));
  delete frame.error;frame.drivable=false;frame.next_step='Reconnect explicitly';
  await privacy.element('refresh').events.click();await privacy.flush();
  assert(privacy.element('status-text').textContent.includes('Reconnect explicitly'));
  delete frame.next_step;frame.drivable=true;

  frame.clear_frame=false;frame.frame={mime_type:'image/png',data:'new-fixture'};
  await privacy.element('refresh').events.click();await privacy.flush();
  assert.equal(privacy.element('image').hidden,false);
  frame.transportError=true;
  await privacy.element('refresh').events.click();await privacy.flush();
  assert.equal(privacy.element('image').hidden,true,'host RPC failure must clear old pixels');
  assert.equal(privacy.element('image').src,undefined);

  console.log('MCP panel: waits for binding; frame and Home stay on one instance; invalid input and teardown fail closed');
})().catch(error=>{console.error(error);process.exitCode=1;});
