// Run with: node --test crates/emp-app/tests/web_features_contract.cjs
const {assert, tr, translate:uiTranslate, esc, deferred, feature} = require('./web_fixture.cjs');
const {test} = require('node:test');
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
  const usage = feature('service-usage.js', 'createServiceUsage')({ esc, tr, number:String, money:value => 'USD ' + value, dateTime:String,
    renderLifetime:feature('service-presentation.js','renderLifetimeUsage',{esc,tr})});
  const html = usage.renderGroups(data, [
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
  assert.match(sections[0][2], /title="30">30<\/span>/);
  assert.match(sections[1][2], /title="70">70<\/span>/);
  assert.match(sections[2][2], /title="50">50<\/span>/);
  assert.doesNotMatch(sections[2][2], /removed|account:first/);
  assert.match(sections[3][2], /removed/);
  assert.match(sections[3][2], /&lt;legacy&gt;/);
  assert.match(sections[3][2], /<span>—<small>/);
  assert.match(sections[2][2], /title="US\$0">US\$0<\/span>/);
  assert.equal(JSON.stringify(data), original, 'presentation never changes stored categories or rows');
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

test('old bookmarks and page reloads reuse the stored header session', async () => {
  const fallback = sessionFixture('http://localhost/?bootstrap=expired', 'still-valid');
  fallback.replies.push(response(401));
  await fallback.client.establish();
  assert.equal(fallback.client.isSignedIn(), true);
  const f = sessionFixture('http://localhost/', 'stored-session');
  await f.client.establish();
  f.replies.push(response(200));
  await f.client.fetch('/api/config', {headers:{'X-EMP-Session':'caller-token'}});
  assert.equal(f.calls[0].options.headers.get('X-EMP-Session'), 'stored-session');
  assert.equal(f.calls[0].options.credentials, 'omit');
});

test('common call details escape model/session names and keep missing metrics empty', () => {
  const reports = feature('call-reports.js','createCallReports')({modelDisplayName:String,serviceName:String, tr,esc,getLanguage:() => 'en'});
  const html = reports.recordHtml({request_id:'one',session_name:'<script>',client_model:'selected',upstream_model:'sent',response_model:'reported',state:'completed',input_tokens:100,cached_input_tokens:50,output_tokens:20});
  assert.match(html,/&lt;script&gt;/);
  assert.match(html,/Selected in Codex/);assert.match(html,/Sent upstream/);assert.match(html,/Reported upstream/);
  assert.match(html,/50.0%/);assert.match(html,/Not applicable/);
  assert.doesNotMatch(html,/0.00 s|0.0 token/);
});

test('call details distinguish visible generation from the full request rate in both languages', () => {
  for (const language of ['en','zh-CN']) {
    const reports = feature('call-reports.js','createCallReports')({modelDisplayName:String,serviceName:String, tr:uiTranslate(language),esc,getLanguage:() => language});
    const row = {request_id:'one',state:'completed',duration_ms:10000,ttft_ms:1,
      first_content_ms:2000,tokens_per_second:20,generation_tokens_per_second:80};
    const html = reports.recordHtml(row);
    assert.match(html, /TTFT 2\.00 s · TPS 80\.0/);
    assert.match(html, /(?:End-to-end TPS|全程 TPS)<\/span><strong>20\.0/);
    assert.match(html, /(?:excluding reasoning|思考 token)/);
    delete row.generation_tokens_per_second;
    const legacy = reports.recordHtml(row);
    assert.match(legacy, /TTFT 2\.00 s · TPS —/);
    assert.match(legacy, /(?:End-to-end TPS|全程 TPS)<\/span><strong>20\.0/);
  }
});

test('request success separates model failures, recovery and client cancellation', () => {
  const reports = feature('call-reports.js','createCallReports')({modelDisplayName:String,serviceName:String, tr,esc,getLanguage:() => 'en'});
  const result = reports.outcomes({calls:20,completed:9,failed:1,interrupted:2,cancelled:3,recovery_required:4,unknown:1,success_samples:10,failure_breakdown:[{origin:'transport',code:'upstream_error',count:1}]});
  assert.equal(result.value,'90.0%'); assert.equal(result.label,'Request success rate');
  assert.match(result.hint,/1 failed/); assert.match(result.detailsHtml,/4 history resends/); assert.match(result.detailsHtml,/Connection<\/td><td>Response transport failed/);
  assert.doesNotMatch(result.hint,/history resends/);
  assert.match(reports.outcomes({completed:0,failed:1,success_samples:1,failure_breakdown:[{origin:'emp',code:'<script>',count:1}]}).detailsHtml,/&lt;script&gt;/);
  assert.equal(reports.outcomes({calls:5,completed:0,failed:0,success_samples:0}).value,'—');
  assert.equal(reports.outcomes({calls:10,completed:9}).label,'Completion ratio');
  const row = reports.recordHtml({request_id:'error',state:'failed',error_origin:'transport',error_code:'upstream_error'});
  assert.match(row,/Error source/);assert.match(row,/upstream_error/);
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
  let outcomeDialogOpen = false;
  const content = {innerHTML:'',textContent:'',querySelectorAll:() => [],querySelector:()=>null,ownerDocument:{querySelector:()=>outcomeDialogOpen ? {} : null}};
  const root = {innerHTML:'',setAttribute() {},querySelectorAll:() => [],querySelector(selector) { if (selector === '[data-call-content]') return content; if (!controls.has(selector)) controls.set(selector,{value:'',dataset:{}}); return controls.get(selector); }};
  const report = id => ({start:0,end:Date.now()/1000,records:[{request_id:id,client_model:id,state:'completed'}],total:1,offset:0,limit:50});
  const reports = feature('call-reports.js','createCallReports', {
    URLSearchParams, createReportQuery:feature('report-query.js','createReportQuery',{AbortController}), setReportState() {}, setTimeout:fn => { timers.set(fn,fn); return fn; }, clearTimeout:id => timers.delete(id),
  })({modelDisplayName:String,serviceName:String,tr,esc,getLanguage:() => 'en',getState:() => ({}),getActivity:() => ({}),$:() => root,openModal() {},
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
  outcomeDialogOpen=true; content.innerHTML='held snapshot'; reports.live(); assert.equal(content.innerHTML,'held snapshot');
  outcomeDialogOpen=false; reports.live(); assert.match(content.innerHTML,/fresh/);
  reports.refresh(); reports.refresh(); reports.refresh(); assert.equal(timers.size,1);
  await runTimer(); assert.equal(requests.length,3);
  reports.stop(); requests[2].resolve(report('after-close')); await tick();
  assert.doesNotMatch(content.innerHTML,/after-close/); assert.equal(timers.size,0);
  reports.openActivity({dataset:{activityKind:'models',activityModels:'[\"service/model\",\"account/model\"]'}});
  const selected=new URL(requests[3].path,'http://example.invalid').searchParams;
  assert.deepEqual(selected.getAll('models'),['service/model','account/model']);
  assert.equal(selected.has('model'),false); reports.stop();
  requests[3].resolve(report('closed-group')); await tick();
  reports.mount('embedded',{account:'first'},false,report('cached'));
  assert.equal(requests.length,4); assert.match(content.innerHTML,/cached/);
  const shell = root.innerHTML;
  const updated = report('updated');
  reports.mount('embedded',{account:'first'},false,updated);
  assert.equal(root.innerHTML,shell); assert.match(content.innerHTML,/updated/);
  root.onclick({target:{closest:() => ({dataset:{callAction:'next'}})}});
  assert.match(requests[4].path,/offset=50/);
  requests[4].resolve({...report('page-two'),offset:50}); await tick();
  reports.mount('embedded',{account:'first'},false,updated);
  assert.match(content.innerHTML,/page-two/); assert.doesNotMatch(content.innerHTML,/first-page/);
  reports.mount('embedded',{account:'first'},false,report('first-page'));
  assert.match(requests[5].path,/offset=50/);
  assert.match(content.innerHTML,/page-two/);
  reports.mount('embedded',{account:'second'},false,report('new-filter'));
  assert.equal(requests[5].options.signal.aborted,true);
  requests[5].resolve({...report('stale-page'),offset:50}); await tick();
  assert.match(content.innerHTML,/new-filter/); assert.doesNotMatch(content.innerHTML,/page-two/);
  assert.doesNotMatch(content.innerHTML,/stale-page/);
  assert.equal(requests.length,6); reports.stop();
});

test('network diagnostics show specific advice in Chinese and English', async () => {
  for (const language of ['zh', 'en']) {
    const boxes = new Map();
    const $ = id => { if (!boxes.has(id)) boxes.set(id, {textContent:'', innerHTML:''}); return boxes.get(id); };
    const translate = uiTranslate(language === 'en' ? 'en' : 'zh-CN');
    let records = [];
    const diagnostics = feature('diagnostics.js', 'createDiagnostics')({
      api:async () => ({records}), getLanguage:() => language, tr:translate, t:x => x, esc, $, localTimeText:String,
    });
    for (const [error_class, zh, en] of [
      ['proxy_unavailable', '监听端口', 'listening port'],
      ['dns_failure', 'DNS 设置', 'DNS settings'],
      ['tls_failure', '证书信任', 'certificate trust'],
      ['connect_timeout', '代理节点', 'proxy node'],
    ]) {
      records = [{error_class, status:503}];
      assert.equal(await diagnostics.load(), true);
      assert.ok($('diagnostics_records').innerHTML.includes(translate(zh, en)));
    }
    records = [{error_class:'tls_failure', status:502, output_emitted:true}];
    await diagnostics.load();
    assert.ok($('diagnostics_records').innerHTML.includes(translate('先检查任务进度', 'Check the task')));
  }
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

// Native browser effects are accepted in E2E; this VM checks page binding order.
test('usage services display account labels and distinguish unlinked history without exposing fingerprints', () => {
  const usage = feature('usage-data.js', 'createUsageData')({serviceName:String, tr, esc, number:String});
  const amount = {model:'model',requests:1,priced_requests:0,input_tokens:10,output_tokens:2,cached_input_tokens:0,cost_nanos:0};
  const groups = [
    {...amount,category:'subscription',owner:'account:private-fingerprint',owner_name:'Native',owner_email:'n***@fixture.invalid',owner_local_id:'@native'},
    {...amount,category:'subscription',owner:'history:egg'},
    {...amount,category:'subscription',owner:'account:unknown-fingerprint'},
  ];
  const html = usage.table({groups}, 'services');
  assert.match(html, /Native/);
  assert.match(html, /n\*\*\*@fixture.invalid/);
  assert.match(html, /Unlinked historical usage · egg/);
  assert.match(html, /Unlinked account/);
  assert.doesNotMatch(html, /fingerprint|history:egg|account:|@native/);
});

test('usage service colors distinguish colliding identities and stay stable across metrics and filters', () => {
  const usage = feature('usage-data.js', 'createUsageData')({serviceName:String, tr, esc, number:String});
  const groups = ['a','i','q'].map((id,index) => ({category:'subscription',owner:'account:'+id,
    owner_name:['Native','egg','yht'][index],model:'model',requests:1,priced_requests:0,
    input_tokens:10,output_tokens:2,cached_input_tokens:0,cost_nanos:0}));
  const report = {start:0,end:120,bucket:60,groups,series:groups.map(row=>({...row,start:0})),periods:[]};
  const serviceChart = usage.chart(report,'tokens','services');
  const colors = serviceChart.chartSeries.map(row=>row.color);
  assert.equal(new Set(colors).size,3);
  usage.chart(report,'tokens','models');
  for (const group of groups) {
    const filtered = usage.chart({...report,groups:[group],series:[{...group,start:0}]},'cost_nanos','services');
    assert.equal(filtered.chartSeries[0].color,serviceChart.chartSeries.find(row=>row.label===group.owner_name).color);
  }
  const reversed = usage.chart({...report,groups:[...groups].reverse(),series:[...report.series].reverse()},'tokens','services');
  assert.deepEqual(reversed.chartSeries.map(row=>row.color),colors);
  assert.equal(serviceChart.periods.reduce((sum,row)=>sum+row.tokens,0),36);
});
