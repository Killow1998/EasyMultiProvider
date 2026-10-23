// Native replacement and subscription import own separate pending login jobs.
function createNativeLogin({onImported}) {
  let timer=null, view=null;
  function stop() {clearTimeout(timer);view=null;}
  function active(owner) {return view===owner && owner.node.isConnected;}
  function open({importing=false,back=openServiceChooser}={}) {
    const mode=state.native_account?.auth_mode || 'missing';
    const description=importing?`<p class="muted">${tr('login.import_help')}</p><label>${tr('services.name')}<input id="device_account_name" maxlength="256" autocomplete="off" placeholder="${tr('services.for_example_work_account')}"></label>`:`<p>${tr('login.mode_'+mode)}</p><p class="muted">${tr('login.local_help')}</p>`;
    openModal(tr(importing?'login.device_sign_in':'login.title'),`<div id="native_login">${description}<div class="form-actions"><button type="button" data-native-login-start>${tr('login.sign_in')}</button>${!importing&&mode==='local_key'?`<button type="button" class="danger" data-native-key-remove>${tr('login.remove_key')}</button>`:''}</div><div id="native_login_progress" role="status" aria-live="polite"></div></div>`,'',null,null,{back});
    const owner={node:$('native_login'),importing};view=owner;
    owner.node.querySelector('[data-native-login-start]').onclick=()=>run(owner,'start');
    owner.node.querySelector('[data-native-key-remove]')?.addEventListener('click',async()=>{
      if(!confirm(tr('login.remove_confirm')))return;
      try {await api('/api/native/login/cancel',{method:'POST',body:'{}'});await api('/api/integration/restore',{method:'POST',body:JSON.stringify({confirm_reload:true})});state=await api('/api/config');fill();closeModal();notice(tr('login.removed'));}
      catch(error){if(active(owner))$('modal_status').textContent=error.message;}
    });
    void run(owner,'status');
    if(importing)$('device_account_name').focus();
  }
  async function run(owner,operation) {
    if(!active(owner))return;
    clearTimeout(timer);
    const request=owner.request=(owner.request||0)+1;
    if(operation!=='status')$('modal_status').textContent='';
    const button=owner.node.querySelector('[data-native-login-start]');button.disabled=true;
    try {
      const name=owner.node.querySelector('#device_account_name');
      if(operation==='start'&&owner.importing&&!name.value.trim())throw new Error(tr('login.login_name_required'));
      const result=await api((owner.importing?'/api/accounts/login/':'/api/native/login/')+operation,{method:'POST',body:JSON.stringify(operation==='start'&&owner.importing?{name:name.value.trim()}:{})});
      if(!active(owner)||owner.request!==request)return;
      const pending=['starting','pending'].includes(result.status);
      button.disabled=pending;
      if(name)name.disabled=pending;
      if(pending)timer=setTimeout(()=>run(owner,'status'),1000);
      const signature=JSON.stringify([result.status,result.user_code,result.code]);
      if(owner.signature===signature)return;
      owner.signature=signature;
      const box=$('native_login_progress');
      box.replaceChildren();
      if(result.status==='pending') {
        box.innerHTML=`<p>${tr('login.enter_code')}</p><p><strong class="device-code">${esc(result.user_code)}</strong> <button type="button" class="secondary" data-login-copy aria-label="${esc(tr('login.copy_code'))}">${uiIcon('copy')}</button></p><p><a target="_blank" rel="noopener noreferrer" href="${esc(result.verification_url)}">${tr('login.open_page')}</a></p><p class="muted">${tr('login.expires',{time:new Date(result.expires_at*1000).toLocaleTimeString()})}</p><button type="button" class="secondary" data-login-cancel>${tr('login.cancel')}</button>`;
        box.querySelector('[data-login-copy]').onclick=()=>navigator.clipboard.writeText(result.user_code).catch(error=>notice(error.message,true));
      } else if(result.status==='starting') {
        box.innerHTML=`<p>${tr('login.starting')}</p><button type="button" class="secondary" data-login-cancel>${tr('login.cancel')}</button>`;
      } else if(result.status==='failed') box.textContent=tr('login.'+result.code);
      else if(result.status!=='idle') box.textContent=tr('login.'+(owner.importing&&result.status==='completed'?'import_completed':result.status));
      box.querySelector('[data-login-cancel]')?.addEventListener('click',()=>run(owner,'cancel'));
      if(result.status==='completed'){
        if(owner.importing)await onImported();
        else {state=await api('/api/config');fill();}
      }
    } catch(error){if(active(owner)&&owner.request===request){const key='login.'+(error.payload?.error?.code||error.message);$('modal_status').textContent=tr(key)===key?error.message:tr(key);button.disabled=false;}}
  }
  return {open,stop};
}
