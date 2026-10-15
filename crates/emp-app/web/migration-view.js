// Pure list projection and markup; no reads, timers, requests or persistence.
function createMigrationView(getState) {
  function mergeRules() {
    return [
      tr('Codex 订阅：确认同一账号时更新已有服务；不同账号或无法确认身份时保留为独立服务。','Codex subscriptions: verified matching accounts update the existing service; different or unverifiable accounts remain separate.'),
      tr('外部 API 服务：连接方式相同且完整 base URL 与非空 API key 都确认一致时合并；任一不同或无法确认时保留双方。相同上游模型更新，其他模型保留。','External API services: matching connection methods, exact base URL and verified nonempty API key merge; different or unverifiable connections remain separate. Matching upstream models update; other models remain.'),
      tr('本机 Claude 订阅：没有可验证的账号身份时保留为独立服务。','Local Claude subscriptions: retain a separate service when account identity cannot be verified.'),
      tr('名称仅用于显示，同名不会导致合并或阻止导入。右侧预览和黄色提醒说明每项的实际处理。','Names are for display; matching names do not merge services or prevent import. The preview and yellow notices describe each action.')
    ].join('\n\n');
  }
  function modelChoices(service, source, importing) {
    if (service.kind === 'provider') {
      return (source.models || []).filter(model => model.provider === service.id).map(model => ({
        id:model.id, selected:true, name:model.display_name || model.upstream_id,
        original_name:model.display_name || model.upstream_id,
        context_window:model.context_window || '', enabled:model.enabled !== false,
      }));
    }
    const windows = service.record.model_context_windows || {};
    const snapshot = (source._shadow_model_previews || source.model_previews || {})[service.id];
    if (snapshot?.length) {
      return snapshot.map(model => ({...model,
        context_window:windows[model.id] ?? model.context_window ?? '',
        selected:!(service.record.hidden_models || []).includes(model.id),
      }));
    }
    const catalog = getState().subscription_models || [];
    const known = importing ? catalog.concat(Object.keys(windows)
      .filter(id => !catalog.some(model => model.id === id))
      .map(id => ({id, display_name:id, context_window:windows[id]}))) : catalog;
    return known.map(model => ({
      id:model.id, selected:!(service.record.hidden_models || []).includes(model.id),
      name:importing ? model.display_name || model.id : presentationName(
        service.id === '@native' ? model.id : (service.record.prefix || service.id) + '/' + model.id,
        model.display_name || model.id),
      original_name:canonicalDisplayName(model.display_name || model.id),
      context_window:windows[model.id] ?? '',
      default_context:model.default_context_window || model.context_window,
      maximum:model.max_context_window,
    }));
  }
  function nativeExportId(config) {
    const reserved = new Set([...(config.accounts || []),...(config.providers || [])]
      .flatMap(record => [record.id,record.prefix]).filter(Boolean));
    const alphabet = 'abcdefghijklmnopqrstuvwxyz0123456789';
    for (;;) {
      const bytes = crypto.getRandomValues(new Uint8Array(4));
      const id = [...bytes].map(byte => alphabet[byte % alphabet.length]).join('');
      if (!reserved.has(id)) return id;
    }
  }
  function serviceType(row) {
    if (row.kind === 'account') return 'Codex Subscription';
    if (row.record.execution_backend === 'claude_cli' && row.record.auth_mode === 'claude_login') return tr('本机 Claude 订阅','Local Claude subscription');
    return tr('外部 API 服务','External API service');
  }
  function services(config, importing = false, credentials = {}) {
    const state = getState();
    let accounts = importing ? config.accounts || [] : [state.native_account, ...(state.accounts || [])].filter(Boolean);
    if(importing&&config._shadow_merge_preview){
      accounts=[...(config.native_account?[config.native_account]:[]),...accounts];
      accounts=accounts.filter(account=>!account.duplicate||!accounts.some(owner=>account.duplicate_of==='当前 Codex 登录'?owner.native:owner.id===account.duplicate_of));
    }
    const rows = accounts.map(record => ({
      id:record.id, kind:'account', brand:'codex', selected:true, record,
      name:record.native ? 'Native' : record.name || record.id,
      credential_set:importing ? record.native&&record.credential_set || (credentials.accounts || []).includes(record.id) : record.credential_set,
    }));
    for (const record of config.providers || []) {
      rows.push({id:record.id, kind:'provider', selected:true, record,
        name:record.name || record.id,
        brand:record.execution_backend === 'claude_cli' ? 'claudecode' :
          Object.entries(officialProviders).find(([,preset]) => preset.base_url === record.base_url)?.[0] || 'custom',
        credential_set:importing ? (credentials.providers || []).includes(record.id) :
          record.api_key_set || record.auth_mode === 'claude_login',
      });
    }
    for (const row of rows) {
      row.models = modelChoices(row, config, importing);
      if (!importing && row.record.native) row.export_id = nativeExportId(config);
      if (!importing && row.kind === 'account' && !row.credential_set) {
        row.selected = false; row.exportable = false;
      }
    }
    return rows;
  }
  function selections(candidate) {
    return candidate.rows.filter(row => row.selected).map(row => ({
      id:row.id, kind:row.kind, name:row.name, export_id:row.export_id, source_token:row.record._shadow_migration_token,
      models:row.models.map(model => ({id:model.id, selected:model.selected,
        name:model.name, original_name:model.original_name, context_window:model.context_window,
        default_context:model.default_context, maximum:model.maximum,
      })),
    }));
  }
  function options(candidate) {
    return {include_context:candidate.includeContext, include_rename:candidate.includeRename,preference_choices:candidate.preferenceChoices||{}};
  }
  function preferenceRows(candidate){
    const summary=(kind,value)=>kind==='visibility'?(value.length?tr(`隐藏 ${value.length} 个模型`,`Hide ${value.length} models`):tr('全部显示','Show all')):
      Object.keys(value).length?Object.entries(value).slice(0,4).map(([model,tokens])=>`${model}: ${compactContext(tokens)}`).join(' · '):tr('目录默认值','Catalog defaults');
    return (candidate.preferenceConflicts||[]).map(conflict=>`<section class="migration-preference-conflict"><strong>${esc(tr(`与 ${conflict.target_name} 为同一账号，合并前确认设置`,`Same account as ${conflict.target_name}; choose settings before importing`))}</strong><div class="migration-preference-choices">${Object.entries(conflict.fields).map(([kind,field])=>`<label>${kind==='visibility'?tr('模型显示','Model visibility'):tr('上下文设置','Context settings')}<select data-migration-preference-source="${esc(conflict.source_id)}" data-migration-preference-kind="${kind}"><option value="" disabled ${!field.resolved?'selected':''}>${tr('请选择','Choose')}</option>${['current','incoming'].map(choice=>`<option value="${choice}" ${field.choice===choice?'selected':''}>${choice==='current'?tr('保留本机','Keep local'):tr('采用导入','Use incoming')} · ${esc(summary(kind,field[choice]))}</option>`).join('')}</select></label>`).join('')}</div></section>`).join('');
  }
  function importedNameRows(candidate) {
    const selected = new Set(candidate.rows.filter(row => row.selected).map(row => row.kind+':'+row.id));
    return services(candidate.importConfig, true).filter(row =>
      row.record._shadow_migration_token === candidate.token && selected.has(row.kind+':'+row.record._shadow_source_id));
  }
  function warningHtml(row, candidate, rows) {
    const name = item => candidate.nameEdits?.get(item.kind+':'+item.id)?.trim() || item.name;
    const warnings = (candidate.warnings || []).filter(warning => warning.kind === row.kind && warning.id === row.id &&
      (warning.code !== 'duplicate_name' || rows.some(other => (other.kind !== row.kind || other.id !== row.id) && name(other) === name(row))));
    return warnings.length ? shadowHelp(warnings.map(item => tr(item.message,item.message_en || item.message)).join('\n'),'!','warning') : '';
  }
  function previewRows(rows, candidate, nativePlaceholder = false) {
    const placeholder=nativePlaceholder&&!(candidate.mode==='import'&&rows.some(row=>row.record.native));
    let html = placeholder ? `<div class="migration-preview-row">${serviceIconHtml('codex')}
      <div><strong>Native</strong><small>${candidate.mode === 'export' ?
        tr('新电脑的本机登录','Local sign-in on the new computer') :
        tr('保留当前本机登录','Current local sign-in is retained')}</small></div></div>` : '';
    const editable = new Set(candidate.mode === 'import' && candidate.importConfig ? importedNameRows(candidate).map(row => row.kind+':'+row.id) : []);
    for (const row of rows) {
      const sourceId = candidate.mode === 'import' ? row.record._shadow_source_id || row.id : row.id;
      const index = candidate.rows.findIndex(item => item.kind === row.kind && item.id === sourceId);
      const models = index >= 0 ? candidate.rows[index].models : row.models || [];
      const count = models.filter(model => model.selected).length;
      const id = candidate.mode === 'export' ? row.export_id || row.id : row.id;

      const key = row.kind+':'+row.id;
      const name = editable.has(key) ? `<input class="migration-preview-name" data-migration-name="${esc(key)}" aria-label="${esc(tr('名称','Name')+' · '+row.name)}" value="${esc(candidate.nameEdits?.get(key) ?? row.name)}" autocomplete="off">` : `<strong>${esc(row.name || serviceType(row))}</strong>`;
      const warning = `<span data-migration-warning="${esc(key)}">${warningHtml(row,candidate,rows)}</span>`;
      html += `<div class="migration-preview-row">${serviceIconHtml(row.brand,
        row.record?.execution_backend === 'claude_cli' && row.record?.auth_mode !== 'claude_login')}
        <div><div class="migration-preview-heading">${name}<span>· ${serviceType(row)}</span>${warning}</div>${!row.credential_set ? `<small>${tr('需要登录','Sign-in required')}</small>` : ''}
          <details class="migration-preview-models" data-migration-detail="${esc(row.kind+':'+id)}"><summary>Model: ${count}</summary>
            ${models.map((model, mi) => modelHtml(model,index,mi,candidate)).join('')}</details>
        </div></div>`;
    }
    return html || `<p class="migration-preview-empty">${tr('选择服务后显示预览','Select services to preview')}</p>`;
  }
  function modelHtml(model, serviceIndex, index, candidate) {
    const selectable = serviceIndex >= 0;
    const label = candidate.includeRename ? model.name : model.original_name;
    const context = candidate.includeContext ? model.context_window : '';
    return `<label class="migration-model">${selectable ? `<input type="checkbox"
      data-migration-model-select="${serviceIndex}:${index}" ${model.selected ? 'checked' : ''}>` : ''}
      <span>${esc(label || model.id)}<small>${esc(model.id)}${context ? ' · '+compactContext(context) : ''}</small></span></label>`;
  }
  function rowHtml(row, index) {
    return `<label class="migration-service"><input type="checkbox" data-migration-select="${index}"
      ${row.exportable === false ? 'disabled' : ''} ${row.selected ? 'checked' : ''}
      aria-label="${esc(serviceType(row)+' · '+row.name)}">
      <span>${serviceIconHtml(row.brand,row.record.execution_backend === 'claude_cli' && row.record.auth_mode !== 'claude_login')}
        <span class="migration-service-label"><strong>${esc(row.name)}</strong><small>${serviceType(row)}</small></span></span></label>`;
  }
  function bodyHtml(candidate) {
    const importing = candidate.mode === 'import';
    const tabs = `<div class="report-tabs">
      <button type="button" data-migration-mode="import" class="secondary ${importing ? 'active' : ''}">${tr('导入','Import')}</button>
      <button type="button" data-migration-mode="export" class="secondary ${!importing ? 'active' : ''}">${tr('导出','Export')}</button></div>`;
    const steps=importing?`<ol class="migration-steps" aria-label="${tr('导入步骤','Import steps')}">
      ${[tr('选择文件与密码','Choose file and password'),tr('打开迁移包','Open package'),tr('确认后导入','Review and import')].map((label,index)=>`<li data-migration-step="${index}"><span aria-hidden="true">${index+1}</span>${label}</li>`).join('')}</ol>`:'';
    const chooser = importing && !candidate.token ? `
      <p class="migration-open-hint">${tr('选择 .emp 文件并填写导出时的密码，然后点击「打开迁移包」查看可导入的服务。','Choose a .emp file and enter its export password, then click Open package to review the services to import.')}</p>
      <label>${tr('迁移文件','Migration file')}<input type="file" id="shadow_migration_file" accept=".emp"></label>
      <label><span class="migration-field-label">${tr('迁移密码','Migration password')}${shadowHelp(tr('填写导出时的密码；兼容旧版 8 字节密码。首尾空白不计。','Use the export password; legacy 8-byte passwords remain supported. Leading and trailing whitespace is ignored.'))}</span><input id="shadow_import_password" type="password" autocomplete="current-password"></label>
      <button type="button" class="primary" data-migration-decode>${tr('打开迁移包','Open package')}</button><button type="button" class="secondary" data-migration-demo>${tr('查看导入演示','Preview an example import')}</button>` : '';
    const list = `<div class="migration-workspace"><div class="migration-choices">
      <div class="toolbar"><button type="button" class="secondary" data-migration-all="true">${tr('全选','Select all')}</button>
        <button type="button" class="secondary" data-migration-all="false">${tr('全不选','Select none')}</button></div>
      <div class="migration-services">${candidate.rows.map(rowHtml).join('')}</div></div>
      <div class="migration-preview"><h3>${importing ? tr('导入后的服务列表','Service list after import') :
        tr('新 EMP 全量导入后的服务列表','Service list after full import into a new EMP')}</h3>
        <p id="migration_selected_count" class="muted"></p><div id="migration_preview_rows"></div></div></div>`;
    const flags = `<div class="migration-options">
      <label><input type="checkbox" id="migration_include_context" ${candidate.includeContext ? 'checked' : ''}>${tr('包含订阅模型上下文设置','Include subscription model context settings')}</label>
      <label><input type="checkbox" id="migration_include_rename" ${candidate.includeRename ? 'checked' : ''}>${tr('包含订阅模型显示名称','Include subscription model display names')}</label></div>`;
    const passwords = !importing ? `<div class="migration-passwords">
      <label><span class="migration-field-label">${tr('迁移密码','Migration password')}${shadowHelp(tr('12–4096 个 UTF-8 字节；纯英文或数字至少 12 个字符。首尾空白不计，不要求特定字符组合。','12–4096 UTF-8 bytes; at least 12 characters for plain English or digits. Leading and trailing whitespace is ignored; no specific character combination is required.'))}</span><input type="password" id="shadow_export_password" autocomplete="new-password"></label>
      <label><span class="migration-field-label">${tr('确认密码','Confirm password')}${shadowHelp(tr('两次输入须完全一致。','Both entries must match exactly.'))}</span><input type="password" id="shadow_export_password_confirm" autocomplete="new-password"></label></div>` : '';
    return `<div id="shadow_migration">${tabs}${steps}${candidate.demo ? `<p class="migration-demo-note">${tr('演示数据，不会保存或导入真实服务。','Sample data; no services will be saved or imported.')}</p>` : ''}${chooser}${candidate.rows.length ? flags+list : ''}${passwords}</div>`;
  }
  return {services,selections,options,importedNameRows,warningHtml,previewRows,bodyHtml,mergeRules,preferenceRows};
}
