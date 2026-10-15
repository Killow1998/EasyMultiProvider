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

function createSubscriptionSettings({getState,cloneState,$,tr,esc,api,services,persistState,closeModal,notice,help,compactNumber,compactContext,canonicalDisplayName,modelSort,uiIcon}) {
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
    const percent=Number(model.effective_context_window_percent??95),usable=Math.floor(tokens*percent/100);
    $('shadow_usable_'+index).textContent=tr('Codex 可用 ','Codex usable ')+compactNumber(usable);
    const button=$('subscription_model_list').querySelector(`[data-shadow-context-help="${index}"]`);
    button.dataset.shadowHelp=tr(`当前${input.value===''?'使用目录默认值':'设置'} ${tokens.toLocaleString('en-US')} Tokens；目录比例 ${percent}%。\n${tokens.toLocaleString('en-US')} × ${percent}% = ${usable.toLocaleString('en-US')} 可用 Tokens。`,`Current ${input.value===''?'catalog default':'setting'}: ${tokens.toLocaleString('en-US')} tokens; catalog ratio ${percent}%.\n${tokens.toLocaleString('en-US')} × ${percent}% = ${usable.toLocaleString('en-US')} usable tokens.`);
  }
  function sync(index){
    const model=editor?.models[index];if(!model)return;
    const list=$('subscription_model_list'),input=list.querySelector(`[data-context-index="${index}"]`);
    if(!input.checkValidity()){input.reportValidity();return;}
    capture(editor);editor.syncModels.add(model.id);
    const state=getState(),targets=(state.accounts||[]).filter(a=>a.id!==editor.account.id).length+(!editor.native&&state.native_account?1:0);
    const button=list.querySelector(`[data-shadow-subscription-sync="${index}"]`);
    button.dataset.icon='check';button.title=button.ariaLabel=tr(`保存时同步到其他 ${targets} 个订阅`,`Sync to ${targets} other subscriptions when saving`);
  }
  function render(candidate){
    if(!current(candidate))return;
    const box=$('subscription_model_list');
    candidate.models=subscriptionPreferenceModels(candidate.models,candidate.sources).sort((a,b)=>Number(candidate.hidden.has(a.id))-Number(candidate.hidden.has(b.id))||modelSort(a,b));
    box.innerHTML=candidate.models.map((model,index)=>{
      const maximum=Number(model.max_context_window||0),value=candidate.windows[model.id]??'';
      const choices=['visibility','context'].filter(kind=>new Set(candidate.sources.map(source=>kind==='visibility'?source.hidden.has(model.id):String(source.windows[model.id]??''))).size>1&&!candidate.resolved.has(model.id+':'+kind));
      const conflicts=choices.length?`<div class="subscription-preference-conflicts">${choices.map(kind=>`<label>${tr('同一账号的设置不同，请选择','Settings for this account differ; choose')} · ${kind==='visibility'?tr('显示','Visibility'):tr('上下文','Context')}<select data-preference-index="${index}" data-preference-kind="${kind}"><option value="" selected disabled>${tr('请选择','Choose')}</option>${candidate.sources.map((source,si)=>`<option value="${si}">${esc(source.label)} · ${kind==='visibility'?(source.hidden.has(model.id)?tr('隐藏','Hidden'):tr('显示','Visible')):esc(source.windows[model.id]??tr('目录默认值','Catalog default'))}</option>`).join('')}</select></label>`).join('')}</div>`:'';
      return `<div class="model-option subscription-model-row shadow-context-row">
        <div class="subscription-model-identity"><button type="button" class="secondary shadow-model-action" data-shadow-subscription-visibility="${index}" aria-pressed="${!candidate.hidden.has(model.id)}" aria-label="${esc(candidate.hidden.has(model.id)?tr('显示模型','Show model'):tr('隐藏模型','Hide model'))}">${uiIcon(candidate.hidden.has(model.id)?'eye-off':'eye')}</button><span><strong>${esc(canonicalDisplayName(model.display_name||model.id))}</strong><span class="shadow-context-usable" id="shadow_usable_${index}"></span></span></div>
        <div><div class="shadow-context-control"><input type="number" name="subscription_context" data-model="${esc(model.id)}" min="1" step="1" ${maximum?`max="${maximum}"`:'disabled'} value="${esc(value)}" placeholder="${Number(model.default_context_window||0)}" data-context-index="${index}" aria-label="${esc(tr('最大上下文窗口（Tokens）','Maximum context window (tokens)'))}" aria-describedby="subscription_context_preview_${index}">${help('').replace('data-shadow-help=',`data-shadow-context-help="${index}" data-shadow-help=`)}</div><small id="subscription_context_preview_${index}" class="muted">${model.unavailable?tr('此模型不在当前目录中；上下文修改需要刷新上限','Model missing from this catalog; refresh limits before changing context'):tr('目录上限 ','Catalog limit ')+compactContext(maximum)+' · '+tr('默认 ','Default ')+compactContext(model.default_context_window)}</small></div>
        <button type="button" class="secondary shadow-model-action shadow-subscription-sync" data-shadow-subscription-sync="${index}" data-icon="${candidate.syncModels.has(model.id)?'check':'sync'}" title="${esc(tr('同步显示与上下文到其他订阅','Sync visibility and context to other subscriptions'))}" aria-label="${esc(tr('同步显示与上下文到其他订阅','Sync visibility and context to other subscriptions'))}"></button>
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
      if(refresh)$('modal_status').textContent=tr('模型上限已刷新','Model limits refreshed');
    }catch(error){if(current(candidate)&&!controller.signal.aborted)$('modal_status').textContent=error.message;}
    finally{if(current(candidate)&&refresh)button.disabled=false;}
  }
  function open(id,navigation={}){
    const state=getState(),account=state.native_account?.id===id?state.native_account:(state.accounts||[]).find(a=>a.id===id);if(!account)return;
    if(account.duplicate&&account.duplicate_of==='当前 Codex 登录')return notice(tr('模型显示请在原生账户中调整','Adjust model visibility from the Native account'),true);
    const candidate={account,native:account.native===true,models:state.subscription_models||[],hidden:new Set(account.hidden_models||[]),windows:{...(account.model_context_windows||{})},syncModels:new Set(),controller:new AbortController()};
    candidate.sources=[account,...subscriptionPeers(state,id,candidate.native)].map(source=>({label:source.native?'Native':source.name||tr('已导入设置','Imported settings'),hidden:new Set(source.hidden_models||[]),windows:source.model_context_windows||{}}));
    candidate.resolved=new Set();
    const body=`<p class="muted">${tr('留空使用默认上下文。名称在“模型显示”中设置。','Leave blank for the default context. Rename models in Model display.')}</p>
      <div class="toolbar"><button id="subscription_model_refresh" type="button" class="secondary" data-icon="refresh">${tr('刷新模型上限','Refresh model limits')}</button></div>
      <div class="subscription-list-head"><span>${tr('在 Codex /model 中显示','Visible in Codex /model')}</span><span>${tr('最大上下文窗口（Tokens）','Maximum context window (tokens)')} ${help(tr('设置不能超过在线目录上限；留空采用在线目录默认值。Codex 可用窗口按目录中的比例计算，系统提示词、历史、工具和输出都占用窗口。自动压缩阈值另算。','Settings cannot exceed the online catalog limit. Leave blank for the catalog default. Codex usable context follows the catalog percentage; instructions, history, tools and output use this window. The compaction threshold is separate.'))}</span></div>
      <div id="subscription_model_list" class="model-selection"></div>`;
    services.open(id,'account','edit',body,tr('保存修改','Save changes'),async()=>{
      if($('subscription_model_list').querySelector('[data-preference-kind]'))throw new Error(tr('请先选择同一账号的显示与上下文设置','Choose visibility and context settings for this account first'));
      for(const input of $('subscription_model_list').querySelectorAll('input[name="subscription_context"]')){
        if(!input.checkValidity()){input.reportValidity();throw new Error(tr('上下文必须为目录上限内的正整数','Context must be a positive integer within the catalog limit'));}
      }
      capture(candidate);const windows=Object.fromEntries(Object.entries(candidate.windows).map(([model,value])=>[model,Number(value)]));
      const config=applySubscriptionPreferences(cloneState(),{id,native:candidate.native,hidden:candidate.hidden,windows,syncModels:candidate.syncModels});
      await persistState(tr('账户信息已保存','Account saved'),config);if(current(candidate))closeModal();
    },navigation);
    editor=candidate;$('subscription_model_refresh').onclick=()=>load(candidate,true);render(candidate);void load(candidate);
  }
  return {open,stop};
}
