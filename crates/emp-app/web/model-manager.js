// Service-local model operations; preview isolation is supplied by its transport adapter.
function createProviderModelManager({getState,cloneState,$,tr,esc,api,persistState,openModal,notice,modelSort,compactContext,capabilitySummary,uiIcon,testModel,createEditor,isPreview}) {
  let active=null, editor=null;
  function stop(){active?.controller.abort();active=null;editor?.stop();editor=null;}
  function current(view){return active===view && $('shadow_model_manager')===view.node;}
  function rows(view){
    const configured=(getState().models||[]).filter(model=>model.provider===view.id);
    const combined=new Map(view.models.map(model=>[model.upstream_id,model]));
    for(const model of configured)combined.set(model.upstream_id,{...combined.get(model.upstream_id),...model});
    return [...combined.values()].map(model=>({...model,enabled:configured.some(item=>item.upstream_id===model.upstream_id&&item.enabled!==false)}))
      .sort((a,b)=>Number(b.enabled)-Number(a.enabled)||modelSort(a,b));
  }
  function button(action,model,glyph,label,pressed=''){
    return `<button type="button" class="secondary shadow-model-action" data-manager-action="${action}" data-upstream="${esc(model.upstream_id)}" ${['eye','eye-off'].includes(glyph)?'':`data-icon="${glyph}"`} aria-label="${esc(label)}" title="${esc(label)}" ${pressed}>${['eye','eye-off'].includes(glyph)?uiIcon(glyph):''}</button>`;
  }
  function render(view){
    if(!current(view))return;
    const query=view.query.toLowerCase().trim(), all=rows(view), visible=all.filter(model=>((model.display_name||'')+' '+model.upstream_id).toLowerCase().includes(query));
    $('shadow_model_count').textContent=tr(`显示 ${all.filter(model=>model.enabled).length} / ${all.length} · 搜索结果 ${visible.length}`,`${all.filter(model=>model.enabled).length} / ${all.length} visible · ${visible.length} matches`);
    $('shadow_managed_models').innerHTML=visible.map(model=>`<div class="shadow-managed-model ${model.enabled?'':'is-hidden'}" data-managed-model="${esc(model.upstream_id)}" data-visible="${model.enabled}">
      <div><strong>${esc(model.display_name||model.upstream_id)}</strong><small>${esc(model.upstream_id)}${model.context_window>0?' · '+tr('上下文 ','Context ')+compactContext(model.context_window):''}${capabilitySummary(model)?' · '+capabilitySummary(model):''}</small></div>
      <div class="shadow-model-actions">${button('toggle',model,model.enabled?'eye':'eye-off',model.enabled?tr('隐藏模型','Hide model'):tr('显示模型','Show model'),`aria-pressed="${model.enabled}"`)}${button('edit',model,'edit',tr('编辑模型','Edit model'))}${button('test',model,'test',tr('测试模型','Test model'))}</div></div>`).join('')||`<p class="muted">${tr('没有匹配的模型；可读取列表或手动添加。','No matching models; read the list or add one manually.')}</p>`;
  }
  async function open(id,query='',navigation={}){
    const provider=getState().providers.find(item=>item.id===id);if(!provider)return;
    const view={id,query,navigation,models:[],controller:new AbortController()};
    openModal(tr('模型管理','Manage models')+' · '+(provider.name||id),`<div id="shadow_model_manager">
      <p class="section-help">${tr('显示或隐藏会立即保存到 Codex 模型列表。测试会向此服务发送请求，可能消耗额度或产生费用。','Showing or hiding a model saves immediately to the Codex model list. Testing sends a request to this service and may use quota or incur charges.')}</p>
      <div class="toolbar"><input type="search" id="shadow_model_search" placeholder="${tr('搜索模型名称或 ID','Search model name or ID')}" value="${esc(query)}"><button type="button" class="secondary" data-manager-action="refresh" data-icon="refresh">${tr('更新模型列表','Update model list')}</button><button type="button" class="secondary" data-manager-action="add" data-icon="plus">${tr('手动添加','Add manually')}</button></div>
      <p id="shadow_model_count" class="muted" role="status"></p><div id="shadow_managed_models">${tr('正在读取模型列表…','Loading model list…')}</div></div>`,'',null,null,{...navigation,reopen:()=>open(id,view.query,navigation)});
    active=view;view.node=$('shadow_model_manager');
    $('shadow_model_search').oninput=event=>{view.query=event.target.value;render(view);};
    view.node.onclick=async event=>{
      const control=event.target.closest('[data-manager-action]');if(!control||!current(view)||control.disabled)return;
      const action=control.dataset.managerAction;
      if(action==='refresh'){control.disabled=true;try{await api('/api/providers/discover',{method:'POST',body:JSON.stringify({provider:id,cache_refresh:true}),signal:view.controller.signal});return await load(view);}catch(error){if(current(view))$('modal_status').textContent=error.message;}finally{if(control.isConnected)control.disabled=false;}return;}
      if(action==='add')return edit(view,null);
      const model=rows(view).find(item=>item.upstream_id===control.dataset.upstream);if(!model)return;
      if(action==='test'){if(isPreview()){notice(tr('影子页不发送真实模型请求；正式版可在此测试。','The shadow page does not send real model requests; test here after promotion.'));return;}const route=(getState().models||[]).find(item=>item.provider===id&&item.upstream_id===model.upstream_id);if(!route){notice(tr('先添加模型，再测试连接。','Add the model before testing its connection.'),true);return;}return testModel(route.id);}
      if(action==='edit')return edit(view,model);
      control.disabled=true;
      try{
        const candidate=cloneState();let saved=candidate.models.find(item=>item.provider===id&&item.upstream_id===model.upstream_id);
        if(saved)saved.enabled=!model.enabled;
        else candidate.models.push({...model,id:id+'/'+model.upstream_id,provider:id,enabled:true});
        await persistState('',candidate);render(view);
      }catch(error){if(current(view))$('modal_status').textContent=error.message;}
      finally{if(control.isConnected)control.disabled=false;}
    };
    await load(view);
  }
  async function load(view,control){
    if(view.loading)return;view.loading=true;if(control)control.disabled=true;
    try{
      const result=await api('/api/providers/'+encodeURIComponent(view.id)+'/models',{signal:view.controller.signal});
      if(current(view)){view.models=result.models||[];$('modal_status').textContent='';render(view);}
    }catch(error){if(current(view)&&!view.controller.signal.aborted){$('modal_status').textContent=error.message;render(view);}}
    finally{view.loading=false;if(control?.isConnected)control.disabled=false;}
  }
  function edit(view,model){
    const id=view.id, query=view.query, route=model ? model.id||id+'/'+model.upstream_id : '';
    const existing=model&&(getState().models||[]).some(item=>item.id===route);
    const draft=model&&!existing ? {...model,id:route,provider:id,enabled:false} : null;
    const back=()=>open(id,query,view.navigation);
    const instance=createEditor({getState:()=>draft?{...getState(),models:[...getState().models,draft]}:getState(),closeModal:back});
    instance.open(route,id,{back});editor=instance;
    $('modal_model_provider').disabled=true;
  }
  return {open,stop};
}
