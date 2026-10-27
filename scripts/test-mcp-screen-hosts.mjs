// Exercise the actual panel script against a host transport that refuses all
// app tools/call in WorkBuddy mode. No browser profile, phone or network.
import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";

const html = fs.readFileSync(new URL("../crates/mcp/src/screen.html", import.meta.url), "utf8");
for (const resources of [false, true]) for (const instance of [null, "i13"]) {
  const elements = new Map();
  function element(id) {
    if (!elements.has(id)) elements.set(id, {
      hidden: id === "image", dataset: {}, textContent: "", title: "", disabled: false,
      addEventListener(name, handler) { this[name] = handler; },
      removeAttribute(name) { delete this[name]; },
    });
    return elements.get(id);
  }
  const events = new Map(), documentEvents = new Map(), messages = [], timers = new Map();
  let timerId = 0, privateFrame = false, clearFrame = false;
  const body = () => ({drivable:true, clear_frame:clearFrame, private_secs:privateFrame ? 60 : null,
    frame:privateFrame ? null : {mime_type:"image/png",data:"fixture"}});
  const window = {
    addEventListener(name, handler) { events.set(name,handler); }, innerWidth:320,
    parent: { postMessage(request) {
      messages.push(request);
      if (!request.id) return;
      let result;
      if (request.method === "ui/initialize") result = {hostContext:{theme:"dark"}};
      else if (request.method === "resources/read") {
        assert(resources);
        assert.equal(request.params.uri.includes("/instance-v1/i13/"), instance === "i13");
        assert.match(request.params.uri.replace("instance-v1/i13/", ""), /^ui:\/\/iphone-use\/frame-v1\/600\/\d+$/);
        result = {contents:[{uri:request.params.uri,mimeType:"application/json",text:JSON.stringify(body())}]};
      } else if (request.method === "tools/call") {
        assert(!resources, "WorkBuddy cannot forward app tools/call");
        result = {structuredContent:body(),content:[]};
      } else assert.fail("unexpected host method " + request.method);
      queueMicrotask(() => events.get("message")({source:window.parent,data:{jsonrpc:"2.0",id:request.id,result}}));
    }},
  };
  const document = {hidden:false, title:"iPhone screen", body:{},
    documentElement:{dataset:{},getBoundingClientRect(){return {height:680};}},
    querySelectorAll(){return [];}, getElementById:element,
    addEventListener(name,handler){documentEvents.set(name,handler);},
  };
  const script = html.match(/<script>([\s\S]*?)<\/script>/)[1]
    .replaceAll("__IU_RESOURCE_FRAMES__",String(resources))
    .replaceAll("__IU_FRAME_RESOURCE__","ui://iphone-use/frame-v1/")
    .replaceAll("__IU_FRAME_TOOL__","phone_screen_frame")
    .replaceAll("__IU_VERSION__","test");
  vm.runInNewContext(script, {window,document,navigator:{language:"en"},performance:{now:()=>0},
    setTimeout(handler,delay){timers.set(++timerId,{handler,delay});return timerId;},
    clearTimeout(id){timers.delete(id);}, console});
  const flush = async () => {for(let i=0;i<5;i++) await new Promise(resolve=>setImmediate(resolve));};
  const poll = async () => {
    const found=[...timers].filter(([,t])=>t.delay<8000).sort((a,b)=>a[1].delay-b[1].delay)[0];assert(found,"panel must schedule next poll");
    timers.delete(found[0]);found[1].handler();await flush();
  };
  await flush();
  assert.equal(messages.filter(m=>m.method==="resources/read" || m.method==="tools/call").length,0,"unbound panel must not read startup phone");
  events.get("message")({source:window.parent,data:{jsonrpc:"2.0",method:"ui/notifications/tool-result",params:{_meta:{"iphone-use/instance":instance}}}});
  await poll();
  assert.equal(element("image").hidden,false);
  assert.equal(element("image").src,"data:image/png;base64,fixture");
  assert.equal(element("toolbar").hidden === true,resources);
  assert.equal(messages.filter(m=>m.method==="resources/read").length,resources ? 1 : 0);
  assert.equal(messages.filter(m=>m.method==="tools/call").length,resources ? 0 : 1);
  await element("home").click();await flush();
  assert.equal(messages.filter(m=>m.params?.name==="phone_shortcut").length,resources ? 0 : 1);
  clearFrame=true;await poll();
  assert.equal(element("image").hidden,true,"a capture privacy refusal must clear even when status still says open");
  assert.equal(element("image").src,undefined);
  clearFrame=false;privateFrame=true;await poll();
  assert.equal(element("image").hidden,true,"a private hold must clear the previous frame");
  assert.equal(element("image").src,undefined);
  if(resources) {
    const uris=messages.filter(m=>m.method==="resources/read").map(m=>m.params.uri);
    assert.equal(new Set(uris).size,uris.length,"frame resource nonce must change");
  }
  events.get("pagehide")();
  const reads=messages.length;
  for(const [id,timer] of [...timers]) if(timer.delay<8000){timers.delete(id);timer.handler();}
  await flush();assert.equal(messages.length,reads,"hidden panel must stop polling");
  console.log(`ok - ${resources ? "WorkBuddy resources" : "generic/Doubao tools"}: draw, private hold, controls, cache nonce, hidden polling`);
}
