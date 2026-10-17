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
    $('shadow_model_count').textContent=tr("models.visible_matches", {count:all.filter(model=>model.enabled).length, count2:all.length, count3:visible.length});
    $('shadow_managed_models').innerHTML=visible.map(model=>`<div class="shadow-managed-model ${model.enabled?'':'is-hidden'}" data-managed-model="${esc(model.upstream_id)}" data-visible="${model.enabled}">
      <div><strong>${esc(model.display_name||model.upstream_id)}</strong><small>${esc(model.upstream_id)}${model.context_window>0?' · '+tr("models.context")+compactContext(model.context_window):''}${capabilitySummary(model)?' · '+capabilitySummary(model):''}</small></div>
      <div class="shadow-model-actions">${button('toggle',model,model.enabled?'eye':'eye-off',model.enabled?tr("models.hide_model"):tr("models.show_model"),`aria-pressed="${model.enabled}"`)}${button('edit',model,'edit',tr("models.edit_model"))}${button('test',model,'test',tr("models.test_model"))}</div></div>`).join('')||`<p class="muted">${tr("models.no_matching_models_read_the_list_or_add_one")}</p>`;
  }
  async function open(id,query='',navigation={}){
    const provider=getState().providers.find(item=>item.id===id);if(!provider)return;
    const view={id,query,navigation,models:[],controller:new AbortController()};
    openModal(tr("models.manage_models")+' · '+(provider.name||id),`<div id="shadow_model_manager">
      <div class="toolbar"><input type="search" id="shadow_model_search" placeholder="${tr("models.search_model_name_or_id")}" value="${esc(query)}"><button type="button" class="secondary" data-manager-action="refresh" data-icon="refresh">${tr("models.update_model_list")}</button><button type="button" class="secondary" data-manager-action="add" data-icon="plus">${tr("models.add_manually")}</button></div>
      <p id="shadow_model_count" class="muted" role="status"></p><div id="shadow_managed_models">${tr("models.loading_model_list")}</div></div>`,'',null,null,{...navigation,reopen:()=>open(id,view.query,navigation)});
    active=view;view.node=$('shadow_model_manager');
    $('shadow_model_search').oninput=event=>{view.query=event.target.value;render(view);};
    view.node.onclick=async event=>{
      const control=event.target.closest('[data-manager-action]');if(!control||!current(view)||control.disabled)return;
      const action=control.dataset.managerAction;
      if(action==='refresh'){control.disabled=true;try{await api('/api/providers/discover',{method:'POST',body:JSON.stringify({provider:id,cache_refresh:true}),signal:view.controller.signal});return await load(view);}catch(error){if(current(view))$('modal_status').textContent=error.message;}finally{if(control.isConnected)control.disabled=false;}return;}
      if(action==='add')return edit(view,null);
      const model=rows(view).find(item=>item.upstream_id===control.dataset.upstream);if(!model)return;
      if(action==='test'){if(isPreview()){notice(tr("models.the_shadow_page_does_not_send_real_model_requests"));return;}const route=(getState().models||[]).find(item=>item.provider===id&&item.upstream_id===model.upstream_id);if(!route){notice(tr("models.add_the_model_before_testing_its_connection"),true);return;}return testModel(route.id);}
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
