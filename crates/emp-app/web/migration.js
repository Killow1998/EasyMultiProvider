// Migration selection belongs to this dialog; credentials remain in the local helper.
function createMigration() {
  let view=null, previewTimer=null;
  const {services,selections,options,importedNameRows,warningHtml,previewRows,bodyHtml,mergeRules,preferenceRows}=createMigrationView(()=>state);
  const lan=createLanMigration({active,selections,options,openPreview});
  function stop() { lan.stop();clearTimeout(previewTimer);view?.controller.abort();view=null; }
  function active(candidate) { return view===candidate && $('shadow_migration')?.isConnected; }
  function updatePreview() {
    if(!view)return;
    $('shadow_migration').setAttribute('aria-busy',String(Boolean(view.busy)));
    if(view.mode==='choose')return;
    if(view.mode==='tunnel'){lan.update(view);return;}
    $('modal_submit').disabled=!selections(view).length || view.busy || (view.mode==='import'&&!view.previewError&&(!view.token||!view.previewReady||view.preferenceConflicts?.some(conflict=>Object.values(conflict.fields).some(field=>!field.resolved))));
    const decodeButton=document.querySelector('[data-migration-decode]');
    if(decodeButton)decodeButton.disabled=view.busy;
    const box=$('migration_preview_rows');if(!box)return;
    const expanded=new Set([...box.querySelectorAll('details[open]')].map(node=>node.dataset.migrationDetail));
    $('migration_selected_count').textContent=tr("transfers.services_selected", {count:selections(view).length});
    if(view.mode==='export')box.innerHTML=previewRows(view.rows.filter(row=>row.selected),view,true);
    else if(view.importConfig)box.innerHTML=preferenceRows(view)+previewRows(services(view.importConfig,true,{accounts:view.importConfig.accounts?.map(a=>a.id),providers:view.importConfig.providers?.map(p=>p.id)}),view,true);
    else box.innerHTML=previewRows(view.rows.filter(row=>row.selected),view);
    for(const node of box.querySelectorAll('details'))node.open=expanded.has(node.dataset.migrationDetail);
  }
  function back(candidate) {
    if(!active(candidate)||!candidate.parent||candidate.committing)return;
    clearTimeout(previewTimer);candidate.controller.abort();
    if(candidate.parent.view.mode==='choose')candidate.parent.view.drafts.set(candidate.mode,{view:candidate,body:$('shadow_migration')});
    view=candidate.parent.view;render(candidate.parent.body);
    if(view.mode==='choose')document.querySelector(`[data-migration-mode="${candidate.mode}"]`).focus({preventScroll:true});
  }
  function render(savedBody=null) {
    const candidate=view, importing=candidate.mode==='import';
    lan.stop();
    const submitLabel=candidate.previewError?tr("transfers.retry_preview"):candidate.demo?tr("transfers.finish_demo"):importing?tr("transfers.import"):tr("core.download_emp");
    openModal(tr("data_migration"),savedBody?'':bodyHtml(candidate),submitLabel,
      ['choose','tunnel'].includes(candidate.mode)||importing&&!candidate.token?null:()=>candidate.previewError?schedulePreview(candidate):submit(candidate),null,{back:candidate.parent?()=>back(candidate):null});
    if(savedBody)$('modal_body').append(savedBody);
    const title=candidate.mode==='choose'?'data_migration':candidate.mode==='tunnel'?'lan.tunnel':'transfers.'+candidate.mode;
    $('modal_title').innerHTML=`<span>${esc(tr(title))}</span>${shadowHelp(mergeRules())}`;
    const help=$('modal_title').querySelector('.shadow-help');help.dataset.migrationRulesHelp='';help.setAttribute('aria-label',tr("transfers.import_merge_rules"));
    const node=$('shadow_migration');
    node.onclick=event=>{
      if(candidate.committing)return;
      const mode=event.target.closest('[data-migration-mode]')?.dataset.migrationMode;
      if(mode&&candidate.mode==='choose'){
        const saved=candidate.drafts.get(mode);
        view=saved?.view||{mode,transport:mode==='tunnel'?'lan':undefined,rows:mode==='import'?[]:services(state),includeContext:true,includeRename:true,parent:{view:candidate,body:node}};
        view.controller=new AbortController();view.busy=false;render(saved?.body);
        $('modal_back').focus({preventScroll:true});return;
      }
      if(event.target.closest('[data-migration-decode]'))void decode(candidate);
      if(event.target.closest('[data-migration-demo]'))void demo(candidate);
      const all=event.target.closest('[data-migration-all]');if(all){candidate.rows.forEach(row=>row.selected=row.exportable!==false&&all.dataset.migrationAll==='true');render();schedulePreview(candidate);}
    };
    node.oninput=event=>{
      if(candidate.committing)return;
      const input=event.target;
      if(input.dataset.migrationPreferenceSource!==undefined){
        candidate.preferenceChoices ||= {};candidate.preferenceChoices[input.dataset.migrationPreferenceSource] ||= {};
        candidate.preferenceChoices[input.dataset.migrationPreferenceSource][input.dataset.migrationPreferenceKind]=input.value;
        schedulePreview(candidate);return;
      }
      if(input.dataset.migrationName!==undefined){
        candidate.nameEdits ||= new Map();candidate.nameEdits.set(input.dataset.migrationName,input.value);
        const rows=services(candidate.importConfig,true);
        for(const warning of node.querySelectorAll('[data-migration-warning]')){
          const row=rows.find(item=>item.kind+':'+item.id===warning.dataset.migrationWarning);
          if(row)warning.innerHTML=warningHtml(row,candidate,rows);
        }
        return;
      }
      if(input.dataset.migrationSelect!==undefined)candidate.rows[Number(input.dataset.migrationSelect)].selected=input.checked;
      if(input.dataset.migrationModelSelect!==undefined){const [ri,mi]=input.dataset.migrationModelSelect.split(':').map(Number);candidate.rows[ri].models[mi].selected=input.checked;}
      if(input.id==='migration_include_context')candidate.includeContext=input.checked;
      if(input.id==='migration_include_rename')candidate.includeRename=input.checked;
      updatePreview();schedulePreview(candidate);
    };
    updatePreview();
    $('modal_status').textContent=candidate.previewError || '';
    if(candidate.mode==='tunnel')lan.mount(candidate);
  }
  function openPreview(candidate,result,isDemo=false) {
    const config={...result.payload.config,model_previews:result.payload.model_previews};
    view={mode:'import',transport:candidate.transport,rows:services(config,true,{accounts:result.credential_accounts,providers:result.credential_providers}),
      includeContext:candidate.includeContext,includeRename:candidate.includeRename,controller:new AbortController(),
      token:result.token,demo:isDemo,parent:{view:candidate,body:$('shadow_migration')}};
    render();schedulePreview(view);
  }
  function schedulePreview(candidate) {
    if(candidate.mode!=='import'||!candidate.token||candidate.committing)return;
    const failed=candidate.previewError;candidate.previewError='';
    candidate.busy=true;candidate.previewReady=false;updatePreview();
    if(failed)render();
    clearTimeout(previewTimer);candidate.controller.abort();candidate.controller=new AbortController();
    const controller=candidate.controller;
    $('modal_status').textContent=tr("transfers.updating_preview");
    previewTimer=setTimeout(async()=>{
      if(!active(candidate))return;
      let timedOut=false;
      const deadline=setTimeout(()=>{timedOut=true;controller.abort();},30000);
      try{
        const result=await api('/api/migration/preview',{method:'POST',body:JSON.stringify({token:candidate.token,current_config:migrationScrub(state),selections:selections(candidate),...options(candidate)}),signal:controller.signal});
        if(!active(candidate)||controller.signal.aborted)return;
        candidate.importConfig=result.config;candidate.warnings=result.warnings || [];candidate.preferenceConflicts=result.preference_conflicts || [];candidate.previewReady=true;$('modal_status').textContent='';
        candidate.preferenceChoices=Object.fromEntries(candidate.preferenceConflicts.map(conflict=>[conflict.source_id,Object.fromEntries(Object.keys(conflict.fields).filter(kind=>candidate.preferenceChoices?.[conflict.source_id]?.[kind]).map(kind=>[kind,candidate.preferenceChoices[conflict.source_id][kind]]))]));
      }catch(error){
        if(active(candidate)&&candidate.controller===controller)candidate.previewError=timedOut?tr("transfers.preview_timed_out"):error.message;
      }finally{
        clearTimeout(deadline);
        if(active(candidate)&&candidate.controller===controller){candidate.busy=false;if(candidate.previewError)render();else updatePreview();}
      }
    },180);
  }
  async function demo(candidate) {
    if(!active(candidate)||candidate.busy)return;
    const controller=candidate.controller;
    candidate.busy=true;updatePreview();
    try {
      const result=await api('/api/migration/demo',{method:'POST',body:JSON.stringify({current_config:migrationScrub(state)}),signal:controller.signal});
      if(!active(candidate)||controller.signal.aborted)return;
      openPreview(candidate,result,true);
    } catch(error) {if(active(candidate)&&!controller.signal.aborted)$('modal_status').textContent=error.message;}
    finally {if(candidate.controller===controller){candidate.busy=false;if(active(candidate))updatePreview();}}
  }
  async function decode(candidate) {
    if(!active(candidate)||candidate.busy)return;
    const controller=candidate.controller;
    const file=$('shadow_migration_file').files[0], password=$('shadow_import_password').value;
    if(!file||!password){$('modal_status').textContent=tr("transfers.select_a_file_and_enter_its_password");return;}
    if(file.size>32*1024*1024){$('modal_status').textContent=tr("transfers.migration_packages_cannot_exceed_32_mib");return;}
    candidate.busy=true;updatePreview();$('modal_status').textContent=tr("transfers.opening_package");
    try{
      const bundle=bytesToBase64(new Uint8Array(await file.arrayBuffer()));
      if(controller.signal.aborted)return;
      const result=await api('/api/migration/decode',{method:'POST',body:JSON.stringify({password,bundle}),signal:controller.signal});
      if(!active(candidate)||controller.signal.aborted)return;
      openPreview(candidate,result);
    }catch(error){if(active(candidate)&&!controller.signal.aborted)$('modal_status').textContent=error.message;}
    finally{if(candidate.controller===controller){candidate.busy=false;if(active(candidate))updatePreview();}}
  }
  async function submit(candidate) {
    if(!active(candidate)||candidate.busy)return;
    const controller=candidate.controller;
    if(candidate.mode==='import'){
      if(!candidate.token)throw new Error(tr("transfers.select_a_migration_file_enter_its_password_then_click"));
      if(!candidate.previewReady||!candidate.importConfig)throw new Error(tr("transfers.the_service_list_is_not_ready_yet_please_wait"));
      if(candidate.preferenceConflicts?.some(conflict=>Object.values(conflict.fields).some(field=>!field.resolved)))throw new Error(tr("transfers.choose_model_visibility_and_context_settings_in_this_import"));
      if(candidate.demo){closeModal();notice(tr("transfers.demo_finished_no_services_saved"));return;}
      const selected=selections(candidate);
      for(const row of importedNameRows(candidate)){
        const source=selected.find(item=>item.kind===row.kind && item.id===row.record._shadow_source_id);
        if(source)source.name=candidate.nameEdits?.get(row.kind+':'+row.id)?.trim() || row.name;
      }
      candidate.committing=true;candidate.busy=true;
      const controls=[...$('shadow_migration').querySelectorAll('input,select,button'),$('modal_back')].map(control=>[control,control.disabled]);
      controls.forEach(([control])=>control.disabled=true);
      $('shadow_migration').setAttribute('aria-busy','true');
      try {
        // Preview cancellation must not discard a committed import's result.
        await api('/api/migration/import',{method:'POST',body:JSON.stringify({token:candidate.token,selections:selected,...options(candidate)})});
        state=await api('/api/config');fill();
        if(active(candidate))closeModal();
        notice(tr("transfers.selected_services_imported"));
      } finally {
        candidate.committing=false;candidate.busy=false;
        controls.forEach(([control,disabled])=>control.disabled=disabled);
        if(active(candidate))updatePreview();
      }
      return;
    }
    const password=$('shadow_export_password').value, confirmation=$('shadow_export_password_confirm').value;
    if(password!==confirmation)throw new Error(tr("transfers.passwords_do_not_match"));
    const passwordBytes=new TextEncoder().encode(password.trim()).length;
    if(passwordBytes>4096)throw new Error(tr("transfers.password_cannot_exceed_4096_utf_8_bytes"));
    if(passwordBytes<12)throw new Error(tr("transfers.password_requires_at_least_12_utf_8_bytes"));
    candidate.busy=true;updatePreview();
    const controls=[...$('shadow_migration').querySelectorAll('input,button')].map(control=>[control,control.disabled]);controls.forEach(([control])=>control.disabled=true);
    $('modal_status').textContent=tr("transfers.encrypting_selected_services");
    let result;try{const authorization=await api('/api/migration/export/confirm',{method:'POST',body:'{}',signal:controller.signal});result=await api('/api/migration/export',{method:'POST',body:JSON.stringify({confirmation:authorization.confirmation,password,selections:selections(candidate),...options(candidate)}),signal:controller.signal});}finally{controls.forEach(([control,disabled])=>control.disabled=disabled);if(candidate.controller===controller){candidate.busy=false;if(active(candidate))updatePreview();}}
    if(!active(candidate)||controller.signal.aborted)return;
    const bytes=Uint8Array.from(atob(result.bundle),c=>c.charCodeAt(0));const url=URL.createObjectURL(new Blob([bytes],{type:'application/octet-stream'}));
    const link=document.createElement('a');link.href=url;link.download='EMP.emp';link.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
    closeModal();notice(tr("transfers.selected_services_exported"));
  }
  return {open(){if(window.EMP_PREVIEW && shadowServicesDemoActive){notice(tr("transfers.end_the_service_demo_to_migrate_data"));return;}stop();view={mode:'choose',drafts:new Map(),controller:new AbortController()};render();},stop};
}

function migrationScrub(value) {
  if(Array.isArray(value))return value.map(migrationScrub);
  if(value&&typeof value==='object')return Object.fromEntries(Object.entries(value).filter(([key])=>!['api_key','api_key_file','auth_file','auth','imported_auth','provider_keys','access_token','refresh_token','id_token','password','authorization','cookie'].includes(key.toLowerCase())).map(([key,item])=>[key,migrationScrub(item)]));
  return value;
}
