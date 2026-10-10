// Fixture-only service gallery. It never imports credentials or persists its config.
const shadowServiceDemoMode = new URLSearchParams(location.search).get('demo');
let shadowServicesDemoActive = ['services','service-errors'].includes(shadowServiceDemoMode);
let shadowServiceDemoDraft = null;

function shadowServiceDemoState(live) {
  if (shadowServiceDemoDraft) return structuredClone(shadowServiceDemoDraft);
  const now = Math.floor(Date.now()/1000);
  const quota = (used, weekly, unlimited = false) => ({plan_type:'plus', credits:{balance:2400}, rate_limits:{
    ...(unlimited ? {} : {primary:{used_percent:used,window_duration_mins:300,resets_at:now+4800}}),
    secondary:{used_percent:weekly,window_duration_mins:10080,resets_at:now+291600},
  }});
  const account = (id, native) => ({id,native,name:native?'Native':'Codex Demo',prefix:native?'':'demo',enabled:true,credential_set:true,hidden_models:[],quota:quota(15,32,native)});
  const providers = [
    {id:'demo-claude-local',name:'Claude Code',execution_backend:'claude_cli',auth_mode:'claude_login',protocol:'anthropic_messages'},
    {id:'demo-claude-cpa',name:'Claude CPA',execution_backend:'claude_cli',auth_mode:'api_key',protocol:'anthropic_messages',base_url:'https://cpa.example.invalid/v1'},
    ...Object.entries(officialProviders).filter(([key])=>key!=='chatgpt').map(([key,preset])=>({...preset,id:'demo-'+key})),
    {id:'demo-custom',name:'Custom Demo',execution_backend:'http',auth_mode:'api_key',protocol:'responses',base_url:'https://custom.example.invalid/v1'},
  ].map(provider=>({...provider,enabled:true,api_key_set:false}));
  const models=providers.map(provider=>({id:provider.id+'/demo-model',provider:provider.id,upstream_id:'demo-model',display_name:provider.name+' · Demo',enabled:true,context_window:131072,input_modalities:['text'],output_modalities:['text'],reasoning_efforts:[],capability_sources:{input_modalities:{source:'manual'},output_modalities:{source:'manual'}}}));
  shadowServiceDemoDraft={...live,native_account:account('@native',true),accounts:[account('demo-codex',false)],providers,models,claude_quota:quota(38,57),catalog_models:[],catalog_families:models.map(model=>({id:model.id,display_name:model.display_name,context_window:model.context_window,routes:[{id:model.id,source_id:model.provider,source_type:'external'}]}))};
  return structuredClone(shadowServiceDemoDraft);
}

function mountShadowServiceDemo() {
  let entry=document.getElementById('shadow_demo_button');
  if (!entry) {
    entry=document.createElement('button');entry.id='shadow_demo_button';entry.type='button';entry.className='secondary';
    document.querySelector('.page-header-controls [data-i18n="settings_menu"]').before(entry);
    entry.onclick=()=>openModal(tr('演示','Demos'),`<div class="choice-grid"><button type="button" class="choice" data-shadow-demo="services"><strong>${tr('全部服务调用中','All services running')}</strong><small>${tr('查看品牌颜色、运行光晕和额度样式','View brand colors, activity halos and quota styles')}</small></button><button type="button" class="choice" data-shadow-demo="service-errors"><strong>${tr('全部服务报错','All service errors')}</strong><small>${tr('查看红色提示与错误气泡','View red indicators and error bubbles')}</small></button></div>`,'',null);
  }
  entry.textContent=tr('演示','Demos');
  if (!shadowServicesDemoActive) return;
  let banner=document.getElementById('shadow_services_demo');
  if (!banner) {banner=document.createElement('div');banner.id='shadow_services_demo';banner.className='presentation-error-demo';document.body.append(banner);}
  banner.innerHTML=`<span>${shadowServiceDemoMode==='service-errors'?tr('服务演示 · 全部模拟报错','Service demo · All errors simulated'):tr('服务演示 · 全部模拟调用中','Service demo · All calls simulated')}</span><button type="button" class="secondary">${tr('结束演示','End demo')}</button>`;
  banner.querySelector('button').onclick=()=>{const url=new URL(location.href);url.searchParams.delete('demo');location.assign(url);};
}

function installShadowServiceDemo() {
  document.addEventListener('click',event=>{
    const choice=event.target.closest('[data-shadow-demo]');if(!choice)return;
    const url=new URL(location.href);url.searchParams.set('demo',choice.dataset.shadowDemo);location.assign(url);
  });
  if (!shadowServicesDemoActive) return;
  const summary=activitySummaryForIndicator;
  activitySummaryForIndicator=function(element){
    if(element.closest('#services,#models,#catalog_display_models'))return {inFlight:shadowServiceDemoMode==='services'?1:0,lastFinished:null,recent:false};
    return summary(element);
  };
  // The gallery uses a fixed snapshot; live quota/activity events cannot overwrite it.
  startQuotaEvents=function(){};
  requestQuotaSync=async function(){};
  document.addEventListener('click',event=>{
    const button=event.target.closest('#services [data-ui-action]');
    if(!button || !['activity-details','account-details','provider-details','account-refresh','account-quota-history','provider-quota-refresh','provider-quota-history'].includes(button.dataset.uiAction))return;
    event.preventDefault();event.stopImmediatePropagation();
    const id=button.dataset.activityId || button.dataset.id;
    const account=[state.native_account,...state.accounts].find(item=>item.id===id);
    const provider=state.providers.find(item=>item.id===id);
    const label=account?.native?'Native':account?.name || provider?.name || id;
    const model=provider?provider.id+'/demo-model':(account?.native?'':'demo/')+'codex-demo-model';
    openModal(tr('服务调用 · 演示','Service call · Demo'),`<p>${esc(label)} · ${shadowServiceDemoMode==='services'?tr('调用中','Running'):tr('连接报错','Connection error')}</p><p><code>${esc(model)}</code></p><p class="muted">${tr('这是界面演示，账号、额度和运行状态均为示例数据。','This is a UI demo. Accounts, quota and running states are sample data.')}</p>`,'',null);
  },true);
}
