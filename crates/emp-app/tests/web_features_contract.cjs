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
const serviceUsage = () => feature('service-usage.js', 'createServiceUsage')({esc, tr, number:String, money:value => 'USD ' + value, dateTime:String});

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

test('service details isolate provider usage and discard results after navigation', async () => {
  let box, html;
  const pending = [], calls = [];
  const state = {providers:[{id:'first', name:'<img src=x>', base_url:'https://user:secret@example.invalid/v1?key=secret', protocol:'responses'}, {id:'second', name:'Second'}]};
  const service = feature('service-list.js', 'createServiceList')({
    getState:() => state, $:() => box, tr, esc, presets:{}, icon:() => '', protocolLabel:x => x, authLabel:() => 'API Key',
    usageSummary:serviceUsage(), openModal(_title, body) { html = body; box = {innerHTML:''}; },
    api:async path => { calls.push(path); const value = deferred(); pending.push(value); return value.promise; },
  });
  const first = service.details('first');
  assert.doesNotMatch(html, /<img|Base URL|Authentication|Connection settings/);
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

test('service summaries merge model usage across tiers and categories without dropping unknown or free usage', () => {
  const html = serviceUsage().render({groups:[
    {model:'same-model',category:'native',service_tier:'default',requests:1,input_reports:1,output_reports:1,input_tokens:10,output_tokens:2,priced_requests:1,cost_nanos:5},
    {model:'same-model',category:'subscription',service_tier:'fast',requests:1,input_reports:1,output_reports:1,input_tokens:20,output_tokens:3,priced_requests:1,cost_nanos:7},
    {model:'same-model',requests:1},
    {model:'free-model',requests:1,input_reports:1,output_reports:1,input_tokens:4,output_tokens:0,priced_requests:1,cost_nanos:0},
    {model:'<unknown>',requests:1},
  ]});
  assert.equal((html.match(/<td>same-model<\/td>/g) || []).length, 1);
  assert.match(html, /<td>same-model<\/td><td>30<\/td><td>5<\/td><td>USD 12<\/td>/);
  assert.match(html, /<td>free-model<\/td><td>4<\/td><td>0<\/td><td>USD 0<\/td>/);
  assert.match(html, /<td>&lt;unknown&gt;<\/td><td>—<\/td><td>—<\/td><td>—<\/td>/);
  assert.match(html, /<strong>39<\/strong>/);
  assert.match(html, /3 \/ 5 requests priced/);
});

test('usage groups service types before models and preserves account/tier details and unknown service history', () => {
  const row = (category, owner, input, extra = {}) => ({category, owner, model:'same-model',service_tier:'default',requests:1,input_reports:1,input_tokens:input,output_reports:1,output_tokens:2,priced_requests:1,cost_nanos:0,...extra});
  const data = {groups:[
    row('native','account:first',10),
    row('subscription','account:second',20,{service_tier:'fast'}),
    row('external','local',30),
    row('external','cpa',40),
    row('external','api',50),
    row('external','removed',60),
    row('unknown','history:legacy',0,{model:'<legacy>',input_reports:0,output_reports:0,priced_requests:0}),
  ]};
  const original = JSON.stringify(data);
  const html = serviceUsage().renderGroups(data, [
    {id:'local',execution_backend:'claude_cli',auth_mode:'claude_login'},
    {id:'cpa',execution_backend:'claude_cli'},
    {id:'api'},
  ], item => item.owner);
  const sections = [...html.matchAll(/<section[^>]*data-usage-service-type="([^"]+)"[^>]*>([\s\S]*?)<\/section>/g)];
  assert.deepEqual(sections.map(section => section[1]), ['codex','claude','api','other']);
  for (const section of sections) assert.equal((section[2].match(/<details class="usage-model">/g) || []).length, section[1] === 'other' ? 2 : 1);
  assert.match(sections[0][2], /account:first/);
  assert.match(sections[0][2], /account:second/);
  assert.match(sections[0][2], /fast/);
  assert.match(sections[0][2], /<span>30<small>/);
  assert.match(sections[1][2], /<span>70<small>/);
  assert.match(sections[2][2], /<span>50<small>/);
  assert.doesNotMatch(sections[2][2], /removed|account:first/);
  assert.match(sections[3][2], /removed/);
  assert.match(sections[3][2], /&lt;legacy&gt;/);
  assert.match(sections[3][2], /<span>—<small>/);
  assert.match(sections[2][2], /<span>USD 0<\/span>/);
  assert.equal(JSON.stringify(data), original, 'presentation never changes stored categories or rows');
});

test('service badges own settings navigation while list actions retain refresh, trend and model settings', () => {
  const box = {innerHTML:'',contains:() => false};
  const state = {native_account:{id:'@native',native:true,credential_set:true},providers:[{id:'local',execution_backend:'claude_cli',auth_mode:'claude_login'},{id:'api'}]};
  const service = feature('service-list.js', 'createServiceList', {document:{activeElement:null}})({
    getState:() => state, $:() => box, tr, esc, presets:{}, icon:() => '',activity:() => '',
    accountSummary:() => '<button data-ui-action="account-details"></button>',quotaMeters:() => '',
    refreshingAccounts:new Set(),refreshErrors:{},quotaAnimationAccounts:new Set(),updateActivityDots:() => {},
  });
  service.render();
  assert.doesNotMatch(box.innerHTML, /data-ui-action="(?:account|provider)-edit"/);
  for (const action of ['account-details','provider-details','account-refresh','account-quota-history','provider-model-settings','provider-quota-refresh','provider-quota-history']) assert.match(box.innerHTML, new RegExp('data-ui-action="'+action+'"'));
  assert.match(service.tabs('@native','account'), /data-ui-action="account-edit"/);
  assert.match(service.tabs('api','provider','edit'), /data-ui-action="provider-edit"[^>]*aria-selected="true"/);
  assert.match(service.tabs('duplicate','account','details',true), /data-ui-action="account-edit"[^>]*disabled/);
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

test('common call details escape model/session names and keep missing metrics empty', () => {
  const reports = feature('call-reports.js','createCallReports')({tr,esc,getLanguage:() => 'en'});
  const html = reports.recordHtml({request_id:'one',session_name:'<script>',client_model:'selected',upstream_model:'sent',response_model:'reported',state:'completed',input_tokens:100,cached_input_tokens:50,output_tokens:20});
  assert.match(html,/&lt;script&gt;/);
  assert.match(html,/Selected in Codex/);assert.match(html,/Sent upstream/);assert.match(html,/Reported upstream/);
  assert.match(html,/50.0%/);assert.match(html,/Not applicable/);
  assert.doesNotMatch(html,/0.00 s|0.0 token/);
});

test('report refresh cancels superseded requests, retains data on error and ignores closure', async () => {
  const pending = [], states = [], results = [];
  const query = feature('report-query.js','createReportQuery',{AbortController})({
    api:(_path,options) => { const request=deferred(); pending.push({...request,signal:options.signal}); return request.promise; },
    onState:(loading,error) => states.push([loading,error]),onResult:value => results.push(value),
  });
  const old = query.run('/old'), fresh = query.run('/fresh');
  assert.equal(pending[0].signal.aborted,true);
  pending[1].resolve('new data'); await fresh;
  pending[0].resolve('stale data'); await old;
  assert.deepEqual(results,['new data']);
  assert.deepEqual(states.at(-1),[false,'']);
  const failed = query.run('/failed');
  pending[2].reject(new Error('Network unavailable')); await failed;
  assert.deepEqual(results,['new data']);
  assert.deepEqual(states.at(-1),[false,'Network unavailable']);
  const closed = query.run('/closed'); query.cancel();
  assert.equal(pending[3].signal.aborted,true);
  pending[3].resolve('closed result'); await closed;
  assert.deepEqual(results,['new data']);
  assert.deepEqual(states.at(-1),[false,'']);
});

test('call queries coalesce events and discard stale filters and closed windows', async () => {
  const requests = [], timers = new Map(), controls = new Map();
  const content = {innerHTML:'',textContent:'',querySelectorAll:() => []};
  const root = {innerHTML:'',setAttribute() {},querySelectorAll:() => [],querySelector(selector) { if (selector === '[data-call-content]') return content; if (!controls.has(selector)) controls.set(selector,{value:'',dataset:{}}); return controls.get(selector); }};
  const report = id => ({start:0,end:Date.now()/1000,records:[{request_id:id,client_model:id,state:'completed'}],total:1,offset:0,limit:50});
  const reports = feature('call-reports.js','createCallReports', {
    URLSearchParams, createReportQuery:feature('report-query.js','createReportQuery',{AbortController}), setReportState:feature('report-query.js','setReportState'), setTimeout:fn => { timers.set(fn,fn); return fn; }, clearTimeout:id => timers.delete(id),
  })({tr,esc,getLanguage:() => 'en',getState:() => ({}),getActivity:() => ({}),$:() => root,openModal() {},
    periodPickerHtml:() => '',registerPeriodPicker:(_id,_preset,onChange,range) => onChange(range || {start:0,end:Date.now()/1000}),
    api:(path,options) => { const pending=deferred(); requests.push({path,options,...pending}); return pending.promise; }});
  const tick = async () => { await Promise.resolve(); await Promise.resolve(); };
  const runTimer = async () => { const fn=timers.keys().next().value; timers.delete(fn); fn(); await tick(); };
  reports.openActivity({dataset:{activityKind:'account',activityId:'first'}});
  assert.match(requests[0].path,/account=first/);
  root.onchange({target:{dataset:{callFilter:'service'},value:'account:second'}});
  requests[0].resolve(report('stale')); await tick();
  assert.equal(content.innerHTML,'');
  assert.equal(requests[0].options.signal.aborted,true);
  assert.equal(timers.size,0);
  assert.match(requests[1].path,/account=second/);
  requests[1].resolve(report('fresh')); await tick();
  assert.match(content.innerHTML,/fresh/); assert.doesNotMatch(content.innerHTML,/stale/);
  reports.refresh(); reports.refresh(); reports.refresh(); assert.equal(timers.size,1);
  await runTimer(); assert.equal(requests.length,3);
  reports.stop(); requests[2].resolve(report('after-close')); await tick();
  assert.doesNotMatch(content.innerHTML,/after-close/); assert.equal(timers.size,0);
  reports.openActivity({dataset:{activityKind:'models',activityModels:'[\"service/model\",\"account/model\"]'}});
  const selected=new URL(requests[3].path,'http://example.invalid').searchParams;
  assert.deepEqual(selected.getAll('models'),['service/model','account/model']);
  assert.equal(selected.has('model'),false); reports.stop();
  requests[3].resolve(report('closed-group')); await tick();
});

test('closing diagnostics discards in-flight reports without a polling timer', async () => {
  const requests = [], intervals = new Set(), boxes = new Map();
  const $ = id => { if (!boxes.has(id)) boxes.set(id, {textContent:'', innerHTML:'', querySelectorAll:() => []}); return boxes.get(id); };
  let diagnostics;
  diagnostics = feature('diagnostics.js', 'createDiagnostics', {
    document:{visibilityState:'visible'}, setInterval:fn => { intervals.add(fn); return fn; }, clearInterval:fn => intervals.delete(fn),
  })({api:path => { const pending = deferred(); requests.push({path, ...pending}); return pending.promise; },
    callReports:{mount() {}}, getLanguage:() => 'en', tr, t:x => x, esc, $, localTimeText:String, notice() {},
    openModal() { diagnostics.stop(); },
  });
  diagnostics.open();
  assert.equal(intervals.size, 0);
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
  assert.equal(vm.runInContext('typeof openSettings + ":" + typeof openDiagnostics + ":" + typeof callReports.openActivity', context), 'function:function:function');
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
  const weeklyOnly = run('quotaMetersHtml({quota:{rate_limits:{primary:{usedPercent:60,windowDurationMins:10080,resetsAt:2000000000}}}})');
  assert.match(weeklyOnly, /233%/);
  assert.match(weeklyOnly, /class="quota-reset">1m111s<\/span>/);
  assert.doesNotMatch(weeklyOnly, /aria-valuenow="233"/);
  assert.equal((weeklyOnly.match(/data-reset-countdown=/g) || []).length, 1);
  run(`quotaChartWidth=()=>600;globalThis.series=[{window_minutes:300,points:[{observed_at:100,remaining_percent:76},{observed_at:200,remaining_percent:60}]}]`);
  run('quotaChartSvg(series,{start:100,end:200},300)');
  assert.equal(run('quotaChartData.points[0].value'), 24);
  assert.equal(run('series[0].points[0].remaining_percent'), 76);
  run("quotaDisplay.set('remaining')");
  run('quotaChartSvg(series,{start:100,end:200},300)');
  assert.equal(run('quotaChartData.points[0].value'), 76);
});
