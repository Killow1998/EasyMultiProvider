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

test('unified chooser preserves local Claude identity and exposes direct connection choices', () => {
  let html = '';
  const state = {native_account:{credential_set:true}, providers:[{id:'existing-local', name:'Renamed account', execution_backend:'claude_cli', auth_mode:'claude_login'}]};
  const chooser = feature('service-chooser.js', 'createServiceChooser')({
    tr, esc, presets:{chatgpt:{name:'Legacy forward'}, openai:{name:'OpenAI'}, anthropic:{name:'Anthropic'}},
    icon:() => '', getState:() => state, openModal:(_title, body) => { html = body; },
  });
  chooser.open();
  assert.match(html, /data-ui-action="claude-local-choose" data-id="existing-local"/);
  assert.match(html, /data-ui-action="claude-cpa-choose"/);
  assert.match(html, /data-ui-action="service-import-account"/);
  assert.match(html, /data-ui-action="service-native"/);
  assert.doesNotMatch(html, /Legacy forward|execution_backend|claude-provider-choose/);
});

test('service details isolate provider usage, escape labels and discard results after navigation', async () => {
  let box, html;
  const pending = [], calls = [];
  const state = {providers:[{id:'first', name:'<img src=x>', base_url:'https://user:secret@example.invalid/v1?key=secret', protocol:'responses'}, {id:'second', name:'Second'}]};
  const service = feature('service-list.js', 'createServiceList')({
    getState:() => state, $:() => box, tr, esc, presets:{}, icon:() => '', protocolLabel:x => x, authLabel:() => 'API Key',
    usageNumber:String, usageMoney:String, openModal(_title, body) { html = body; box = {innerHTML:''}; },
    api:async path => { calls.push(path); const value = deferred(); pending.push(value); return value.promise; },
  });
  const first = service.details('first');
  assert.match(html, /&lt;img src=x&gt;/);
  assert.doesNotMatch(html, /user:secret|key=secret/);
  assert.match(calls[0], /category=external/);
  pending[0].resolve({groups:[{category:'external',owner:'first',model:'own-model'}, {category:'external',owner:'second',model:'other-model'}]});
  await first;
  assert.match(box.innerHTML, /own-model/);
  assert.doesNotMatch(box.innerHTML, /other-model/);
  const stale = service.details('first');
  const current = service.details('second');
  pending[1].reject(new Error('old request failed'));
  await stale;
  assert.equal(box.innerHTML, '');
  pending[2].resolve({groups:[]});
  await current;
  assert.match(box.innerHTML, /No usage recorded/);
});

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

test('quota mode changes bars and history together without changing recorded values', () => {
  const html = fs.readFileSync(path.join(web, 'index.html'), 'utf8');
  const storage = new Map();
  const context = vm.createContext({Headers, URL, Blob, console,
    fetch:() => new Promise(() => {}),
    localStorage:{getItem:key => storage.get(key), setItem:(key, value) => storage.set(key,value)},
    window:{location:{href:'http://localhost/'}, addEventListener() {}},
    document:{addEventListener() {}, visibilityState:'hidden'},
  });
  for (const script of html.matchAll(/<script(?: src="([^"]+)")?>([\s\S]*?)<\/script>/g)) {
    vm.runInContext(script[1] ? fs.readFileSync(path.join(web, path.basename(script[1])), 'utf8') : script[2], context);
  }
  const run = source => vm.runInContext(source, context);
  run(`language='en'; globalThis.account={quota:{rate_limits:{primary:{used_percent:24,window_minutes:300},secondary:{used_percent:90,window_minutes:10080}}}}`);
  assert.match(run('quotaMetersHtml(account)'), /aria-valuenow="76"/);
  assert.match(run('quotaMetersHtml(account)'), /5h · Remaining/);
  run("quotaDisplay.set('used')");
  assert.equal(storage.get('emp.quotaDisplay'), 'used');
  assert.match(run('quotaMetersHtml(account)'), /aria-valuenow="24"/);
  assert.match(run('quotaMetersHtml(account)'), /5h · Used/);
  assert.match(run('quotaMetersHtml(account)'), /quota-meter is-low/); // 90% used stays red.
  assert.equal(run('account.quota.rate_limits.primary.used_percent'), 24);
  assert.doesNotMatch(run('quotaMetersHtml({quota_pending:true})'), /aria-valuenow|233%|100%/);
  run(`quotaChartWidth=()=>600;globalThis.series=[{window_minutes:300,points:[{observed_at:100,remaining_percent:76},{observed_at:200,remaining_percent:60}]}]`);
  run('quotaChartSvg(series,{start:100,end:200},300)');
  assert.equal(run('quotaChartData.points[0].value'), 24);
  assert.equal(run('series[0].points[0].remaining_percent'), 76);
  run("quotaDisplay.set('remaining')");
  run('quotaChartSvg(series,{start:100,end:200},300)');
  assert.equal(run('quotaChartData.points[0].value'), 76);
});
