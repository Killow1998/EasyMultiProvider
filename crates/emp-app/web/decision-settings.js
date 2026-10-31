function createDecisionSettings({getState, cloneState, persistState, openModal, api, $, esc, tr, help}) {
  function open(back, draft) {
    const state = getState();
    const providers = state.providers.filter(provider => provider.enabled !== false && provider.auth_mode === 'api_key' && provider.execution_backend !== 'claude_cli');
    const selected = draft?.providerId ?? state.decision_provider ?? '';
    const selections = draft?.selections || new Map();
    const catalogs = draft?.catalogs || new Map();
    const options = providers.map(provider => `<option value="${esc(provider.id)}" ${provider.id === selected ? 'selected' : ''}>${esc(provider.name || provider.id)}</option>`).join('');
    const missing = selected && !providers.some(provider => provider.id === selected);
    openModal(tr('core.decision_models'), `<div class="routing-settings" id="decision_settings">
      <p class="muted">${tr('core.decision_hint')}</p>
      <div class="setting-row routing-mode"><label class="setting-title" for="decision_provider">${tr('core.decision_service')}</label><select id="decision_provider"><option value="">${tr('core.decision_no_default')}</option>${missing ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr('core.unavailable')}</option>` : ''}${options}</select></div>
      <div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="decision_api">${tr('core.decision_api')}</label>${help(tr('core.decision_api_help'))}</div><select id="decision_api">${['openai','openrouter','systemone'].map(protocol => `<option value="${protocol}">${tr('core.decision_api_' + protocol)}</option>`).join('')}</select></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="decision_model">${tr('core.decision_model')}</label><div class="input-action"><select id="decision_model"></select><button type="button" class="secondary" data-icon="refresh" id="decision_refresh" title="${esc(tr('core.decision_refresh'))}" aria-label="${esc(tr('core.decision_refresh'))}"></button></div></div>
      <p class="muted" id="decision_catalog_status" role="status"></p>
      <div class="setting-row routing-mode" id="decision_custom_row" hidden><label class="setting-title" for="decision_custom_model">${tr('core.decision_custom_model')}</label><input id="decision_custom_model" autocomplete="off" placeholder="${tr('core.decision_custom_placeholder')}"></div>
      <p class="muted" id="decision_endpoint"></p>
    </div>`, tr('save_display'), async () => {
      const selection = read(), candidate = cloneState();
      candidate.decision_provider = selection.providerId;
      if (selection.providerId) {
        const provider = candidate.providers.find(provider => provider.id === selection.providerId);
        if (!providers.some(item => item.id === selection.providerId)) throw new Error(tr('core.decision_service_required'));
        const preference = selection.selections.get(selection.providerId);
        if (!preference.model) throw new Error(tr('core.decision_model_required'));
        provider.decision_api = preference;
      }
      const form = $('decision_settings'), controls = [...form.querySelectorAll('select,input,button')].map(control => ({control, disabled:control.disabled}));
      controls.forEach(({control}) => control.disabled = true);
      try {
        await persistState(tr('core.decision_saved'), candidate);
        if (form.isConnected) back();
      } finally { controls.forEach(({control, disabled}) => control.disabled = disabled); }
    }, null, {back, reopen:() => open(back, read())});
    const form = $('decision_settings');
    let active = '';
    function remember() {
      if (active && !$('decision_model').disabled) selections.set(active, {protocol:$('decision_api').value, model:$('decision_model').value === '@custom' ? $('decision_custom_model').value.trim() : $('decision_model').value});
    }
    function read() {
      remember();
      return {providerId:$('decision_provider').value, selections, catalogs};
    }
    async function fill(refresh = false) {
      active = $('decision_provider').value;
      const provider = providers.find(provider => provider.id === active);
      const protocol = provider?.decision_api?.protocol || ({'api.openai.com':'openai','api.typesafe.ai':'systemone'}[provider ? new URL(provider.base_url).hostname : ''] || 'openrouter');
      const preference = selections.get(active) || provider?.decision_api || {protocol, model:''};
      $('decision_api').value = preference.protocol;
      $('decision_custom_model').value = preference.model;
      $('decision_custom_row').hidden = true;
      $('decision_model').innerHTML = `<option value="">${tr('core.decision_model_placeholder')}</option>`;
      $('decision_api').disabled = !provider;
      $('decision_model').disabled = true;
      $('decision_refresh').disabled = !provider;
      $('decision_catalog_status').textContent = '';
      $('decision_endpoint').textContent = provider ? tr('core.decision_endpoint', {url:location.origin + '/v1/' + (preference.protocol === 'systemone' ? 'systemone' : 'decisions')}) : tr(providers.length ? 'core.decision_explicit_service_hint' : 'core.decision_add_service_hint');
      $('modal_submit').disabled = !!provider;
      if (!provider) return;
      selections.set(active, preference);
      const key = active + ':' + preference.protocol;
      const current = () => form.isConnected && $('decision_provider').value === provider.id && $('decision_api').value === preference.protocol;
      $('decision_catalog_status').textContent = tr('core.decision_loading');
      $('decision_refresh').disabled = true;
      let models;
      try {
        models = !refresh && catalogs.get(key) || (await api('/api/decisions/models', {method:'POST', body:JSON.stringify({provider:provider.id, protocol:preference.protocol})})).models;
        catalogs.set(key, models);
        if (current()) $('decision_catalog_status').textContent = models.length ? '' : tr('core.decision_empty');
      } catch (error) {
        models = catalogs.get(key) || [];
        if (current()) $('decision_catalog_status').textContent = tr('core.decision_load_failed', {error:error.message});
      }
      if (!current()) return;
      $('decision_model').innerHTML += models.map(model => `<option value="${esc(model.id)}">${esc(model.name)}</option>`).join('') + `<option value="@custom">${tr('core.decision_custom')}</option>`;
      const custom = preference.model && !models.some(model => model.id === preference.model);
      $('decision_model').value = custom ? '@custom' : preference.model;
      $('decision_custom_row').hidden = !custom;
      $('decision_model').disabled = $('decision_refresh').disabled = $('modal_submit').disabled = false;
    }
    $('decision_provider').onchange = () => { remember(); fill(); };
    $('decision_api').onchange = () => {
      selections.set(active, {protocol:$('decision_api').value, model:''});
      fill();
    };
    $('decision_model').onchange = () => { $('decision_custom_row').hidden = $('decision_model').value !== '@custom'; };
    $('decision_refresh').onclick = () => { remember(); fill(true); };
    fill();
  }
  return {open};
}
