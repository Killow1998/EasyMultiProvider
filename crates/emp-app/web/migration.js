// Migration selection belongs to this dialog; credentials remain in the local helper.
function createMigration() {
  let view=null, previewTimer=null;
  const {services,selections,options,importedNameRows,warningHtml,previewRows,bodyHtml,mergeRules,preferenceRows}=createMigrationView(()=>state);
  function stop() { clearTimeout(previewTimer);view?.controller.abort();view=null; }
  function active(candidate) { return view===candidate && $('shadow_migration')?.isConnected; }
  function updatePreview() {
    if(!view)return;
    $('modal_submit').disabled=!selections(view).length || view.busy || (view.mode==='import'&&(!view.token||!view.previewReady||view.preferenceConflicts?.some(conflict=>Object.values(conflict.fields).some(field=>!field.resolved))));
    const file=$('shadow_migration_file'), password=$('shadow_import_password');
    const step=view.token?2:(file?.files.length&&password?.value?1:0);
    for(const node of document.querySelectorAll('[data-migration-step]')){
      if(Number(node.dataset.migrationStep)===step)node.setAttribute('aria-current','step');else node.removeAttribute('aria-current');
    }
    const decodeButton=document.querySelector('[data-migration-decode]');
    if(decodeButton)decodeButton.disabled=view.busy;
    const box=$('migration_preview_rows');if(!box)return;
    const expanded=new Set([...box.querySelectorAll('details[open]')].map(node=>node.dataset.migrationDetail));
    $('migration_selected_count').textContent=tr(`已选择 ${selections(view).length} 个服务`,`${selections(view).length} services selected`);
    if(view.mode==='export')box.innerHTML=previewRows(view.rows.filter(row=>row.selected),view,true);
    else if(view.importConfig)box.innerHTML=preferenceRows(view)+previewRows(services(view.importConfig,true,{accounts:view.importConfig.accounts?.map(a=>a.id),providers:view.importConfig.providers?.map(p=>p.id)}),view,true);
    else box.innerHTML=previewRows(view.rows.filter(row=>row.selected),view);
    for(const node of box.querySelectorAll('details'))node.open=expanded.has(node.dataset.migrationDetail);
  }
  function back(candidate) {
    if(!active(candidate)||!candidate.parent)return;
    clearTimeout(previewTimer);candidate.controller.abort();
    view=candidate.parent.view;render(candidate.parent.body);
  }
  function render(savedBody=null) {
    const candidate=view, importing=candidate.mode==='import';
    const submitLabel=candidate.demo?tr('结束演示','Finish demo'):importing?tr('导入','Import'):tr('下载 .emp','Download .emp');
    openModal(tr('数据迁移','Data migration'),savedBody?'':bodyHtml(candidate),submitLabel,
      importing&&!candidate.token?null:()=>submit(candidate),null,{back:candidate.parent?()=>back(candidate):null});
    if(savedBody)$('modal_body').append(savedBody);
    $('modal_title').innerHTML=`<span>${esc(tr('数据迁移','Data migration'))}</span>${shadowHelp(mergeRules())}`;
    const help=$('modal_title').querySelector('.shadow-help');help.dataset.migrationRulesHelp='';help.setAttribute('aria-label',tr('导入合并规则','Import merge rules'));
    const node=$('shadow_migration');
    node.onclick=event=>{
      const mode=event.target.closest('[data-migration-mode]')?.dataset.migrationMode;
      if(mode){clearTimeout(previewTimer);candidate.controller.abort();view={mode,rows:mode==='export'?services(state):[],includeContext:true,includeRename:true,controller:new AbortController()};render();return;}
      if(event.target.closest('[data-migration-decode]'))void decode(candidate);
      if(event.target.closest('[data-migration-demo]'))void demo(candidate);
      const all=event.target.closest('[data-migration-all]');if(all){candidate.rows.forEach(row=>row.selected=row.exportable!==false&&all.dataset.migrationAll==='true');render();schedulePreview(candidate);}
    };
    node.oninput=event=>{
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
  }
  function openPreview(candidate,result,isDemo=false) {
    const config={...result.payload.config,model_previews:result.payload.model_previews};
    view={mode:'import',rows:services(config,true,{accounts:result.credential_accounts,providers:result.credential_providers}),
      includeContext:candidate.includeContext,includeRename:candidate.includeRename,controller:new AbortController(),
      token:result.token,demo:isDemo,parent:{view:candidate,body:$('shadow_migration')}};
    render();schedulePreview(view);
  }
  function schedulePreview(candidate) {
    if(candidate.mode!=='import'||!candidate.token)return;
    candidate.busy=true;candidate.previewReady=false;updatePreview();
    clearTimeout(previewTimer);candidate.controller.abort();candidate.controller=new AbortController();
    const controller=candidate.controller;
    previewTimer=setTimeout(async()=>{
      if(!active(candidate))return;
      candidate.busy=true;updatePreview();$('modal_status').textContent=tr('正在更新预览…','Updating preview…');
      try{
        const result=await api('/api/migration/preview',{method:'POST',body:JSON.stringify({token:candidate.token,current_config:migrationScrub(state),selections:selections(candidate),...options(candidate)}),signal:controller.signal});
        if(!active(candidate)||controller.signal.aborted)return;
        candidate.importConfig=result.config;candidate.warnings=result.warnings || [];candidate.preferenceConflicts=result.preference_conflicts || [];candidate.previewReady=true;$('modal_status').textContent='';
        candidate.preferenceChoices=Object.fromEntries(candidate.preferenceConflicts.map(conflict=>[conflict.source_id,Object.fromEntries(Object.keys(conflict.fields).filter(kind=>candidate.preferenceChoices?.[conflict.source_id]?.[kind]).map(kind=>[kind,candidate.preferenceChoices[conflict.source_id][kind]]))]));
      }catch(error){if(active(candidate)&&!controller.signal.aborted)$('modal_status').textContent=error.message;}
      finally{if(active(candidate)&&!controller.signal.aborted){candidate.busy=false;updatePreview();}}
    },180);
  }
  async function demo(candidate) {
    if(!active(candidate)||candidate.busy)return;
    candidate.busy=true;updatePreview();
    try {
      const result=await api('/api/migration/demo',{method:'POST',body:JSON.stringify({current_config:migrationScrub(state)}),signal:candidate.controller.signal});
      if(!active(candidate))return;
      openPreview(candidate,result,true);
    } catch(error) {if(active(candidate))$('modal_status').textContent=error.message;}
    finally {candidate.busy=false;if(active(candidate))updatePreview();}
  }
  async function decode(candidate) {
    if(!active(candidate)||candidate.busy)return;
    const file=$('shadow_migration_file').files[0], password=$('shadow_import_password').value;
    if(!file||!password){$('modal_status').textContent=tr('请选择文件并填写密码','Select a file and enter its password');return;}
    if(file.size>32*1024*1024){$('modal_status').textContent=tr('迁移包不能超过 32 MiB','Migration packages cannot exceed 32 MiB');return;}
    candidate.busy=true;updatePreview();$('modal_status').textContent=tr('正在打开迁移包…','Opening package…');
    try{
      const bundle=bytesToBase64(new Uint8Array(await file.arrayBuffer()));
      const result=await api('/api/migration/decode',{method:'POST',body:JSON.stringify({password,bundle}),signal:candidate.controller.signal});
      if(!active(candidate))return;
      openPreview(candidate,result);
    }catch(error){if(active(candidate))$('modal_status').textContent=error.message;}
    finally{candidate.busy=false;if(active(candidate))updatePreview();}
  }
  async function submit(candidate) {
    if(!active(candidate)||candidate.busy)return;
    if(candidate.mode==='import'){
      if(!candidate.token)throw new Error(tr('请选择迁移文件、填写密码，然后点击「打开迁移包」。','Select a migration file, enter its password, then click Open package.'));
      if(!candidate.previewReady||!candidate.importConfig)throw new Error(tr('服务列表尚未准备好，请稍候再导入。','The service list is not ready yet. Please wait before importing.'));
      if(candidate.preferenceConflicts?.some(conflict=>Object.values(conflict.fields).some(field=>!field.resolved)))throw new Error(tr('请在导入预览中选择模型显示与上下文设置','Choose model visibility and context settings in this import preview'));
      if(candidate.demo){closeModal();notice(tr('演示已完成，未保存服务','Demo finished; no services saved'));return;}
      const selected=selections(candidate);
      for(const row of importedNameRows(candidate)){
        const source=selected.find(item=>item.kind===row.kind && item.id===row.record._shadow_source_id);
        if(source)source.name=candidate.nameEdits?.get(row.kind+':'+row.id)?.trim() || row.name;
      }
      await api('/api/migration/import',{method:'POST',body:JSON.stringify({token:candidate.token,selections:selected,...options(candidate)}),signal:candidate.controller.signal});
      if(!active(candidate))return;
      const loaded=await api('/api/config');if(!active(candidate))return;
      state=loaded;fill();closeModal();notice(tr('所选服务已导入','Selected services imported'));return;
    }
    const password=$('shadow_export_password').value, confirmation=$('shadow_export_password_confirm').value;
    if(password!==confirmation)throw new Error(tr('两次密码不一致','Passwords do not match'));
    const passwordBytes=new TextEncoder().encode(password.trim()).length;
    if(passwordBytes>4096)throw new Error(tr('密码不能超过 4096 个 UTF-8 字节','Password cannot exceed 4096 UTF-8 bytes'));
    if(passwordBytes<12)throw new Error(tr('密码至少需要 12 个 UTF-8 字节','Password requires at least 12 UTF-8 bytes'));
    candidate.busy=true;updatePreview();
    const controls=[...$('shadow_migration').querySelectorAll('input,button')];controls.forEach(control=>control.disabled=true);
    $('modal_status').textContent=tr('正在加密所选服务…','Encrypting selected services…');
    let result;try{const authorization=await api('/api/migration/export/confirm',{method:'POST',body:'{}'});result=await api('/api/migration/export',{method:'POST',body:JSON.stringify({confirmation:authorization.confirmation,password,selections:selections(candidate),...options(candidate)}),signal:candidate.controller.signal});}finally{if(active(candidate)){controls.forEach(control=>control.disabled=false);candidate.busy=false;updatePreview();}}
    if(!active(candidate))return;
    const bytes=Uint8Array.from(atob(result.bundle),c=>c.charCodeAt(0));const url=URL.createObjectURL(new Blob([bytes],{type:'application/octet-stream'}));
    const link=document.createElement('a');link.href=url;link.download='EMP.emp';link.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
    closeModal();notice(tr('所选服务已导出','Selected services exported'));
  }
  return {open(){if(window.EMP_PREVIEW && shadowServicesDemoActive){notice(tr('结束服务演示后可迁移数据','End the service demo to migrate data'));return;}stop();view={mode:'import',rows:[],includeContext:true,includeRename:true,controller:new AbortController()};render();},stop};
}

function migrationScrub(value) {
  if(Array.isArray(value))return value.map(migrationScrub);
  if(value&&typeof value==='object')return Object.fromEntries(Object.entries(value).filter(([key])=>!['api_key','api_key_file','auth_file','auth','imported_auth','provider_keys','access_token','refresh_token','id_token','password','authorization','cookie'].includes(key.toLowerCase())).map(([key,item])=>[key,migrationScrub(item)]));
  return value;
}
