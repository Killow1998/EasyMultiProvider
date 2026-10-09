// Public feature interfaces own requests even when a transport ignores abort.
const {test} = require('node:test');
const {assert, feature, tr, esc, deferred} = require('./web_fixture.cjs');
const tick = () => new Promise(resolve => setImmediate(resolve));
const queries = () => {
  const calls=[];
  return {calls, api:(path, options={}) => { const pending=deferred(); calls.push({path,options,...pending}); return pending.promise; }};
};
const formats = {periodPickerHtml:() => '', periodLocalInput:() => '',timeAxisSvg:() => ''};

test('usage owns scan cancellation, fallback timers and pricing-save navigation', async () => {
  let nodes={}, report, period, timerId=0;
  const timers=new Map(), q=queries(), saving=deferred();
  const get=id => nodes[id];
  function openModal() {
    report.stop();
    const status={textContent:'',dataset:{}};
    nodes={usage_report:{querySelector:() => status,querySelectorAll:() => [],setAttribute(){},getAttribute:() => 'false'},usage_category:{value:'all'},usage_bucket:{value:'hour'},usage_result:{innerHTML:''}};
  }
  const periods={...formats,periodPickers:{},setPeriod(){}};
  periods.registerPeriodPicker=(id,preset,callback) => { period={start:1,end:10,preset}; periods.periodPickers[id]=period; callback(period,{user:true}); };
  const context={setTimeout:callback => {timers.set(++timerId,callback);return timerId;},clearTimeout:id => timers.delete(id)};
  report=feature('usage-report.js','createUsageReport', {...context,createReportQuery:feature('report-query.js','createReportQuery'),setReportState:feature('report-query.js','setReportState')})({
    ...q,$:get,tr,esc,getState:() => ({providers:[],pricing_aliases:{}}),getLanguage:() => 'en',persistState:() => saving.promise,openModal,
    serviceUsage:{renderGroups:() => ''},callReports:{open(){}},eventsConnected:() => false,periods,
  });
  const data={start:1,end:10,totals:{requests:0,priced_requests:0},periods:[],pricing:{},history:{running:true}};
  report.open();
  q.calls[0].resolve(data); await tick();
  assert.equal(timers.size,1);
  const old=nodes.usage_report;
  old.onclick({target:{closest:() => ({dataset:{usageAction:'pricing'}})}});
  report.stop(); nodes={}; saving.resolve(); await tick();
  assert.equal(timers.size,0,'a late save must not restart polling in another window');
  report.open();
  const stale=q.calls[1]; report.refresh();
  assert.equal(stale.options.signal.aborted,true);
  stale.resolve(data); await tick(); assert.equal(nodes.usage_result.innerHTML,'');
  q.calls[2].resolve({...data,history:{}}); await tick();
  assert.match(nodes.usage_result.innerHTML,/No usage recorded/);
  nodes.usage_report.onclick({target:{closest:() => ({dataset:{usageAction:'scan'}})}});
  const scan=q.calls[3]; assert.equal(scan.path,'/api/usage/scan');
  report.stop(); nodes={}; assert.equal(scan.options.signal.aborted,true);
  scan.resolve({}); await tick(); assert.equal(q.calls.length,4);
});

test('account details cancel reads and cannot update another service after a save', async () => {
  let nodes={}, details, mounted=0;
  const q=queries(), save=deferred();
  const state={accounts:[{id:'first',name:'First'},{id:'second',name:'Second'}]};
  const services={open() { details.stop();nodes={account_usage:{innerHTML:''},account_alias:{value:'New alias'},modal_status:{textContent:''}}; },mount() {mounted++;}};
  details=feature('account-details.js','createAccountDetails')({...q,getState:() => state,$:id => nodes[id],tr,esc,services,
    serviceUsage:{render:data => data.model},persistState:() => save.promise,uiIcon:x => x,subscriptionPlanLabel:() => '',resetCreditCount:() => 0});
  const first=details.open('first'), second=details.open('second');
  assert.equal(q.calls[0].options.signal.aborted,true);
  q.calls[0].resolve({model:'stale'}); await first; assert.equal(nodes.account_usage.innerHTML,'');
  q.calls[1].resolve({model:'fresh'}); await second; assert.equal(nodes.account_usage.innerHTML,'fresh');
  const alias=details.saveAlias({dataset:{id:'second'}});
  details.stop(); nodes={}; save.resolve(); await alias;
  assert.equal(mounted,0,'saving a departed account must not reopen its tabs');
});

