// Run with: node --test crates/emp-app/tests/web_features_contract.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {test} = require('node:test');

const web = path.join(__dirname, '../web');
const tr = (_zh, en) => en;
const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; };
function feature(file, factory, globals = {}) {
  const context = vm.createContext({Headers, URL, Blob, ...globals});
  vm.runInContext(fs.readFileSync(path.join(web, file), 'utf8'), context, {filename:file});
  return context[factory];
}
function sessionFixture(url = 'http://localhost/?bootstrap=once', saved = '') {
  let token = saved;
  const calls = [], replies = [];
  const window = {location:{href:url}, history:{replaceState(_state, _unused, target) { window.location.href = new URL(target, url).href; }}};
  const client = feature('management-client.js', 'createManagementClient')({
    tr, window, storage:() => ({getItem:() => token, setItem:(_key, value) => { token = value; }, removeItem:() => { token = ''; }}),
    fetch:async (url, options) => { calls.push({url, options, page:window.location.href}); return replies.shift(); },
  });
  return {client, calls, replies, token:() => token};
}
const response = (status, body = {}) => new Response(JSON.stringify(body), {status, headers:{'Content-Type':'application/json'}});

test('management login removes bootstrap secrets and distinguishes upstream rejection from expired sessions', async () => {
  const f = sessionFixture();
  f.replies.push(response(200, {session:'signed-in'}));
  await f.client.establish();
  assert.equal(f.calls[0].page, 'http://localhost/');
  assert.equal(f.calls[0].options.headers['X-EMP-Bootstrap'], 'once');
  assert.equal(f.calls[0].options.body, '{}');
  f.replies.push(response(401, {error:{type:'auth', message:'upstream rejected key'}}));
  await assert.rejects(f.client.request('/api/test'), error => error.status === 401 && !error.session);
  assert.equal(f.token(), 'signed-in');
  assert.equal(f.calls[1].options.headers.get('X-EMP-Session'), 'signed-in');
  assert.equal(f.calls[1].options.credentials, 'omit');
  assert.equal(f.calls[1].options.redirect, 'error');
  f.replies.push(response(401, {error:{message:'management session is required'}}));
  await assert.rejects(f.client.request('/api/config'), error => error.session);
  assert.equal(f.token(), '');
  assert.equal(f.client.isSignedIn(), false);
  f.client.receiveStorageEvent({key:'emp_management_session_v1', newValue:'renewed'});
  assert.equal(f.client.isSignedIn(), true);
  f.replies.push(response(200));
  await f.client.fetch('/api/quota/events');
  assert.equal(f.calls[3].options.headers.get('X-EMP-Session'), 'renewed');
});

test('old bookmarks and Python cookie sessions retain their existing login path', async () => {
  const fallback = sessionFixture('http://localhost/?bootstrap=expired', 'still-valid');
  fallback.replies.push(response(401));
  await fallback.client.establish();
  assert.equal(fallback.client.isSignedIn(), true);
  for (const url of ['http://localhost/', 'http://localhost/?bootstrap=old']) {
    const f = sessionFixture(url, 'unrelated-header-token');
    f.replies.push(response(404));
    await f.client.establish();
    assert.equal(f.calls[0].options.body, '{}');
    f.replies.push(response(200));
    await f.client.fetch('/api/config', {headers:{'X-EMP-Session':'caller-token'}});
    assert.equal(f.calls[1].options.headers.has('X-EMP-Session'), false);
    assert.equal(f.calls[1].options.credentials, 'same-origin');
  }
});

test('settings wait for a save result and preserve state and disabled controls after failure', async () => {
  const state = {auto_enable_on_start:true, accounts:[]};
  const button = {dataset:{id:'auto_enable_on_start'}, disabled:false, isConnected:true, value:'true', getAttribute() { return this.value; }, setAttribute(_key, value) { this.value = value; }};
  const unavailable = {disabled:true};
  const status = {textContent:''}, pending = deferred();
  let candidate;
  const settings = feature('settings.js', 'createSettings')({
    getState:() => state, cloneState:() => structuredClone(state), tr, t:x => x, esc, openModal() {},
    $:id => id === 'modal_body' ? {querySelectorAll:() => [button, unavailable]} : status,
    persistState:(_message, value) => { candidate = value; return pending.promise; },
  });
  const saving = settings.toggle(button);
  assert.equal(button.disabled, true);
  assert.equal(candidate.auto_enable_on_start, false);
  assert.equal(state.auto_enable_on_start, true);
  pending.reject(new Error('save failed'));
  await saving;
  assert.equal(button.value, 'true');
  assert.equal(button.disabled, false);
  assert.equal(unavailable.disabled, true);
  assert.equal(status.textContent, 'save failed');
});

