const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const source = fs.readFileSync(process.env.BOOKFORGE_DASHBOARD_JS || path.join(__dirname, '../../src/commands/serve/dashboard.js'), 'utf8');

async function dashboard() {
  const nodes = new Map();
  const node = () => ({ innerHTML: '', textContent: '', style: {}, classList: { toggle() {}, add() {}, remove() {} }, setAttribute() {}, querySelector() { return null; } });
  for (const id of ['#stage', '#grid', '#nav', '#toast', '#sun', '#moon', '#audio-progress']) nodes.set(id, node());
  const calls = [], redirects = [];
  let respond = () => ({ ok: true, json: async () => [] });
  const context = vm.createContext({
    console, URL, URLSearchParams, FormData, encodeURIComponent, AbortController, TextDecoder,
    document: { querySelector: id => nodes.get(id) || null, querySelectorAll: () => [], documentElement: node(), createElement: node, body: { appendChild(n) { nodes.set('#' + n.id, n); } } },
    localStorage: { getItem() { return null; }, setItem() {} },
    location: { replace: url => redirects.push(url) },
    setTimeout() {}, clearTimeout() {}, setInterval() {}, clearInterval() {},
    fetch: async (...args) => { calls.push(args); return respond(...args); },
    EventSource: class { addEventListener() {} close() {} },
  });
  await vm.runInContext(source, context);
  // The boot renderer starts loading the library without awaiting it.
  await context.loadLibraryJobs();
  calls.length = 0;
  return { context, nodes, calls, redirects, run: code => vm.runInContext(code, context), respond: fn => { respond = fn; } };
}
const hostile = `<img src=x onerror="alert(1)">&'`;
const escaped = '&lt;img src=x onerror=&quot;alert(1)&quot;&gt;&amp;&#39;';

test('escaping handles markup, quotes, null and scalar values', async () => {
  const d = await dashboard();
  assert.equal(d.context.esc(hostile), escaped);
  assert.equal(d.context.esc(null), '');
  assert.equal(d.context.esc(42), '42');
});

test('library renders escaped translation and audiobook fields from real GET responses', async () => {
  const d = await dashboard();
  d.respond(url => ({ ok: true, json: async () => url === '/api/jobs' ? [{ id: hostile, provider: hostile, model: hostile, status: 'done', target_lang: hostile, done: 1, total_segments: 1 }] : [{ id: hostile, title: hostile, status: 'succeeded' }] }));
  await d.context.loadLibraryJobs();
  assert.deepEqual(d.calls.map(c => c[0]), ['/api/jobs', '/api/audiobooks']);
  const html = d.nodes.get('#grid').innerHTML;
  assert.ok(html.includes(`bfOpenJob('${escaped}'`));
  assert.ok(html.includes(`bfOpenAudiobook('${escaped}'`));
  assert.ok(html.includes(`<div class="book-title">${escaped}</div>`));
  assert.ok(!html.includes(hostile));
});

test('progress, event messages and audiobook source paths are escaped', async () => {
  const d = await dashboard();
  d.context.hostile = hostile;
  d.run('App.selected = hostile');
  d.respond(url => ({ ok: !url.endsWith('/reconfigure'), json: async () => ({ id: hostile, provider: hostile, model: hostile, status: 'done' }) }));
  const stage = { innerHTML: '' };
  await d.context.renderProgress(stage);
  assert.ok(stage.innerHTML.includes(`<div class="t">${escaped}</div>`));
  assert.ok(!stage.innerHTML.includes(hostile));
  const event = d.context.fmtEvent({ Warning: { kind: 'test', message: hostile } });
  assert.ok(event.includes(escaped), event);
  d.run('App.audioWizard = freshAudioWizard(); App.audioWizard.sourceJobId = "job"; App.audioWizard.sourcePath = hostile');
  d.context.renderAudiobook(stage);
  assert.ok(stage.innerHTML.includes(`<div class="fname">${escaped}</div>`));
  d.run('App.screen = "audiobook"; App.audioSelected = "audio"');
  d.respond(() => ({ ok: true, json: async () => ({ status: 'succeeded', warnings: [{ message: hostile }] }) }));
  await d.context.pollAudiobook('audio');
  assert.ok(d.nodes.get('#audio-progress').innerHTML.includes(`<div class="audio-warning">${escaped}</div>`));
});

test('control requests encode IDs and preserve method, JSON headers and body', async () => {
  const d = await dashboard();
  d.respond(() => ({ ok: true, json: async () => ({}) }));
  await d.context.bfJobControl('a/b ?', 'resume', 'test-key');
  assert.equal(d.calls[0][0], '/api/jobs/a%2Fb%20%3F/resume');
  assert.deepEqual(JSON.parse(JSON.stringify(d.calls[0][1])), { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{"api_key":"test-key"}' });
  await d.context.bfJobControl('job', 'pause');
  assert.deepEqual(JSON.parse(JSON.stringify(d.calls[1][1])), { method: 'POST' });
});

test('sign-out posts logout before redirect, including a failed transport', async () => {
  for (const fail of [false, true]) {
    const d = await dashboard();
    d.respond(() => { assert.equal(d.redirects.length, 0); if (fail) throw new Error('offline'); return { ok: true }; });
    await d.context.bfSignOut();
    assert.equal(d.calls[0][0], '/api/auth/logout');
    assert.equal(d.calls[0][1].method, 'POST');
    assert.deepEqual(d.redirects, ['/']);
  }
});

test('unauthorized metadata shows a single signed-out notice', async () => {
  const d = await dashboard();
  d.respond(() => ({ ok: false, status: 401 }));
  await d.context.loadOptions();
  const notice = d.nodes.get('#auth-notice');
  assert.match(notice.textContent, /session expired or signed out/);
  await d.context.loadProviderStatus();
  assert.equal(d.nodes.get('#auth-notice'), notice);
});