test('model settings cancels cached reads, preserves refreshed data and closes work', async () => {
  let nodes={}, settings;
  const q=queries(), button={disabled:false};
  const document={querySelectorAll:() => []};
  settings=feature('model-settings.js','createProviderModelSettings',{document})({...q,getState:() => ({providers:[{id:'api'}],models:[]}),$:id => nodes[id],tr,esc,
    openModal() {settings.stop(); nodes=Object.fromEntries(['provider_model_settings','modal_submit','modal_status','model_list_updated','discovered_models','discovered_search','discovered_select_all','discovered_clear_all','discovered_count'].map(id => [id,{value:'',textContent:'',querySelector:() => button}]));},
    closeModal() {settings.stop();nodes={};},modelSort:() => 0,capabilitySummary:() => '',formatDate:() => '',errorText:error => error.message,onSaved(){}});
  const opening=settings.open('api'), refresh=settings.refresh();
  assert.equal(q.calls[0].options.signal.aborted,true);
  q.calls[1].resolve({models:[],updated_at:'2026-10-09T00:00:00Z'});await refresh;
  const updated=nodes.model_list_updated.textContent;
  q.calls[0].resolve({models:[],updated_at:'2020-01-01T00:00:00Z'}); await opening;
  assert.equal(nodes.model_list_updated.textContent,updated,'late cache data cannot overwrite a fresh catalog');
  const closing=settings.refresh();settings.stop();nodes={};
  assert.equal(q.calls[2].options.signal.aborted,true);
  q.calls[2].reject(new Error('transport ignored abort'));await closing;
});

test('model editor cancels metadata when routes change and discards replies to a closed draft', async () => {
  let nodes={}, editor, notices=[];
  const q=queries(), document={querySelectorAll:() => []};
  editor=feature('model-editor.js','createModelEditor',{document})({...q,getState:() => ({providers:[{id:'api'}],models:[]}),$:id => nodes[id],tr,esc,
    openModal() {editor.stop();nodes=Object.fromEntries(['model_editor','modal_model_provider','modal_model_upstream','modal_model_route','modal_model_upstream_label','modal_model_context','modal_model_context_inspect','modal_model_context_usable','modal_model_reasoning'].map(id => [id,{value:'',textContent:''}]));nodes.modal_model_upstream.value='first';},
    closeModal(){editor.stop();nodes={};},persistState(){},notice:text => notices.push(text),upstreamErrorText:error => error.message,testModelAudio(){},testModelVision(){},
    format:{visionStatus:() => 'unknown',modalityLabel:x => x,compactContext:String,usableContext:() => 1}});
  editor.open();nodes.model_editor.onclick({target:{closest:() => ({dataset:{editorAction:'inspect'}})}});
  nodes.modal_model_upstream.value='second';nodes.model_editor.oninput({target:{dataset:{editorInput:'route'}}});
  assert.equal(q.calls[0].options.signal.aborted,true);
  q.calls[0].resolve({context_window:999999});await tick();assert.equal(nodes.modal_model_context.value,'');
  nodes.model_editor.onclick({target:{closest:() => ({dataset:{editorAction:'inspect'}})}});
  editor.stop();nodes={};q.calls[1].resolve({context_window:999999});await tick();assert.equal(notices.length,0);
});
