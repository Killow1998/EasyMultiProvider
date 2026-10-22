// Subscription preferences are independent of the page's other service editors.
function subscriptionPeers(config,id,native) {
  const owner=native?'当前 Codex 登录':id;
  return (config.accounts||[]).filter(account=>account.id!==id&&account.duplicate&&account.duplicate_of===owner);
}
function subscriptionPreferenceConflicts(config,account) {
  const hidden=new Set(account.hidden_models||[]), windows=account.model_context_windows||{};
  return subscriptionPeers(config,account.id,account.native).some(peer=>{
    const peerHidden=new Set(peer.hidden_models||[]), peerWindows=peer.model_context_windows||{};
    const models=new Set([...hidden,...peerHidden,...Object.keys(windows),...Object.keys(peerWindows)]);
    return [...models].some(model=>hidden.has(model)!==peerHidden.has(model)||String(windows[model]??'')!==String(peerWindows[model]??''));
  });
}
function subscriptionPreferenceModels(models,sources) {
  const result=[...models],known=new Set(models.map(model=>model.id));
  const saved=new Set(sources.flatMap(source=>[...source.hidden,...Object.keys(source.windows)]));
  for(const id of saved){
    if(!known.has(id)&&['visibility','context'].some(kind=>new Set(sources.map(source=>kind==='visibility'?source.hidden.has(id):String(source.windows[id]??''))).size>1))result.push({id,display_name:id,unavailable:true});
  }
  return result;
}
function applySubscriptionPreferences(candidate, source) {
  const {id,native,hidden,windows,syncModels}=source;
  if(native){
    candidate.native_hidden_models=[...hidden];candidate.native_model_context_windows={...windows};
  } else {
    candidate.accounts=candidate.accounts.map(account=>account.id===id?{...account,hidden_models:[...hidden],model_context_windows:{...windows}}:account);
  }
  const peers=new Set(subscriptionPeers(candidate,id,native).map(account=>account.id));
  candidate.accounts=(candidate.accounts||[]).map(account=>peers.has(account.id)?{...account,hidden_models:[...hidden],model_context_windows:{...windows}}:account);
  if(!syncModels.size)return candidate;
  for(const target of [...(candidate.native_account?[candidate.native_account]:[]),...(candidate.accounts||[])]){
    if(target.id===id)continue;
    const hiddenModels=new Set(target.native?candidate.native_hidden_models||[]:target.hidden_models||[]);
    const context={...(target.native?candidate.native_model_context_windows:target.model_context_windows)};
    for(const model of syncModels){
      if(hidden.has(model))hiddenModels.add(model);else hiddenModels.delete(model);
      if(windows[model]===undefined)delete context[model];else context[model]=windows[model];
    }
    if(target.native){candidate.native_hidden_models=[...hiddenModels];candidate.native_model_context_windows=context;}
    else {target.hidden_models=[...hiddenModels];target.model_context_windows=context;}
  }
  return candidate;
}

