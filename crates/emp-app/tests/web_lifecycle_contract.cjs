// Public feature interfaces own requests even when a transport ignores abort.
const {test} = require('node:test');
const {assert, feature, tr, esc, deferred} = require('./web_fixture.cjs');
const releaseHistoryText = feature('release-notes.js', 'releaseHistoryText');
const tick = () => new Promise(resolve => setImmediate(resolve));
const queries = () => {
  const calls=[];
  return {calls, api:(path, options={}) => { const pending=deferred(); calls.push({path,options,...pending}); return pending.promise; }};
};
const formats = {periodPickerHtml:() => '', periodLocalInput:() => '',timeAxisSvg:() => ''};

test('connection Save retains the stored key and cannot close a replacement window',async()=>{
  const saving=deferred(),owner={isConnected:true};let submit,closed=0,saved;
  const state={accounts:[],providers:[{id:'api',name:'API',base_url:'https://fixture.invalid/v1',protocol:'responses',auth_mode:'api_key',api_key_set:true}],models:[]};
  const nodes={modal_body:{firstElementChild:owner},modal_provider_name:{value:'New name'},modal_provider_base:{value:'https://fixture.invalid/v1'},modal_provider_key:{value:''},modal_provider_protocol:{value:'responses'},modal_provider_auth:{value:'api_key'}};
  const forms=feature('service-connections.js','createServiceConnections')({getState:()=>state,cloneState:()=>structuredClone(state),$:id=>nodes[id],tr,esc,notice(){},openModal(_title,_body,_label,save){submit=save;},closeModal(){closed++;},
    persistState(_message,candidate){saved=candidate;return saving.promise;}});
  forms.editProvider('api');const work=submit();
  assert.equal(saved.providers[0].api_key,'••••••••','empty input must preserve the stored key');
  assert.equal(saved.providers[0].name,'New name');owner.isConnected=false;saving.resolve();await work;
  assert.equal(closed,0,'an older Save must not close the new window');
});

test('update window owns requests and discards a late check after closing',async()=>{
  const q=queries();let updater,nodes={};
  updater=feature('updates.js','createUpdates')({...q,$:id=>nodes[id],tr,t:key=>key,esc,notice(){},getLanguage:()=>"en",releaseHistoryText,visible:()=>true,isPreview:()=>false,
    openModal(){updater.stop();nodes={update_install:{disabled:false},update_status:{textContent:''}};}});
  const opening=updater.open();assert.equal(q.calls.length,1);
  updater.stop();nodes={};assert.equal(q.calls[0].options.signal.aborted,true);
  q.calls[0].resolve({state:'available',latest_version:'9.0.0'});await opening;
  assert.equal(q.calls.length,1,'a closed window must not restart polling');
  assert.equal(updater.info(),null,'a late reply must not replace the current update metadata');
});

test('completed update check retains release notes without an extra status request',async()=>{
  const q=queries(),nodes=Object.fromEntries(['update_status','update_version','update_notes','update_progress','update_install','update_release','update_commands','update_manual'].map(id=>[id,{}]));
  const updater=feature('updates.js','createUpdates')({...q,$:id=>nodes[id],tr,t:key=>key,esc,notice(){},getLanguage:()=>"en",releaseHistoryText,visible:()=>true,isPreview:()=>true,openModal(){}});
  const opening=updater.open();q.calls[0].resolve({state:'available',current_version:'0.13.0',latest_version:'1.0.0',supported:false,release_notes:'Literal <script> markup'});await opening;await tick();
  assert.equal(q.calls.length,1,'terminal checks must not overwrite notes with an older status read');
  assert.equal(nodes.update_notes.textContent,'Literal <script> markup');
  assert.equal(nodes.update_version.textContent,'v0.13.0 → v1.0.0');
  assert.equal(nodes.update_install.disabled,true);updater.stop();
});

test('cached notes appear before a new check replies and remain readable after failure',async()=>{
  const q=queries(),nodes=Object.fromEntries(['update_status','update_version','update_notes','update_progress','update_install','update_release','update_commands','update_manual'].map(id=>[id,{}]));
  const updater=feature('updates.js','createUpdates')({...q,$:id=>nodes[id],tr,t:key=>key,esc,notice(){},getLanguage:()=>"en",releaseHistoryText,visible:()=>true,isPreview:()=>false,openModal(){nodes.update_notes.textContent='Loading';}});
  const cached={state:'available',current_version:'0.13.7',latest_version:'1.0.0',release_notes:'Cached notes'};
  updater.hint(cached);
  const opening=updater.open();
  assert.equal(nodes.update_notes.textContent,'Cached notes','opening must not wait for HTTP');
  q.calls[0].resolve({...cached,state:'checking'});await opening;
  assert.equal(nodes.update_notes.textContent,'Cached notes');
  q.calls[1].resolve({...cached,state:'error',error:'check_failed'});await tick();
  assert.equal(nodes.update_notes.textContent,'Cached notes');
  assert.equal(nodes.update_install.hidden,true,'failed validation must not offer an install');
  updater.stop();
});

