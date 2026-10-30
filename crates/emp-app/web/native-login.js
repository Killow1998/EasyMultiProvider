// Native replacement and subscription import own separate pending login jobs.
function createNativeLogin({onImported}) {
  let timer=null, view=null;
  function stop() {clearTimeout(timer);view=null;}
  function active(owner) {return view===owner && owner.node.isConnected;}
  function manage({back=null}={}) {
    stop();
    const mode=state.native_account?.auth_mode||'missing';
    const other=(state.accounts||[]).filter(account=>account.credential_set);
    openModal(tr('login.change_method'),`<p>${tr('login.mode_'+mode)}</p><div class="choice-grid">${[
      ['subscription','login.subscription','login.subscription_help'],
      ['api_key','login.api_key','login.api_key_help'],
      ['other','login.existing_other',other.length?'login.existing_other_help':'login.no_other']
    ].map(([method,title,description])=>`<button type="button" class="choice" data-native-method="${method}" ${method==='other'&&!other.length?'disabled':''}><strong>${tr(title)}</strong><small>${tr(description)}</small></button>`).join('')}</div>`,'',null,null,{back});
    const parent=()=>manage({back});
    $('modal_body').querySelector('[data-native-method=subscription]').onclick=()=>open({back:parent});
    $('modal_body').querySelector('[data-native-method=api_key]').onclick=()=>{
      openModal(tr('login.api_key'),`<p>${tr('login.api_key_help')}</p><label>${tr('login.api_key')}<input id="native_api_key" type="password" autocomplete="new-password" required></label><p class="muted">${tr('login.native_help')}</p>`,tr('login.apply_method'),()=>saveCredential('use-api-key',{api_key:$('native_api_key').value}),null,{back:parent});
      $('native_api_key').focus();
    };
    $('modal_body').querySelector('[data-native-method=other]').onclick=()=>{
      openModal(tr('login.existing_other'),`<p>${tr('login.existing_other_help')}</p><label>${tr('login.select_other')}<select id="native_other_account">${other.map(account=>`<option value="${esc(account.id)}">${esc(account.name||account.id)}</option>`).join('')}</select></label><p class="muted">${tr('login.native_help')}</p>`,tr('login.apply_method'),()=>saveCredential('use-account',{account_id:$('native_other_account').value}),null,{back:parent});
    };
  }
  async function saveCredential(operation,body) {
    try {
      const result=await api('/api/native/login/'+operation,{method:'POST',body:JSON.stringify(body)});
      state=await api('/api/config');fill();closeModal();notice(tr(result.integration_updated?'login.credential_saved':'login.credential_integration_failed'),!result.integration_updated);
    } catch(error) {
      const key='login.'+(error.payload?.error?.code||error.message);
      throw new Error(tr(key)===key?error.message:tr(key));
    }
  }
  function openLocalKey({back=()=>openAccountDetails('@native')}={}) {
    stop();
    openModal(tr('login.switch_local_key'),`<div id="native_local_key"><p>${tr('login.switch_explain')}</p><div class="choice-grid" role="radiogroup" aria-label="${esc(tr('login.switch_local_key'))}"><label class="choice native-key-choice"><input type="radio" name="native_key_action" value="keep" checked><span><strong>${tr('login.switch_keep')}</strong><small>${tr('login.switch_keep_help')}</small></span></label><label class="choice native-key-choice"><input type="radio" name="native_key_action" value="remove"><span><strong>${tr('login.switch_remove')}</strong><small>${tr('login.switch_remove_help')}</small></span></label></div><div class="native-key-actions"><button type="button" data-native-key-switch>${tr('login.switch_local_key')}</button></div></div>`,'',null,null,{back});
    const owner=$('native_local_key'), button=owner.querySelector('[data-native-key-switch]');
    button.onclick=async()=>{
      button.disabled=true;
      try {
        const result=await api('/api/native/login/use-local-key',{method:'POST',body:JSON.stringify({account_action:owner.querySelector('[name=native_key_action]:checked').value})});
        state=await api('/api/config');fill();closeModal();notice(tr(result.integration_updated?'login.switch_done':'login.switch_integration_failed'),!result.integration_updated);
      } catch(error) {
        if(owner.isConnected){const key='login.'+(error.payload?.error?.code||error.message);$('modal_status').textContent=tr(key)===key?error.message:tr(key);button.disabled=false;}
      }
    };
  }
  function open({importing=false,back=openServiceChooser}={}) {
    const mode=state.native_account?.auth_mode || 'missing';
    const nativeLabel=mode==='local_key'?'replace_local_key':mode==='api_key'?'replace_api_key':mode==='missing'?'use_native':'replace_native';
    const description=`<p>${tr('login.mode_'+mode)}</p><label>${tr('login.destination')}<select id="device_login_target"><option value="other" ${importing?'selected':''}>${tr('login.use_other')}</option><option value="native" ${!importing?'selected':''}>${tr('login.'+nativeLabel)}</option></select></label><p id="device_login_help" class="muted"></p><label id="device_account_fields">${tr('services.name')}<input id="device_account_name" maxlength="256" autocomplete="off" placeholder="${tr('services.for_example_work_account')}"></label>`;
    const methods=`<label>${tr('login.method')}<select id="native_login_method"><option value="device">${tr('login.device_sign_in')}</option><option value="browser">${tr('login.browser_sign_in')}</option></select></label><p class="muted" id="native_browser_help" hidden>${tr('login.browser_help')}</p>`;
    openModal(tr('login.subscription'),`<div id="native_login">${description}${methods}<div class="form-actions"><button type="button" data-native-login-start>${tr('login.sign_in')}</button>${!importing&&mode==='local_key'?`<button type="button" class="danger" data-native-key-remove>${tr('login.remove_key')}</button>`:''}</div><div id="native_login_progress" role="status" aria-live="polite"></div></div>`,'',null,null,{back});
    const owner={node:$('native_login')};view=owner;
    $('native_login_method').onchange=()=>{$('native_browser_help').hidden=$('native_login_method').value!=='browser';};
    function selectDestination() {
      owner.importing=$('device_login_target').value==='other';owner.signature=null;
      $('device_account_fields').hidden=!owner.importing;
      const remove=owner.node.querySelector('[data-native-key-remove]');if(remove)remove.hidden=owner.importing;
      $('device_login_help').textContent=tr(owner.importing?'login.other_help':'login.native_help');
      $('native_login_progress').replaceChildren();
      void run(owner,'status');
    }
    $('device_login_target').onchange=selectDestination;
    owner.node.querySelector('[data-native-login-start]').onclick=()=>run(owner,'start');
    owner.node.querySelector('[data-native-key-remove]')?.addEventListener('click',async()=>{
      if(!confirm(tr('login.remove_confirm')))return;
      try {await api('/api/native/login/cancel',{method:'POST',body:'{}'});await api('/api/integration/restore',{method:'POST',body:JSON.stringify({confirm_reload:true})});state=await api('/api/config');fill();closeModal();notice(tr('login.removed'));}
      catch(error){if(active(owner))$('modal_status').textContent=error.message;}
    });
    selectDestination();
    if(importing)$('device_account_name').focus();
  }
  async function run(owner,operation) {
    if(!active(owner))return;
    clearTimeout(timer);
    const request=owner.request=(owner.request||0)+1;
    if(operation!=='status')$('modal_status').textContent='';
    const button=owner.node.querySelector('[data-native-login-start]');button.disabled=true;
    const destination=owner.node.querySelector('#device_login_target');destination.disabled=true;
    const method=owner.node.querySelector('#native_login_method');method.disabled=true;
    const remove=owner.node.querySelector('[data-native-key-remove]');if(remove)remove.disabled=true;
    try {
      const name=owner.node.querySelector('#device_account_name');
      if(operation==='start'&&owner.importing&&!name.value.trim())throw new Error(tr('login.login_name_required'));
      const result=await api((owner.importing?'/api/accounts/login/':'/api/native/login/')+operation,{method:'POST',body:JSON.stringify(operation==='start'?{method:method.value,...(owner.importing?{name:name.value.trim()}:{})}:{})});
      if(!active(owner)||owner.request!==request)return;
      const pending=['starting','pending'].includes(result.status);
      button.disabled=pending;destination.disabled=pending;
      method.disabled=pending;
      if(remove)remove.disabled=pending;
      if(name)name.disabled=pending;
      if(pending)timer=setTimeout(()=>run(owner,'status'),1000);
      const signature=JSON.stringify([result.status,result.user_code,result.verification_url,result.code]);
      if(owner.signature===signature)return;
      owner.signature=signature;
      const box=$('native_login_progress');
      box.replaceChildren();
      if(result.status==='pending') {
        box.innerHTML=`${result.method==='browser'?`<p>${tr('login.browser_help')}</p>`:`<p>${tr('login.enter_code')}</p><p><strong class="device-code">${esc(result.user_code)}</strong> <button type="button" class="secondary" data-login-copy aria-label="${esc(tr('login.copy_code'))}">${uiIcon('copy')}</button></p>`}<p><a target="_blank" rel="noopener noreferrer" href="${esc(result.verification_url)}">${tr('login.open_page')}</a></p><p class="muted">${tr('login.expires',{time:new Date(result.expires_at*1000).toLocaleTimeString()})}</p><button type="button" class="secondary" data-login-cancel>${tr('login.cancel')}</button>`;
        box.querySelector('[data-login-copy]')?.addEventListener('click',()=>navigator.clipboard.writeText(result.user_code).catch(error=>notice(error.message,true)));
      } else if(result.status==='starting') {
        box.innerHTML=`<p>${tr('login.starting')}</p><button type="button" class="secondary" data-login-cancel>${tr('login.cancel')}</button>`;
      } else if(result.status==='failed') box.textContent=tr('login.'+result.code);
      else if(result.status!=='idle') box.textContent=tr('login.'+(result.status==='completed'?(owner.importing?'import_completed':result.integration_updated===false?'credential_integration_failed':'completed'):result.status));
      box.querySelector('[data-login-cancel]')?.addEventListener('click',()=>run(owner,'cancel'));
      if(result.status==='completed'){
        if(owner.importing)await onImported();
        else {state=await api('/api/config');fill();if(remove)remove.hidden=state.native_account?.auth_mode!=='local_key';}
      }
    } catch(error){if(active(owner)&&owner.request===request){const key='login.'+(error.payload?.error?.code||error.message);$('modal_status').textContent=tr(key)===key?error.message:tr(key);button.disabled=false;destination.disabled=false;method.disabled=false;if(remove)remove.disabled=false;}}
  }
  return {open,manage,openLocalKey,stop};
}