function createSubscriptionSettings({getState,cloneState,$,tr,esc,api,services,persistState,closeModal,notice,help,contextHelp,compactNumber,compactContext,canonicalDisplayName,modelSort,uiIcon}) {
  let editor=null;
  function stop(){editor?.controller.abort();editor=null;}
  function current(candidate){return editor===candidate&&!$('modal_backdrop').classList.contains('hidden');}
  function capture(candidate){
    for(const input of $('subscription_model_list').querySelectorAll('input[name="subscription_context"]')){
      if(input.value==='')delete candidate.windows[input.dataset.model];
      else candidate.windows[input.dataset.model]=input.value;
    }
  }
  function preview(input,index){
    const model=editor?.models[index];if(!model)return;
    const tokens=input.value===''?Number(model.default_context_window||0):Number(input.value);
    const usable=contextHelp.budget(model,tokens,{custom:input.value!==''})?.usable || 0;
    $('shadow_usable_'+index).textContent=tr("models.codex_usable")+compactNumber(usable);
    const button=$('subscription_model_list').querySelector(`[data-shadow-context-help="${index}"]`);
    button.dataset.shadowHelp=contextHelp.describe(model,tokens,{custom:input.value!==''});
    button.setAttribute('aria-label', tr('models.context_compression_details'));
  }
  function sync(index){
    const model=editor?.models[index];if(!model)return;
    const list=$('subscription_model_list'),input=list.querySelector(`[data-context-index="${index}"]`);
    if(!input.checkValidity()){input.reportValidity();return;}
    capture(editor);editor.syncModels.add(model.id);
    const state=getState(),targets=(state.accounts||[]).filter(a=>a.id!==editor.account.id).length+(!editor.native&&state.native_account?1:0);
    const button=list.querySelector(`[data-shadow-subscription-sync="${index}"]`);
    button.dataset.icon='check';button.title=button.ariaLabel=tr("models.sync_to_other_subscriptions_when_saving", {targets:targets});
  }
  function render(candidate){
    if(!current(candidate))return;
    const box=$('subscription_model_list');
    candidate.models=subscriptionPreferenceModels(candidate.models,candidate.sources).sort((a,b)=>Number(candidate.hidden.has(a.id))-Number(candidate.hidden.has(b.id))||modelSort(a,b));
    box.innerHTML=candidate.models.map((model,index)=>{
      const maximum=Number(model.max_context_window||0),value=candidate.windows[model.id]??'';
      const choices=['visibility','context'].filter(kind=>new Set(candidate.sources.map(source=>kind==='visibility'?source.hidden.has(model.id):String(source.windows[model.id]??''))).size>1&&!candidate.resolved.has(model.id+':'+kind));
      const conflicts=choices.length?`<div class="subscription-preference-conflicts">${choices.map(kind=>`<label>${tr("models.settings_for_this_account_differ_choose")} · ${kind==='visibility'?tr("models.visibility"):tr("reports.context")}<select data-preference-index="${index}" data-preference-kind="${kind}"><option value="" selected disabled>${tr("transfers.choose")}</option>${candidate.sources.map((source,si)=>`<option value="${si}">${esc(source.label)} · ${kind==='visibility'?(source.hidden.has(model.id)?tr("models.hidden"):tr("models.visible")):esc(source.windows[model.id]??tr("models.catalog_default"))}</option>`).join('')}</select></label>`).join('')}</div>`:'';
      return `<div class="model-option subscription-model-row shadow-context-row">
        <div class="subscription-model-identity"><button type="button" class="secondary shadow-model-action" data-shadow-subscription-visibility="${index}" aria-pressed="${!candidate.hidden.has(model.id)}" aria-label="${esc(candidate.hidden.has(model.id)?tr("models.show_model"):tr("models.hide_model"))}">${uiIcon(candidate.hidden.has(model.id)?'eye-off':'eye')}</button><span><strong>${esc(canonicalDisplayName(model.display_name||model.id))}</strong><span class="shadow-context-usable" id="shadow_usable_${index}"></span></span></div>
        <div><div class="shadow-context-control"><input type="number" name="subscription_context" data-model="${esc(model.id)}" min="1" step="1" ${maximum?`max="${maximum}"`:'disabled'} value="${esc(value)}" placeholder="${Number(model.default_context_window||0)}" data-context-index="${index}" aria-label="${esc(tr("models.maximum_context_window_tokens"))}" aria-describedby="subscription_context_preview_${index}">${help('').replace('data-shadow-help=',`data-shadow-context-help="${index}" data-shadow-help=`)}</div><small id="subscription_context_preview_${index}" class="muted">${model.unavailable?tr("models.model_missing_from_this_catalog_refresh_limits_before_changing"):tr("models.catalog_limit")+compactContext(maximum)+' · '+tr("models.default")+compactContext(model.default_context_window)}</small></div>
        <button type="button" class="secondary shadow-model-action shadow-subscription-sync" data-shadow-subscription-sync="${index}" data-icon="${candidate.syncModels.has(model.id)?'check':'sync'}" title="${esc(tr("models.sync_visibility_and_context_to_other_subscriptions"))}" aria-label="${esc(tr("models.sync_visibility_and_context_to_other_subscriptions"))}"></button>
        ${conflicts}</div>`;
    }).join('');
    box.onclick=event=>{
      const copy=event.target.closest('[data-shadow-subscription-sync]');if(copy){sync(Number(copy.dataset.shadowSubscriptionSync));return;}
      const button=event.target.closest('[data-shadow-subscription-visibility]');if(!button)return;
      capture(candidate);const model=candidate.models[Number(button.dataset.shadowSubscriptionVisibility)];
      if(candidate.hidden.has(model.id))candidate.hidden.delete(model.id);else candidate.hidden.add(model.id);render(candidate);
    };
    box.oninput=event=>{if(event.target.hasAttribute('data-context-index'))preview(event.target,Number(event.target.dataset.contextIndex));};
    box.onchange=event=>{
      const select=event.target.closest('[data-preference-kind]');if(!select)return;
      capture(candidate);const model=candidate.models[Number(select.dataset.preferenceIndex)],source=candidate.sources[Number(select.value)],kind=select.dataset.preferenceKind;
      if(kind==='visibility'){if(source.hidden.has(model.id))candidate.hidden.add(model.id);else candidate.hidden.delete(model.id);}
      else if(source.windows[model.id]===undefined)delete candidate.windows[model.id];else candidate.windows[model.id]=source.windows[model.id];
      candidate.resolved.add(model.id+':'+kind);render(candidate);
    };
    $('modal_submit').disabled=Boolean(box.querySelector('[data-preference-kind]'));
    box.querySelectorAll('input[name="subscription_context"]').forEach((input,index)=>preview(input,index));
  }
  async function load(candidate,refresh=false){
    capture(candidate);candidate.controller.abort();candidate.controller=new AbortController();const controller=candidate.controller;
    const button=$('subscription_model_refresh');if(refresh)button.disabled=true;
    try{
      const result=await api('/api/accounts/'+encodeURIComponent(candidate.account.id)+'/models'+(refresh?'/refresh':''),{...(refresh?{method:'POST',body:'{}'}:{}),signal:controller.signal});
      if(!current(candidate)||controller.signal.aborted)return;
      capture(candidate);candidate.models=result.models;render(candidate);
      if(refresh)$('modal_status').textContent=tr("models.model_limits_refreshed");
    }catch(error){if(current(candidate)&&!controller.signal.aborted)$('modal_status').textContent=error.message;}
    finally{if(current(candidate)&&refresh)button.disabled=false;}
  }
  function open(id,navigation={}){
    const state=getState(),account=state.native_account?.id===id?state.native_account:(state.accounts||[]).find(a=>a.id===id);if(!account)return;
    if(account.duplicate&&account.duplicate_of==='当前 Codex 登录')return notice(tr("models.adjust_model_visibility_from_the_native_account"),true);
    const candidate={account,native:account.native===true,models:state.subscription_models||[],hidden:new Set(account.hidden_models||[]),windows:{...(account.model_context_windows||{})},syncModels:new Set(),controller:new AbortController()};
    candidate.sources=[account,...subscriptionPeers(state,id,candidate.native)].map(source=>({label:source.native?'Native':source.name||tr("models.imported_settings"),hidden:new Set(source.hidden_models||[]),windows:source.model_context_windows||{}}));
    candidate.resolved=new Set();
    const body=`<p class="muted">${tr("models.leave_blank_for_the_default_context_rename_models_in")}</p>
      <div class="toolbar"><button id="subscription_model_refresh" type="button" class="secondary" data-icon="refresh">${tr("models.refresh_model_limits")}</button></div>
      <div class="subscription-list-head"><span>${tr("models.visible_in_codex_model")}</span><span>${tr("models.maximum_context_window_tokens")} ${help(tr("models.settings_cannot_exceed_the_online_catalog_limit_leave_blank"))}</span></div>
      <div id="subscription_model_list" class="model-selection"></div>`;
    services.open(id,'account','edit',body,tr("models.save_changes"),async()=>{
      if($('subscription_model_list').querySelector('[data-preference-kind]'))throw new Error(tr("models.choose_visibility_and_context_settings_for_this_account_first"));
      for(const input of $('subscription_model_list').querySelectorAll('input[name="subscription_context"]')){
        if(!input.checkValidity()){input.reportValidity();throw new Error(tr("models.context_must_be_a_positive_integer_within_the_catalog"));}
      }
      capture(candidate);const windows=Object.fromEntries(Object.entries(candidate.windows).map(([model,value])=>[model,Number(value)]));
      const config=applySubscriptionPreferences(cloneState(),{id,native:candidate.native,hidden:candidate.hidden,windows,syncModels:candidate.syncModels});
      await persistState(tr("models.account_saved"),config);if(current(candidate))closeModal();
    },navigation);
    editor=candidate;$('subscription_model_refresh').onclick=()=>load(candidate,true);render(candidate);void load(candidate);
  }
  return {open,stop};
}
