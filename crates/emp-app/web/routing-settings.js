// Route preferences share one dialog; changes apply only after Save succeeds.
function createRoutingSettings({getState, cloneState, persistState, openModal, closeModal, $, esc, tr, help}) {
  const memoryStages = [
    {field:'memory_extraction_model', id:'routing_memory_extraction_model', kind:'memory_extraction', defaultKey:'extract_model', label:'models.memory_extraction_model', help:'models.memory_extraction_help'},
    {field:'memory_model', id:'routing_memory_model', kind:'memory_consolidation', defaultKey:'consolidation_model', label:'models.memory_model', help:'models.memory_routing_help'},
  ];
  function memoryModels(state) {
    const models = new Map();
    for (const model of [...(state.subscription_models || []), ...(state.catalog_models || []).filter(model => ['native','account'].includes(model.source_type))]) {
      const id = model.id.split('/').at(-1);
      if (/^gpt-[a-z0-9._-]+$/.test(id) && !models.has(id)) models.set(id, model.default_display_name || model.display_name || id);
    }
    return models;
  }
  function refresh() {
    const state = getState(), status = state.memory_routing_status;
    for (const stage of memoryStages) {
      const node = $(stage.id + '_status');
      if (node) node.textContent = status?.state === 'paused' && status.kind === stage.kind && status.model === state[stage.field]
        ? tr('models.memory_paused_' + (['quota_exhausted','native_unavailable','account_unavailable','rate_limited','request_failed','source_conflict'].includes(status.reason) ? status.reason : 'model_unavailable')) : '';
    }
  }
  function accounts(state) {
    return [...(state.native_account ? [{...state.native_account, id:'@native', name:'Native'}] : []), ...(state.accounts || [])]
      .filter(account => account.enabled !== false && account.credential_set && account.credential_status !== 'invalid' && !account.duplicate && account.quota?.plan_type?.toLowerCase() !== 'free');
  }
  function open({back} = {}, draft) {
    const state = getState(), eligible = accounts(state);
    draft ||= {review:state.auto_review_fallback !== false, mode:state.image_routing?.mode || 'follow', account:state.image_routing?.account_id || '', memory_model:state.memory_model || '', memory_extraction_model:state.memory_extraction_model ?? state.memory_model ?? ''};
    draft.adaptive ||= {...(state.adaptive_effort || {enabled:false, classifier_model:'gpt-6-luna', max_effort:'xhigh'})};
    const selected = draft.account || eligible[0]?.id || '';
    const options = eligible.map(account => `<option value="${esc(account.id)}" ${account.id === selected ? 'selected' : ''}>${esc(account.name || account.id)}</option>`).join('');
    const missing = selected && !eligible.some(account => account.id === selected);
    const title = tr("models.routing_settings");
    const models = memoryModels(state);
    const defaults = state.codex_context_settings?.memory_default_models;
    const memoryRows = memoryStages.map(stage => {
      const selectedModel = draft[stage.field], defaultModel = defaults?.[stage.defaultKey];
      const defaultLabel = defaultModel ? tr('models.codex_memory_defaults', {models:defaultModel}) : tr('models.memory_defaults_unavailable');
      const options = [...models].map(([id,label]) => `<option value="${esc(id)}" ${selectedModel === id ? 'selected' : ''}>${esc(label)}</option>`).join('');
      return `<div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="${stage.id}">${tr(stage.label)}</label>${help(tr(stage.help))}</div><select id="${stage.id}"><option value="">${esc(defaultLabel)}</option>${selectedModel && !models.has(selectedModel) ? `<option value="${esc(selectedModel)}" selected disabled>${esc(selectedModel)} · ${tr('core.unavailable')}</option>` : ''}${options}</select></div><p id="${stage.id}_status" class="setting-help" role="status"></p>`;
    }).join('');
    openModal(title, `<div class="routing-settings">
      <div class="setting-row"><div><div class="setting-title" id="routing_review_label">${tr("models.auto_review")}</div><p class="setting-help" id="routing_review_help">${tr("models.use_another_subscription_when_native_runs_out_of_quota")}</p></div><button type="button" class="setting-switch" id="routing_review" role="switch" aria-checked="${draft.review}" aria-labelledby="routing_review_label" aria-describedby="routing_review_help"></button></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="routing_image_mode">${tr("models.image_generation_and_editing")}</label><select id="routing_image_mode">${[['follow',tr("models.follow_conversation")],['account',tr("models.specific_account")],['auto',tr("models.automatic")]].map(([mode,label]) => `<option value="${mode}" ${mode === draft.mode ? 'selected' : ''}>${label}</option>`).join('')}</select></div>
      <div class="setting-row routing-account" id="routing_image_account_row" ${draft.mode === 'account' ? '' : 'hidden'}><label class="setting-title" for="routing_image_account">${tr("models.image_account")}</label><select id="routing_image_account"><option value="" ${selected ? '' : 'selected'}>${tr("models.choose_an_account")}</option>${missing ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr("core.unavailable")}</option>` : ''}${options}</select></div>
      ${memoryRows}
      <div class="setting-row"><div><div class="setting-title collaboration-title"><span id="routing_effort_label">${tr('models.adaptive_effort')}</span>${help(tr('models.adaptive_effort_help'))}</div><p class="setting-help">${tr('models.adaptive_effort_hint')}</p></div><button type="button" class="setting-switch" id="routing_adaptive_effort" role="switch" aria-checked="${draft.adaptive.enabled}" aria-labelledby="routing_effort_label"></button></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="routing_effort_classifier">${tr('models.effort_classifier')}</label><select id="routing_effort_classifier">${!models.has(draft.adaptive.classifier_model) ? `<option value="${esc(draft.adaptive.classifier_model)}" selected>${esc(draft.adaptive.classifier_model)} · ${tr('core.unavailable')}</option>` : ''}${[...models.keys()].map(id => `<option value="${esc(id)}" ${id === draft.adaptive.classifier_model ? 'selected' : ''}>${esc(id)}</option>`).join('')}</select></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="routing_effort_max">${tr('models.effort_ceiling')}</label><select id="routing_effort_max">${['medium','high','xhigh','max'].map(effort => `<option value="${effort}" ${effort === draft.adaptive.max_effort ? 'selected' : ''}>${effort}</option>`).join('')}</select></div>
    </div>`, tr("save_display"), async () => {
      const candidate = cloneState(), form = $('routing_image_mode').closest('.routing-settings'), controls = [...form.querySelectorAll('select,button')].map(control => ({control, disabled:control.disabled}));
      candidate.auto_review_fallback = $('routing_review').getAttribute('aria-checked') === 'true';
      const mode = $('routing_image_mode').value, account = $('routing_image_account').value;
      if (mode === 'account' && !accounts(getState()).some(item => item.id === account)) throw new Error(tr("models.choose_an_available_subscription_account"));
      candidate.image_routing = {mode, account_id:mode === 'account' ? account : ''};
      candidate.adaptive_effort = read().adaptive;
      if (candidate.adaptive_effort.enabled && !models.has(candidate.adaptive_effort.classifier_model)) throw new Error(tr('models.effort_classifier_unavailable'));
      for (const stage of memoryStages) candidate[stage.field] = $(stage.id).value;
      const memoryChanged = memoryStages.some(stage => candidate[stage.field] !== (getState()[stage.field] ?? ''));
      controls.forEach(({control}) => control.disabled = true);
      try {
        await persistState(tr(memoryChanged ? 'models.memory_routing_saved' : 'models.routing_settings_saved'), candidate);
        if (form.isConnected) (back || closeModal)();
      } finally { controls.forEach(({control, disabled}) => control.disabled = disabled); }
    }, null, {back, reopen:() => open({back}, read())});
    function read() { return {review:$('routing_review').getAttribute('aria-checked') === 'true', mode:$('routing_image_mode').value, account:$('routing_image_account').value, adaptive:{enabled:$('routing_adaptive_effort').getAttribute('aria-checked') === 'true', classifier_model:$('routing_effort_classifier').value, max_effort:$('routing_effort_max').value}, ...Object.fromEntries(memoryStages.map(stage => [stage.field, $(stage.id).value]))}; }
    const updateEffortControls = () => {
      const enabled = $('routing_adaptive_effort').getAttribute('aria-checked') === 'true';
      $('routing_effort_classifier').disabled = !enabled;
      $('routing_effort_max').disabled = !enabled;
    };
    $('routing_adaptive_effort').onclick = () => {
      const toggle = $('routing_adaptive_effort');
      toggle.setAttribute('aria-checked', String(toggle.getAttribute('aria-checked') !== 'true'));
      updateEffortControls();
    };
    updateEffortControls();
    $('routing_review').onclick = () => $('routing_review').setAttribute('aria-checked', String($('routing_review').getAttribute('aria-checked') !== 'true'));
    $('routing_image_mode').onchange = () => {
      $('routing_image_account_row').hidden = $('routing_image_mode').value !== 'account';
    };
    for (const stage of memoryStages) {
      const select = $(stage.id);
      select.onchange = () => { select.title = select.selectedOptions[0].textContent; };
      select.onchange();
    }
    refresh();
  }
  return {open, refresh};
}
