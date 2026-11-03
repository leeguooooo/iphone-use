// Lightweight checks against the panel's actual functions; no browser or build.
// Run: node --test scripts/tests/activity-target.test.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../../web/index.html'), 'utf8');

function panel() {
  const nodes = [], timers = [];
  const frame = {
    classList: { contains: () => true }, width: 200, height: 400,
    getBoundingClientRect: () => ({ left: 100, top: 50, width: 400, height: 400 }),
  };
  const activity = { replaceChildren() {}, classList: { add() {}, remove() {} } };
  const ctx = vm.createContext({
    Date: { now: () => 10000 }, Number, Math,
    h264Frame: frame, mjpegFrame: frame, stillFrame: frame,
    overlayState: 'hidden', LANG: 'en', BASE: '',
    document: {
      visibilityState: 'visible', getElementById: () => activity,
      createTextNode: text => text,
      createElement: () => ({ style: {}, setAttribute() {}, addEventListener() {}, remove() { this.removed = true; } }),
    },
    touchLayer: {
      getBoundingClientRect: () => ({ left: 80, top: 30 }),
      append: node => nodes.push(node),
    },
    setTimeout: fn => { timers.push(fn); return timers.length; }, clearTimeout() {},
  });
  vm.runInContext(html.slice(html.indexOf('function visibleFrameContentRect()'), html.indexOf('/* ── Direct HTTP control')), ctx);
  vm.runInContext(html.slice(html.indexOf('const agentActivityEl ='), html.indexOf('/* ── Boot')), ctx);
  return { ctx, nodes, timers, frame };
}
const normalized = { coordinate_space: 'normalized', point: [0.25, 0.75] };
const item = target => ({ seq: 1, at_ms: 10000, action_type: 'tap', target });

test('requires explicit units and complete finite geometry', () => {
  const { ctx } = panel();
  for (const target of [null, {}, { point: [0.5, 0.5] },
    { ...normalized, point: [10, 20] }, { ...normalized, point: ['0.5', 0.5] },
    { ...normalized, point: [NaN, 0.5] }, { ...normalized, point: [0.5] },
    { ...normalized, rect: [0, 0, 0, 1] }, { ...normalized, rect: [0.9, 0, 0.2, 1] },
    { ...normalized, coordinate_space: 'pixels' },
  ]) assert.equal(ctx.activityNormalizedTarget(target), null);
  assert.deepEqual(Array.from(ctx.activityNormalizedTarget(normalized).point), [0.25, 0.75]);
});

test('runner points require their own viewport and never use frame pixels', () => {
  const { ctx } = panel();
  const target = { coordinate_space: 'runner_points', point: [25, 150], rect: [10, 100, 30, 80] };
  for (const viewport of [undefined, [0, 200], [-100, 200], [100, Infinity], ['100', 200]]) {
    assert.equal(ctx.activityNormalizedTarget({ ...target, viewport }), null);
  }
  const converted = ctx.activityNormalizedTarget({ ...target, viewport: [100, 200] });
  assert.deepEqual(Array.from(converted.point), [0.25, 0.75]);
  assert.deepEqual(Array.from(converted.rect), [0.1, 0.5, 0.3, 0.4]);
});

test('letterboxed frame maps both pointer input and agent output to content', () => {
  const { ctx, nodes } = panel();
  // 200x400 source in a 400x400 element: actual content begins at x=200.
  assert.equal(ctx.pointerToNormalized(150, 100), null);
  const pointer = ctx.pointerToNormalized(250, 350);
  assert.equal(pointer.nx, 0.25);
  assert.equal(pointer.ny, 0.75);
  ctx.showAgentActivityTarget(item(normalized));
  assert.equal(nodes.length, 1);
  assert.equal(nodes[0].className, 'ripple agent');
  assert.equal(nodes[0].style.left, '170px'); // client 250 - layer origin 80
  assert.equal(nodes[0].style.top, '320px');
});

test('element target outlines use runner viewport, and feedback expires', () => {
  const { ctx, nodes, timers } = panel();
  ctx.showAgentActivityTarget(item({ coordinate_space: 'runner_points', viewport: [100, 200], point: [25, 150], rect: [10, 100, 30, 80] }));
  assert.equal(nodes.length, 2);
  assert.equal(nodes[1].className, 'agent-target-rect');
  assert.equal(nodes[1].style.left, '140px');
  assert.equal(nodes[1].style.top, '220px');
  assert.equal(nodes[1].style.width, '60px');
  assert.equal(nodes[1].style.height, '160px');
  timers.forEach(fn => fn());
  assert.ok(nodes.every(node => node.removed));
});

test('skips stale, non-click, offline, rotated and unmappable feedback', () => {
  const { ctx, nodes, frame } = panel();
  ctx.showAgentActivityTarget({ ...item(normalized), at_ms: 1 });
  ctx.showAgentActivityTarget({ ...item(normalized), at_ms: 20000 });
  ctx.showAgentActivityTarget({ ...item(normalized), action_type: 'set_value' });
  ctx.showAgentActivityTarget(item({ coordinate_space: 'runner_points', point: [25, 50] }));
  ctx.showAgentActivityTarget(item({ coordinate_space: 'runner_points', point: [25, 50], viewport: [200, 100] }));
  ctx.overlayState = 'offline';
  ctx.showAgentActivityTarget(item(normalized));
  ctx.overlayState = 'hidden';
  frame.width = 0;
  ctx.showAgentActivityTarget(item(normalized));
  assert.equal(nodes.length, 0);
});

test('poll boot suppresses history, then draws only unseen actions', async () => {
  const { ctx, nodes } = panel();
  let feed = { next: 1, items: [item(normalized)] };
  ctx.fetch = async () => ({ ok: true, json: async () => feed });
  await ctx.pollAgentActivity();
  assert.equal(nodes.length, 0);
  feed = { next: 2, items: [item(normalized), { ...item(normalized), seq: 2 }], live_target: { ...item(normalized), seq: 2 } };
  await ctx.pollAgentActivity();
  assert.equal(nodes.length, 1);
  await ctx.pollAgentActivity();
  assert.equal(nodes.length, 1);
});


test('hidden tab draws no fresh agent feedback', async () => {
  const { ctx, nodes } = panel();
  let feed = { next: 1, items: [] };
  ctx.fetch = async () => ({ ok: true, json: async () => feed });
  await ctx.pollAgentActivity();
  const fresh = { ...item({coordinate_space:'runner_points',point:[25,50],viewport:[100,200]}), seq: 2 };
  feed = { next: 2, items: [fresh], live_target: fresh };
  ctx.document.visibilityState = 'hidden';
  await ctx.pollAgentActivity();
  assert.equal(nodes.length, 0);
});


test('historical item geometry is never replayed without live_target', async () => {
  const {ctx,nodes}=panel();
  let feed={next:1,items:[]};ctx.fetch=async()=>({ok:true,json:async()=>feed});
  await ctx.pollAgentActivity();
  feed={next:2,items:[{...item(normalized),seq:2}]};
  await ctx.pollAgentActivity();
  assert.equal(nodes.length,0);
});
