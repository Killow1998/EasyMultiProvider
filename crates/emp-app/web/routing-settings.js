// Route preferences share one dialog; changes apply only after Save succeeds.
function createRoutingSettings({getState, cloneState, persistState, openModal, closeModal, $, esc, tr, help}) {
  function memoryModels(state) {
    const models = new Map();
    for (const model of [...(state.subscription_models || []), ...(state.catalog_models || []).filter(model => ['native','account'].includes(model.source_type))]) {
      const id = model.id.split('/').at(-1);
      if (/^gpt-[a-z0-9._-]+$/.test(id) && !models.has(id)) models.set(id, model.default_display_name || model.display_name || id);
    }
    return models;
  }
  function refresh() {
    const state = getState(), node = $('routing_memory_status'), status = state.memory_routing_status;
    if (node) node.textContent = status?.state === 'paused' && status.model === state.memory_model
      ? tr('models.memory_paused_' + (['quota_exhausted','native_unavailable','account_unavailable','rate_limited','request_failed','source_conflict'].includes(status.reason) ? status.reason : 'model_unavailable')) : '';
  }
  function accounts(state) {
    return [...(state.native_account ? [{...state.native_account, id:'@native', name:'Native'}] : []), ...(state.accounts || [])]
      .filter(account => account.enabled !== false && account.credential_set && account.credential_status !== 'invalid' && !account.duplicate && account.quota?.plan_type?.toLowerCase() !== 'free');
  }
  function open({back} = {}, draft) {
    const state = getState(), eligible = accounts(state);
    draft ||= {review:state.auto_review_fallback !== false, mode:state.image_routing?.mode || 'follow', account:state.image_routing?.account_id || '', memory:state.memory_model || ''};
    const selected = draft.account || eligible[0]?.id || '';
    const options = eligible.map(account => `<option value="${esc(account.id)}" ${account.id === selected ? 'selected' : ''}>${esc(account.name || account.id)}</option>`).join('');
    const missing = selected && !eligible.some(account => account.id === selected);
    const title = tr("models.routing_settings");
    const models = memoryModels(state);
    const memoryOptions = [...models].map(([id,label]) => `<option value="${esc(id)}" ${draft.memory === id ? 'selected' : ''}>${esc(label)}</option>`).join('');
    openModal(title, `<div class="routing-settings">
      <div class="setting-row"><div><div class="setting-title" id="routing_review_label">${tr("models.auto_review")}</div><p class="setting-help" id="routing_review_help">${tr("models.use_another_subscription_when_native_runs_out_of_quota")}</p></div><button type="button" class="setting-switch" id="routing_review" role="switch" aria-checked="${draft.review}" aria-labelledby="routing_review_label" aria-describedby="routing_review_help"></button></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="routing_image_mode">${tr("models.image_generation_and_editing")}</label><select id="routing_image_mode">${[['follow',tr("models.follow_conversation")],['account',tr("models.specific_account")],['auto',tr("models.automatic")]].map(([mode,label]) => `<option value="${mode}" ${mode === draft.mode ? 'selected' : ''}>${label}</option>`).join('')}</select></div>
      <div class="setting-row routing-account" id="routing_image_account_row" ${draft.mode === 'account' ? '' : 'hidden'}><label class="setting-title" for="routing_image_account">${tr("models.image_account")}</label><select id="routing_image_account"><option value="" ${selected ? '' : 'selected'}>${tr("models.choose_an_account")}</option>${missing ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr("core.unavailable")}</option>` : ''}${options}</select></div>
      <div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="routing_memory_model">${tr('models.memory_model')}</label>${help(tr('models.memory_routing_help'))}</div><select id="routing_memory_model"><option value="">${tr('models.codex_decides')}</option>${draft.memory && !models.has(draft.memory) ? `<option value="${esc(draft.memory)}" selected disabled>${esc(draft.memory)} · ${tr('core.unavailable')}</option>` : ''}${memoryOptions}</select></div><p id="routing_memory_status" class="setting-help" role="status"></p>
    </div>`, tr("save_display"), async () => {
      const candidate = cloneState(), form = $('routing_image_mode').closest('.routing-settings'), controls = [...form.querySelectorAll('select,button')];
      candidate.auto_review_fallback = $('routing_review').getAttribute('aria-checked') === 'true';
      const mode = $('routing_image_mode').value, account = $('routing_image_account').value;
      if (mode === 'account' && !accounts(getState()).some(item => item.id === account)) throw new Error(tr("models.choose_an_available_subscription_account"));
      candidate.image_routing = {mode, account_id:mode === 'account' ? account : ''};
      candidate.memory_model = $('routing_memory_model').value;
      controls.forEach(control => control.disabled = true);
      try {
        await persistState(tr("models.routing_settings_saved"), candidate);
        if (form.isConnected) (back || closeModal)();
      } finally { controls.forEach(control => control.disabled = false); }
    }, null, {back, reopen:() => open({back}, read())});
    function read() { return {review:$('routing_review').getAttribute('aria-checked') === 'true', mode:$('routing_image_mode').value, account:$('routing_image_account').value, memory:$('routing_memory_model').value}; }
    $('routing_review').onclick = () => $('routing_review').setAttribute('aria-checked', String($('routing_review').getAttribute('aria-checked') !== 'true'));
    $('routing_image_mode').onchange = () => {
      $('routing_image_account_row').hidden = $('routing_image_mode').value !== 'account';
    };
    refresh();
  }
  return {open, refresh};
}
