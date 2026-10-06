// Synthetic fixtures only: no user auth, Codex helper, or real provider.
export function configFixture() {
 const now=Math.floor(Date.now()/1000),quota={plan_type:'plus',updated_at:now-180,rate_limits:{primary:{usedPercent:32,windowDurationMins:300,resetsAt:now+7200},secondary:{usedPercent:18,windowDurationMins:10080,resetsAt:now+200000}}};
 return {emp_version:'0.12.11',auto_enable_on_start:true,auto_review_fallback:true,native_account:{id:'@native',name:'Native',native:true,credential_set:true,credential_status:'unknown',enabled:true,hidden_models:[],quota},accounts:[{id:'work',prefix:'work',name:'工作账号',credential_set:true,credential_status:'unknown',enabled:true,hidden_models:[],quota:{...quota,plan_type:'pro'}},{id:'personal',prefix:'personal',name:'个人账号',credential_set:true,enabled:true,credential_status:'unknown',hidden_models:[],quota:null}],providers:[{id:'research',name:'研究服务',base_url:'https://api.anthropic.com',protocol:'anthropic_messages',auth_mode:'anthropic_api_key',api_key_set:true,enabled:true}],models:[{id:'research/claude-example',provider:'research',upstream_id:'claude-example',display_name:'Claude · 示例模型',enabled:true,context_window:200000,reasoning_levels:['high'],input_modalities:['text'],output_modalities:['text']}],subscription_models:[{id:'gpt-example',display_name:'GPT · 示例模型',context_window:200000,default_context_window:200000,max_context_window:200000,effective_context_window_percent:95}],catalog_families:[{id:'gpt-example',display_name:'GPT · 示例模型',default_display_name:'GPT · 示例模型',context_window:200000,routes:[{id:'gpt-example'},{id:'work/gpt-example'},{id:'personal/gpt-example'}]},{id:'research/claude-example',display_name:'研究服务 · Claude',default_display_name:'研究服务 · Claude',context_window:200000,routes:[{id:'research/claude-example'}]}],catalog_show_context:true,catalog_presentations:{},catalog_family_presentations:{},pricing_aliases:{},subscription_search:{enabled:true}};
}
export function integrationFixture(s='matched') {return {configuration:{state:['native','empty'].includes(s)?'native':s==='conflict'?'conflict':'emp_applied',stale:false,conflicts:s==='conflict'?['catalog_mismatch']:[]},runtime:{state:s==='pending'?'reload_required':s==='failed'?'verification_failed':s==='stale'?'not_checked':'catalog_loaded',target:['native','empty'].includes(s)?'native':'emp',confidence:s==='stale'?'stale':'live',catalog_verified:s==='matched',routing_verified:false,restoration_verified:false,routing_state:'not_observable',target_matches_saved_configuration:true,detail:'Synthetic observation. No real model requests.'},codex_compatibility:{installed:'fixture'}};}
export function createMock(scenario='matched') {
 let config=configFixture(),integration=integrationFixture(scenario),stopping=false,failCatalog=false;const calls=[];
 if(scenario==='empty'){config.native_account=null;config.accounts=[];config.providers=[];config.models=[];config.catalog_families=[];}
 if(scenario==='many')config.accounts=Array.from({length:13},(_,i)=>({...config.accounts[0],id:`account-${i+1}`,prefix:`account-${i+1}`,name:`研究账号 ${i+1}`}));
 async function handle(path,options={}) {
  const method=options.method||'GET',body=options.body?JSON.parse(options.body):null;calls.push({path,method,body});
  if((scenario==='offline'&&path!=='/api/session')||(stopping&&path==='/api/integration'))throw new TypeError('Failed to fetch');
  if(path==='/api/session')return {session:'synthetic-preview-session'};
  if(path==='/api/config'){if(method==='POST'){config=body;config.providers.forEach(p=>{if(p.api_key){p.api_key_set=true;delete p.api_key;}});}return structuredClone(config);}
  if(path==='/api/accounts')return {native_account:config.native_account,accounts:config.accounts};
  if(path==='/api/integration')return integration;
  if(path==='/api/integration/enable'){integration=integrationFixture('pending');return integration;}
  if(['/api/integration/reload','/api/integration/verify'].includes(path))return integration;
  if(path==='/api/integration/restore'){integration=integrationFixture('native');return integration;}
  if(path==='/api/catalog/refresh')return failCatalog?{__status:503,error:{message:'Synthetic catalog sync failure'}}:{ok:true};
  if(path==='/api/accounts/import'){if(!body.auth_json?.test_fixture)return {__status:400,error:{message:'Preview accepts only test_fixture auth. Do not import real credentials.'}};config.accounts=config.accounts.filter(a=>a.id!==body.id);config.accounts.push({...body,auth_json:undefined,credential_set:true,credential_status:'unknown',quota:null,hidden_models:body.hidden_models||[]});return {ok:true};}
  if(/\/api\/accounts\/[^/]+\/models$/.test(path))return {models:config.subscription_models};
  if(/\/api\/accounts\/[^/]+\/quota$/.test(path))return {account:config.accounts[0]};
  if(path==='/api/quit'){stopping=true;return {status:'stopping'};}
  if(path.startsWith('/api/updates'))return {state:'current',current_version:'0.12.11',supported:false};
  if(path==='/api/client-events')return {ok:true};
  if(path==='/api/diagnostics')return {health:{sample_count:0},models:[],records:[]};
  if(path==='/api/support-report')return {configuration:{exists:true,path:'synthetic preview only'},codex:{},network:{},accounts:{}};
  if(path.startsWith('/api/usage'))return {totals:{requests:0,priced_requests:0,input_reports:0,output_reports:0},groups:[],periods:[],issues:[],sources:[],history:{},pricing:{}};
  if(path.startsWith('/v1/'))throw new Error('Generation forbidden in frontend tests');
  return {__status:404,error:{message:`Unmocked route: ${path}`}};
 }
 return {scenario,calls,handle,get config(){return config;},setIntegration(v){integration=v;},setCatalogFailure(v){failCatalog=v;}};
}