test('update reconnects the existing page once, and stops on rollback, expiry or close',async()=>{
  for(const outcome of ['updated','rollback','expired','closed','unauthorized']) {
    const q=queries(),timers=new Map();let next=0,now=1000,reloads=0;
    const nodes=Object.fromEntries(['update_status','update_version','update_notes','update_progress','update_install','update_release','update_commands','update_manual'].map(id=>[id,{}]));
    const updater=feature('updates.js','createUpdates',{Date:{now:()=>now},setTimeout:callback=>{timers.set(++next,callback);return next;},clearTimeout:id=>timers.delete(id)})({
      ...q,$:id=>nodes[id],tr,t:key=>key,esc,notice(){},getLanguage:()=>"en",releaseHistoryText,visible:()=>true,isPreview:()=>false,openModal(){},reload(){reloads++;}
    });
    updater.render({state:'available',current_version:'0.13.4',latest_version:'0.13.5',supported:true});
    const installing=updater.request('install');q.calls[0].resolve({state:'installing',current_version:'0.13.4',latest_version:'0.13.5'});await installing;
    q.calls[1].reject(new TypeError('connection refused'));await tick();
    assert.equal(timers.size,1);
    assert.match(nodes.update_status.textContent,/Waiting for EMP/);
    if(outcome==='closed') {updater.stop();assert.equal(timers.size,0);assert.equal(reloads,0);continue;}
    const callback=[...timers.values()][0];timers.clear();callback();
    if(outcome==='updated') q.calls[2].resolve({state:'idle',current_version:'0.13.5'});
    if(outcome==='rollback') q.calls[2].resolve({state:'error',current_version:'0.13.4',error:'install_rolled_back'});
    if(outcome==='expired') {now+=180001;q.calls[2].reject(new TypeError('connection refused'));}
    if(outcome==='unauthorized') q.calls[2].reject(Object.assign(new Error('Session expired'),{status:401}));
    await tick();assert.equal(timers.size,0,outcome);assert.equal(reloads,outcome==='updated'?1:0,outcome);
    if(outcome==='rollback') assert.match(nodes.update_status.textContent,/previous version has been restored/i);
    if(outcome==='expired') assert.match(nodes.update_status.textContent,/refresh this page/);
    if(outcome==='unauthorized') assert.equal(nodes.update_status.textContent,'Session expired');
    updater.stop();
  }
});

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

test('account details retain shared reads without updating another service after a save', async () => {
  let nodes={}, details, mounted=0;
  const q=queries(), save=deferred();
  const state={accounts:[{id:'first',name:'First'},{id:'second',name:'Second'}]};
  const services={open() { details.stop();nodes={account_usage:{innerHTML:''},account_alias:{value:'New alias'},modal_status:{textContent:''}}; },mount() {mounted++;}};
  details=feature('account-details.js','createAccountDetails')({...q,getState:() => state,$:id => nodes[id],tr,esc,services,
    serviceUsage:{render:data => data.groups[0].model},persistState:() => save.promise,uiIcon:x => x,renderInfo:()=>''});
  const first=details.open('first'), second=details.open('second');
  const loading=nodes.account_usage.innerHTML;
  assert.equal(q.calls[0].options.signal.aborted,false);
  q.calls[0].resolve({groups:[{model:'stale'}]}); await first; assert.equal(nodes.account_usage.innerHTML,loading);
  q.calls[1].resolve({groups:[{model:'fresh'}]}); await second; assert.equal(nodes.account_usage.innerHTML,'fresh');
  const alias=details.saveAlias({dataset:{id:'second'}});
  details.stop(); nodes={}; save.resolve(); await alias;
  assert.equal(mounted,0,'saving a departed account must not reopen its tabs');
});

