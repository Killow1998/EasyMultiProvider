// Synthetic import only: the server and package belong to this disposable demo.
window.EMP_PREVIEW=true;

managementEvents.stop();managementEvents.start=()=>{};
requestQuotaSync=async()=>{};
const fixtureBundle=__FIXTURE_BUNDLE__,fixturePassword=__FIXTURE_PASSWORD__;
const demoBanner=document.createElement('p');
demoBanner.className='muted';demoBanner.id='demo_import_banner';
demoBanner.textContent='影子演示 · 合成账号与迁移包，保存仅影响临时演示资料 / Shadow demo · Synthetic account and package; saves affect only disposable demo data';
document.querySelector('.page-header').after(demoBanner);
window.EMP_IMPORT_DEMO=()=>{
  if(shadowServicesDemoActive){const url=new URL(location.href);url.searchParams.delete("demo");url.searchParams.set("import","1");location.assign(url);return;}
  document.getElementById('shadow_migration_open').click();
  const transfer=new DataTransfer();
  transfer.items.add(new File([Uint8Array.from(atob(fixtureBundle),c=>c.charCodeAt(0))],'account-example.emp'));
  const file=document.getElementById('shadow_migration_file');file.files=transfer.files;
  const password=document.getElementById('shadow_import_password');password.value=fixturePassword;
  file.dispatchEvent(new Event('input',{bubbles:true}));password.dispatchEvent(new Event('input',{bubbles:true}));
};
const protect=()=>{
  document.getElementById('integration_toggle').disabled=true;
  document.querySelector('[data-i18n="quit_emp"]').hidden=true;
};
const originalFill=fill;fill=function(){
  if(shadowServicesDemoActive)state=shadowServiceDemoState(state);
  originalFill();protect();mountShadowServiceDemo();
  if(new URLSearchParams(location.search).has('import')){
    const url=new URL(location.href);url.searchParams.delete('import');history.replaceState(null,'',url);
    window.EMP_IMPORT_DEMO();
  }
};protect();installShadowServiceDemo();mountShadowServiceDemo();
const previewFetch=window.fetch.bind(window);
window.fetch=(path,options={})=>{
  if(shadowServicesDemoActive&&(options.method||'GET')!=='GET'&&String(path)!=='/api/session')
    return Promise.resolve(new Response(JSON.stringify({error:{message:'服务状态演示仅供预览 / Service status demo is read-only'}}),{status:403,headers:{'Content-Type':'application/json'}}));
  return previewFetch(path,options);
};
const originalRenderIntegration=renderIntegration;renderIntegration=function(...args){const result=originalRenderIntegration(...args);protect();return result;};
document.addEventListener('click',event=>{
  if(event.target.closest('[data-ui-action="account-refresh"],#refresh_all')){
    event.preventDefault();event.stopImmediatePropagation();notice('演示不调用真实额度 / No live quota calls in this demo');
  }
},true);