test('request details follow new snapshots and stop rendering after the modal closes', () => {
  let state = {providers:[{id:'service', name:'First name'}]}, activity;
  const box = {innerHTML:''};
  let details;
  details = feature('request-details.js', 'createRequestDetails')({
    getState:() => state, getActivity:() => activity, getLanguage:() => 'en', tr, esc,
    $:() => box, utcOffsetLabel:() => 'UTC', protocolLabel:x => x || '',
    openModal(_title, html) { details.reset(); box.innerHTML = html; },
  });
  activity = {requests:details.normalize([{request_id:'one', model_id:'service/model', provider_id:'service', state:'active'}, {request_id:'two', model_id:'another/model', provider_id:'another', state:'active'}])};
  details.open({dataset:{activityKind:'provider', activityId:'service'}});
  assert.match(box.innerHTML, /First name/);
  assert.doesNotMatch(box.innerHTML, /another\/model/);
  state = {providers:[{id:'service', name:'<New name>'}]};
  activity.requests[0].state = 'completed';
  details.refresh();
  assert.match(box.innerHTML, /&lt;New name&gt;/);
  assert.match(box.innerHTML, /Completed/);
  details.reset();
  box.innerHTML = 'another modal';
  details.refresh();
  assert.equal(box.innerHTML, 'another modal');
});

test('closing diagnostics discards in-flight reports and cancels its scheduled refresh', async () => {
  const requests = [], intervals = new Set(), boxes = new Map();
  const $ = id => { if (!boxes.has(id)) boxes.set(id, {textContent:'', innerHTML:'', querySelectorAll:() => []}); return boxes.get(id); };
  let diagnostics;
  diagnostics = feature('diagnostics.js', 'createDiagnostics', {
    document:{visibilityState:'visible'}, setInterval:fn => { intervals.add(fn); return fn; }, clearInterval:fn => intervals.delete(fn),
  })({api:path => { const pending = deferred(); requests.push({path, ...pending}); return pending.promise; },
    getLanguage:() => 'en', tr, t:x => x, esc, $, localTimeText:String, notice() {},
    openModal() { diagnostics.stop(); },
  });
  diagnostics.open();
  assert.equal(intervals.size, 1);
  diagnostics.stop();
  assert.equal(intervals.size, 0);
  $('diagnostics_summary').textContent = 'other modal';
  $('support_report').innerHTML = 'other report';
  requests[0].resolve({health:{sample_count:2}});
  requests[1].resolve({configuration:{path:'old report'}});
  await Promise.all(requests.map(r => r.promise));
  assert.equal($('diagnostics_summary').textContent, 'other modal');
  assert.equal($('support_report').innerHTML, 'other report');
  const fresh = diagnostics.load();
  requests[2].resolve({health:{sample_count:3}});
  await fresh;
  assert.match($('diagnostics_summary').textContent, /latest 3 requests/);
});

test('the shipped page initializes its feature bindings using its own script order', () => {
  const html = fs.readFileSync(path.join(web, 'index.html'), 'utf8');
  const pending = deferred();
  const context = vm.createContext({Headers, URL, Blob, console,
    fetch:() => pending.promise,
    localStorage:{getItem:() => ''},
    window:{location:{href:'http://localhost/'}, addEventListener() {}},
    document:{addEventListener() {}, visibilityState:'hidden'},
  });
  for (const script of html.matchAll(/<script(?: src="([^"]+)")?>([\s\S]*?)<\/script>/g)) {
    const filename = script[1] ? path.basename(script[1]) : 'page.js';
    const source = script[1] ? fs.readFileSync(path.join(web, filename), 'utf8') : script[2];
    vm.runInContext(source, context, {filename});
  }
  assert.equal(vm.runInContext('typeof openSettings + ":" + typeof openDiagnostics + ":" + typeof openActivityDetails', context), 'function:function:function');
});