test('account removal finishes before catalog sync and never closes a replacement dialog', async () => {
  let state={accounts:[{id:'first',name:'First'},{id:'second',name:'Second'}]}, nodes={}, details, closed=0, synced=0;
  const q=queries(), catalog=deferred(), notices=[];
  const services={open(){details.stop();nodes={account_usage:{innerHTML:''}};}};
  details=feature('account-details.js','createAccountDetails')({...q,getState:()=>state,$:id=>nodes[id],tr,esc,services,
    serviceUsage:{render:()=>''},uiIcon:()=>'',renderInfo:()=>'',
    confirmRemoval:()=>true,notice:text=>notices.push(text),onRemoved:value=>{state=value;},
    closeModal(){closed++;details.stop();},syncSavedCatalog(){synced++;return catalog.promise;}});
  const opening=details.open('first');q.calls[0].resolve({});await opening;
  const button={disabled:false,textContent:'Remove',isConnected:true};
  const removing=details.remove('first',button);
  assert.equal(button.disabled,true);assert.equal(button.textContent,'Removing…');
  await details.remove('first',button);
  assert.equal(q.calls.filter(call=>call.options.method==='DELETE').length,1);
  const replacement=details.open('second');q.calls[2].resolve({});await replacement;
  q.calls[1].resolve({});await tick();
  q.calls[3].resolve({accounts:[{id:'second',name:'Second'}]});await removing;
  assert.equal(synced,1);assert.equal(closed,0);assert.deepEqual(state.accounts,[{id:'second',name:'Second'}]);
  assert.equal(button.disabled,false);assert.equal(notices[0],'Account removed');
  catalog.resolve();
});

test('service-local model manager cancels replaced reads and ignores stale or closed results', async () => {
  let nodes={}, manager;
  const q=queries();
  manager=feature('model-manager.js','createProviderModelManager')({...q,getState:()=>({providers:[{id:'api'}],models:[]}),$:id=>nodes[id],tr,esc,
    openModal(){manager.stop();nodes=Object.fromEntries(['shadow_model_manager','shadow_model_search','shadow_model_count','shadow_managed_models','modal_status'].map(id=>[id,{value:'',textContent:'',innerHTML:''}]));},
    modelSort:()=>0,compactContext:String,capabilitySummary:()=>'',uiIcon:()=>'<svg/>',isPreview:()=>false});
  const first=manager.open('api'),second=manager.open('api');
  assert.equal(q.calls[0].options.signal.aborted,true);
  q.calls[1].resolve({models:[{upstream_id:'fresh',display_name:'Fresh catalog'}]});await second;
  const visible=nodes.shadow_managed_models.innerHTML;assert.match(visible,/Fresh catalog/);
  q.calls[0].resolve({models:[{upstream_id:'stale',display_name:'Stale catalog'}]});await first;
  assert.equal(nodes.shadow_managed_models.innerHTML,visible);
  const closing=manager.open('api');manager.stop();nodes={};
  assert.equal(q.calls[2].options.signal.aborted,true);
  q.calls[2].reject(new Error('transport ignored abort'));await closing;
});

