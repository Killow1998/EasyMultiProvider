// Real source reads and candidate accounting; writes remain in this preview's draft.
let shadowSnapshot=null;
let readingShadowSource=false;
const demoBanner=document.createElement('p');
demoBanner.className='muted';demoBanner.id='shadow_source_banner';
document.querySelector('.page-header').after(demoBanner);
function describeShadowSource(){
  const time=shadowSnapshot ? new Date(shadowSnapshot.captured_at*1000).toLocaleString(language) : '…';
  demoBanner.textContent=tr('Shadow · 真实资料；账单快照：','Shadow · Real data; accounting snapshot: ')+time+tr('。统计刷新会重新读取，保存仅影响预览。','. Refresh statistics to reload it. Saves affect this preview only.');
}
async function readShadowSource(){shadowSnapshot=await api('/api/shadow/status');describeShadowSource();}
window.EMP_OPEN_SYNTHETIC=async action=>{
  openModal(tr('演示','Demos'),`<p>${tr('正在准备独立的合成演示…','Preparing the separate synthetic demo…')}</p>`,'',null);
  try{
    const result=await api('/api/shadow/demo',{method:'POST',body:JSON.stringify({scenario:action})}),url=new URL(result.url);
    if(['services','service-errors'].includes(action))url.searchParams.set('demo',action);
    else if(action==='import')url.searchParams.set('import','1');
    else if(action)url.searchParams.set('reviewAction',action);
    location.assign(url);
  }catch(error){$('modal_status').textContent=error.message;}
};
const realShadowFill=fill;
fill=function(){
  realShadowFill();
  $('emp_version').textContent+= ' · Shadow';
  $('integration_toggle').disabled=true;
  let button=$('shadow_demo_button');
  if(!button){
    button=document.createElement('button');button.id='shadow_demo_button';button.type='button';button.className='secondary';
    document.querySelector('.page-header-controls [data-i18n="settings_menu"]').before(button);
    button.onclick=()=>openModal(tr('演示','Demos'),`<p class="muted">${tr('以下入口使用独立的合成资料，不代表真实账号状态。','These entries use separate synthetic data, not the state of your accounts.')}</p><div class="choice-grid">${window.EMP_REVIEW_CHOICES()}</div>`,'',null);
  }
  button.textContent=tr('演示','Demos');describeShadowSource();syncPreviewReview();
  if(!shadowSnapshot&&!readingShadowSource){readingShadowSource=true;void readShadowSource().catch(error=>notice(error.message,true)).finally(()=>readingShadowSource=false);}
};
const realShadowRenderIntegration=renderIntegration;
renderIntegration=function(...args){realShadowRenderIntegration(...args);$('integration_toggle').disabled=true;};
confirmIntegrationAction=()=>{};
reportWebClientPhase=()=>{};
document.addEventListener('click',event=>{
  const choice=event.target.closest('[data-shadow-demo]');
  if(choice)window.EMP_REVIEW_ACTION(choice.dataset.shadowDemo);
});
document.addEventListener('click',event=>{
  const button=event.target.closest('#presentation_stats [data-report-action="refresh"]');
  if(!button)return;
  event.preventDefault();event.stopImmediatePropagation();
  if(button.disabled)return;
  const stats=$('presentation_stats');button.disabled=true;
  void api('/api/shadow/snapshot',{method:'POST',body:'{}'}).then(async()=>{
    await readShadowSource();
    if(stats.isConnected)await refreshPeriod('presentation_stats',false);
  }).catch(error=>notice(error.message,true)).finally(()=>button.disabled=false);
},true);
const realShadowSavedNotice=notice;
notice=function(message,error=false){realShadowSavedNotice(!error && /saved|已保存/i.test(message)?tr('影子预览：','Shadow preview: ')+message:message,error);};
const sourceFetch=window.fetch;
window.fetch=async function(path,options={}){
  const response=await sourceFetch(path,options);
  if(String(path)==='/api/usage/scan' && response.ok)void readShadowSource();
  return response;
};