test('model editor cancels metadata when routes change and discards replies to a closed draft', async () => {
  let nodes={}, editor, notices=[];
  const q=queries(), document={querySelectorAll:() => []};
  editor=feature('model-editor.js','createModelEditor',{document})({...q,getState:() => ({providers:[{id:'api'}],models:[]}),$:id => nodes[id],tr,esc,
    help:()=>'',contextHelp:feature('context-help.js','createContextHelp')({getState:()=>({}),tr}),
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

test('quota trend owns filters, display projection, failures and cancelled window reads', async () => {
  let nodes={}, trend, after=0;
  const q=queries(), timers=new Set(), periods={...formats,periodPickers:{},periodPresetRange:() => ({start:100,end:500}),timeAxisTicks:()=>[]};
  const get=id=>nodes[id];
  periods.setPeriod=(_id,range,options)=>{Object.assign(periods.periodPickers.quota_history,range);return periods.periodPickers.quota_history.onChange(range,options);};
  periods.refreshPeriod=()=>periods.periodPickers.quota_history.onChange(periods.periodPickers.quota_history,{user:false});
  periods.registerPeriodPicker=(id,preset,onChange)=>{periods.periodPickers[id]={start:100,end:500,preset,onChange};return onChange(periods.periodPickers[id],{user:true});};
  const display=feature('quota-display.js','createQuotaDisplay')({read:()=> 'remaining',save(){}});
  trend=feature('quota-history.js','createQuotaHistory',{
    createReportQuery:feature('report-query.js','createReportQuery'),setInterval:fn=>{timers.add(fn);return fn;},clearInterval:fn=>timers.delete(fn),
  })({...q,$:get,tr,esc,quotaDisplay:display,periods,afterRead:()=>{after++;},compactWindowText:String,localTimeText:String,utcOffsetLabel:()=> 'UTC',subscriptionPlanKey:()=>'',subscriptionPlanLabel:String,subscriptionPlanColor:()=>'',
    openModal(){trend.stop();nodes=Object.fromEntries(['modal_status','modal_body','quota_history_content','quota_history_controls','quota_history_plan','quota_history_legend','quota_history_plot','quota_history_range','quota_history_back','quota_history_loading'].map(id=>[id,{innerHTML:'',textContent:'',querySelector:selector=>get(selector.slice(1)),querySelectorAll:()=>[]}]));}});
  const samples=[{observed_at:100,remaining_percent:76},{observed_at:200,remaining_percent:60}];
  const payload={start_at:100,end_at:500,series:[{window_minutes:300,points:samples}],failures:[{start_at:220,end_at:450,diagnostics:{code:'quota_transport_error'}}]};
  trend.open('first'); assert.equal(trend.busy(),true); assert.equal(timers.size,1);
  const old=q.calls[0]; trend.open('second','providers');
  assert.equal(old.options.signal.aborted,true); assert.equal(timers.size,1);
  assert.match(q.calls[1].path,/providers\/second\/quota-history/);
  old.resolve(payload); await tick(); assert.doesNotMatch(nodes.quota_history_plot.innerHTML,/<svg/);
  q.calls[1].resolve(payload); await tick(); assert.equal(trend.busy(),false);
  assert.match(nodes.quota_history_legend.innerHTML,/<strong>60%/);
  assert.match(nodes.quota_history_plot.innerHTML,/Quota connection failed/);
  display.set('used');trend.redraw();assert.match(nodes.quota_history_legend.innerHTML,/<strong>40%/);
  assert.equal(samples[0].remaining_percent,76);assert.equal(q.calls.length,2);
  const chart=nodes.quota_history_plot.innerHTML;
  trend.refresh();q.calls[2].reject(new Error('offline'));await tick();
  assert.equal(nodes.quota_history_plot.innerHTML,chart);assert.equal(nodes.modal_status.textContent,'offline');
  nodes.quota_history_content.onclick({target:{closest:()=>({dataset:{start:'100',end:'200'},id:''})}});
  assert.match(q.calls[3].path,/start=100&end=200/);
  trend.stop();assert.equal(q.calls[3].options.signal.aborted,true);assert.equal(timers.size,0);
  nodes.modal_status.textContent='other window';q.calls[3].resolve(payload);await tick();
  assert.equal(nodes.modal_status.textContent,'other window');assert.equal(after,2);
});

test('management events own split frames, connection cancellation and reconnect timers', async () => {
  const q=queries(), timers=new Map(), frames=[], encoder=new TextEncoder();
  let source, cancellations=0, disconnected=0;
  const connection=feature('management-events.js','createManagementEvents',{
    TextDecoder,setTimeout:fn=>{timers.set(fn,fn);return fn;},clearTimeout:id=>timers.delete(id),
  })({management:{fetch:q.api,isSignedIn:()=>true,clear(){}},visible:()=>true,onStart(){},onDisconnect:()=>disconnected++,onEvent:(type,data)=>frames.push([type,data])});
  connection.start();connection.start();assert.equal(q.calls.length,1);
  q.calls[0].resolve({ok:true,body:new ReadableStream({start(controller){source=controller;},cancel(){cancellations++;}})});await tick();
  assert.equal(connection.connected(),true);
  source.enqueue(encoder.encode('event: activity-updated\r\ndata: {"revision":'));
  source.enqueue(encoder.encode('1}\r'));
  source.enqueue(encoder.encode('\n\r\nevent: quota-updated\n\n'));await tick();
  assert.deepEqual(frames,[['activity-updated','{"revision":1}'],['quota-updated','']]);
  source.close();await tick();assert.equal(connection.connected(),false);assert.equal(timers.size,1);
  connection.stop();await tick();assert.equal(timers.size,0);assert.equal(q.calls.length,1);
  connection.start();const stale=q.calls[1];connection.stop();assert.equal(stale.options.signal.aborted,true);
  stale.resolve({ok:true,body:new ReadableStream({cancel(){cancellations++;}})});await tick();
  assert.equal(connection.connected(),false);assert.equal(frames.length,2);assert.ok(disconnected>0);assert.equal(cancellations,1);
  connection.start();q.calls[2].resolve({ok:true,body:new ReadableStream({cancel(){cancellations++;}})});await tick();
  connection.stop();await tick();assert.equal(cancellations,2);assert.equal(connection.connected(),false);assert.equal(timers.size,0);
});
