function createRoutingSettings({api, getState, cloneState, persistState, openModal, closeModal, $, esc, tr}) {
  const memoryStages = [
    {field:'memory_extraction_model', effort:'memory_extraction_effort', defaultEffort:'low', id:'routing_memory_extraction_model', kind:'memory_extraction', defaultKey:'extract_model', label:'models.memory_extraction_model', help:'models.memory_extraction_help'},
    {field:'memory_model', effort:'memory_consolidation_effort', defaultEffort:'medium', id:'routing_memory_model', kind:'memory_consolidation', defaultKey:'consolidation_model', label:'models.memory_model', help:'models.memory_routing_help'},
  ];
  function memoryModels(state) {
    const models = new Map();
    for (const model of [...(state.subscription_models || []), ...(state.catalog_models || []).filter(model => ['native','account'].includes(model.source_type))]) {
      const id = model.id.split('/').at(-1);
      if (/^gpt-[a-z0-9._-]+$/.test(id) && !models.has(id)) models.set(id, {label:model.default_display_name || model.display_name || id, levels:model.reasoning_levels || []});
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
      .filter(account => account.auth_mode !== 'local_key' && account.enabled !== false && account.credential_set && account.credential_status !== 'invalid' && !account.duplicate && account.quota?.plan_type?.toLowerCase() !== 'free');
  }
  function localDate(seconds) {
    if (!seconds) return '';
    const date = new Date(seconds * 1000);
    return new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0,16);
  }
  const creditModels = ['gpt-image-2.6-flare', 'gpt-image-2.6-sunburst'];
  const creditEndpoint = 'https://api.openai.com/v1';
  const isCreditService = provider => provider?.enabled !== false && provider?.auth_mode === 'api_key' && provider.base_url === creditEndpoint;
  function connectCredit(back) {
    const current = getState().image_routing?.credit;
    const provider = getState().providers.find(item => item.id === current?.provider && isCreditService(item) && /^openai-image-credit(?:-\d+)?$/.test(item.id));
    const reopen = () => connectCredit(back);
    openModal(tr('models.credit_service'), `<div class="settings-list" id="credit_connect_form">
      <div class="form-links"><a href="https://platform.openai.com/api-keys" target="_blank" rel="noopener">${tr('models.credit_create_key')}</a>
      <button type="button" class="secondary" id="credit_copy_prompt">${tr('models.credit_copy_prompt')}</button></div>
      <div class="setting-row routing-mode"><label for="credit_key">API Key</label><input id="credit_key" type="password" autocomplete="off" ${provider?.api_key_set ? '' : 'required'}></div>
      <div class="setting-row routing-mode"><label for="credit_connect_budget">${tr('models.credit_budget')}</label><input type="number" id="credit_connect_budget" min="0.01" max="1000000" step="0.01" value="${current?.budget_cents ? current.budget_cents/100 : ''}" required></div>
      <div class="setting-row routing-mode"><label for="credit_connect_expiry">${tr('models.credit_expiry')}</label><input type="datetime-local" id="credit_connect_expiry" value="${localDate(current?.expires_at)}" required></div>
      <details><summary>${tr('models.credit_advanced')}</summary><div class="setting-row routing-mode"><label for="credit_connect_percent">${tr('models.credit_percent')}</label><input type="number" id="credit_connect_percent" min="1" max="100" value="${current?.percent ?? 95}" required></div>
      <label><input type="checkbox" id="credit_connect_monthly" ${current?.renew_monthly ? 'checked' : ''}> ${tr('models.credit_monthly')}</label></details>
    </div>`, tr('save_display'), async () => {
      const form = $('credit_connect_form'), candidate = cloneState();
      const now = Math.floor(Date.now()/1000), expiry = new Date($('credit_connect_expiry').value).getTime()/1000;
      const start = current && current.expires_at > now ? current.cycle_started_at : now;
      const key = $('credit_key').value.trim();
      if (![...form.querySelectorAll('input')].every(input => input.checkValidity()) || expiry <= now || (!key && !provider?.api_key_set)) {
        throw new Error(tr('models.credit_complete_fields'));
      }
      let id = candidate.providers.find(item => item.id === provider?.id && isCreditService(item))?.id;
      if (!id && !key) throw new Error(tr('models.credit_complete_fields'));
      if (!id) {
        id = 'openai-image-credit';
        for (let suffix=2; candidate.providers.some(item => item.id === id); suffix++) id = 'openai-image-credit-'+suffix;
        const name = tr('models.credit_service') + (id === 'openai-image-credit' ? '' : ' '+id.split('-').at(-1));
        candidate.providers.push({id,name,base_url:creditEndpoint,protocol:'responses',auth_mode:'api_key',api_key:key,enabled:true});
      } else if (key) {
        candidate.providers.find(item => item.id === id).api_key = key;
      }
      candidate.image_routing = {mode:'credit',account_id:'',credit:{provider:id,model:current?.model || creditModels[0],
        cycle_started_at:start,expires_at:expiry,budget_cents:Math.round($('credit_connect_budget').valueAsNumber*100),
        percent:$('credit_connect_percent').valueAsNumber,renew_monthly:$('credit_connect_monthly').checked}};
      await persistState(tr('models.settings_saved'), candidate);
      if (form.isConnected) open({back});
    }, null, {back,reopen});
    $('credit_copy_prompt').onclick = async event => {
      const button = event.currentTarget;
      try {
        await navigator.clipboard.writeText(tr('models.credit_setup_prompt'));
        button.textContent = tr('models.credit_prompt_copied');
      } catch (_) {
        $('modal_status').textContent = tr('models.credit_setup_prompt');
      }
    };
  }
  function open({back} = {}, draft) {
    const state = getState(), eligible = accounts(state);
    draft ||= {review:state.auto_review_fallback !== false, mode:state.image_routing?.mode || 'follow', account:state.image_routing?.account_id || '', credit:state.image_routing?.credit || {}, memory_model:state.memory_model || '', memory_extraction_model:state.memory_extraction_model ?? state.memory_model ?? '', memory_extraction_effort:state.memory_extraction_effort || '', memory_consolidation_effort:state.memory_consolidation_effort || ''};
    const selected = draft.account || eligible[0]?.id || '';
    const options = eligible.map(account => `<option value="${esc(account.id)}" ${account.id === selected ? 'selected' : ''}>${esc(account.name || account.id)}</option>`).join('');
    const missing = selected && !eligible.some(account => account.id === selected);
    const title = tr("models.routing_settings");
    const models = memoryModels(state);
    const credit = draft.credit;
    const creditOptions = state.providers.filter(provider => provider.api_key_set && isCreditService(provider) && !/^team-[0-9a-fA-F]{32}$/.test(provider.id))
      .map(provider => `<option value="${esc(provider.id)}" ${provider.id === credit.provider ? 'selected' : ''}>${esc(provider.name)}</option>`).join('');
    const defaults = state.codex_context_settings?.memory_default_models;
    const comparisonRows = [
      ['models.memory_usage_extraction', '0.98×', '3.27×', '0.57%', '11.92×'],
      ['models.memory_usage_consolidation', '1.40×', '5.44×', '90.84%', '3.19×'],
      ['models.memory_usage_conversation', '1×', '1×', '97.32%', '1×'],
    ].map(([label,...values]) => `<tr><th scope="row">${tr(label)}</th>${values.map(value => `<td>${value}</td>`).join('')}</tr>`).join('');
    const comparison = `<strong>${tr('models.memory_usage_title')}</strong><p>${tr('models.memory_usage_reference')}</p><p>${tr('models.memory_usage_baseline')}</p><div class="collaboration-table" tabindex="0" role="region" aria-label="${esc(tr('models.memory_usage_title'))}"><table><thead><tr>${['models.memory_usage_stage','reports.input_tokens','reports.output_tokens','models.memory_usage_cache','models.memory_usage_usd'].map(key => `<th scope="col">${tr(key)}</th>`).join('')}</tr></thead><tbody>${comparisonRows}</tbody></table></div><p>${tr('models.memory_usage_basis')}</p>`;
    const memoryRows = memoryStages.map(stage => {
      const selectedModel = draft[stage.field], defaultModel = defaults?.[stage.defaultKey];
      const defaultLabel = defaultModel ? tr('models.codex_memory_defaults', {models:defaultModel}) : tr('models.memory_defaults_unavailable');
      const options = [...models].map(([id,model]) => `<option value="${esc(id)}" ${selectedModel === id ? 'selected' : ''}>${esc(model.label)}</option>`).join('');
      return `<div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="${stage.id}">${tr(stage.label)}</label><button type="button" class="shadow-help" data-shadow-help="" data-shadow-help-template="${stage.id}_help" aria-label="${esc(tr(stage.label))}" aria-haspopup="dialog" aria-expanded="false" aria-controls="shadow_feedback">?</button><template id="${stage.id}_help"><div class="collaboration-comparison">${tr(stage.help).split('\n\n').map(text => `<p>${esc(text)}</p>`).join('')}${comparison}</div></template></div><div class="memory-routing-controls"><select id="${stage.id}"><option value="">${esc(defaultLabel)}</option>${selectedModel && !models.has(selectedModel) ? `<option value="${esc(selectedModel)}" selected disabled>${esc(selectedModel)} · ${tr('core.unavailable')}</option>` : ''}${options}</select><select id="${stage.id}_effort" aria-label="${esc(tr('models.memory_effort_for', {stage:tr(stage.label)}))}" title="${esc(tr('models.subscription_comparison_effort'))}"></select></div></div><p id="${stage.id}_status" class="setting-help" role="status"></p>`;
    }).join('');
    openModal(title, `<div class="routing-settings">
      <div class="setting-row"><div class="setting-title collaboration-title"><span id="routing_review_label">${tr("models.auto_review")}</span>${shadowHelp(tr("models.use_another_subscription_when_native_runs_out_of_quota"))}</div><button type="button" class="setting-switch" id="routing_review" role="switch" aria-checked="${draft.review}" aria-labelledby="routing_review_label"></button></div>
      <div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="routing_image_mode">${tr("models.image_generation_and_editing")}</label><button type="button" class="shadow-help" data-shadow-help="" data-shadow-help-template="routing_image_help" aria-label="${esc(tr('models.image_generation_and_editing'))}" aria-haspopup="dialog" aria-expanded="false" aria-controls="shadow_feedback">?</button><template id="routing_image_help">${tr('models.image_routing_help').split('\n\n').map(text => `<p>${esc(text)}</p>`).join('')}</template></div><select id="routing_image_mode">${[['follow',tr("models.follow_conversation")],['account',tr("models.specific_account")],['auto',tr("models.automatic")],['credit',tr("models.openai_image_credit")]].map(([mode,label]) => `<option value="${mode}" ${mode === draft.mode ? 'selected' : ''}>${label}</option>`).join('')}</select></div>
      <div class="setting-row routing-account" id="routing_image_account_row" ${draft.mode === 'account' ? '' : 'hidden'}><label class="setting-title" for="routing_image_account">${tr("models.image_account")}</label><select id="routing_image_account"><option value="" ${selected ? '' : 'selected'}>${tr("models.choose_an_account")}</option>${missing ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr("core.unavailable")}</option>` : ''}${options}</select></div>
      <div id="routing_image_credit" class="settings-list" ${draft.mode === 'credit' ? '' : 'hidden'}>
        <div class="setting-row routing-mode"><div class="setting-title collaboration-title"><label for="credit_provider">${tr('models.credit_provider')}</label>${shadowHelp(tr('models.credit_help'))}</div><select id="credit_provider"><option value="">${tr('models.credit_choose_provider')}</option>${creditOptions}</select></div>
        <div class="setting-row routing-mode"><label for="credit_model">${tr('models.credit_image_model')}</label><select id="credit_model">${creditModels.map(model => `<option ${model === credit.model ? 'selected' : ''}>${model}</option>`).join('')}</select></div>
        <div class="setting-row routing-mode"><label for="credit_budget">${tr('models.credit_budget')}</label><input id="credit_budget" type="number" min="0.01" max="1000000" step="0.01" value="${credit.budget_cents ? credit.budget_cents / 100 : ''}" required></div>
        <div class="setting-row routing-mode"><label for="credit_start">${tr('models.credit_start')}</label><input id="credit_start" type="datetime-local" value="${localDate(credit.cycle_started_at)}" required></div>
        <div class="setting-row routing-mode"><label for="credit_expiry">${tr('models.credit_expiry')}</label><input id="credit_expiry" type="datetime-local" value="${localDate(credit.expires_at)}" required></div>
        <details><summary>${tr('models.credit_advanced')}</summary><div class="setting-row routing-mode"><label for="credit_percent">${tr('models.credit_percent')}</label><input id="credit_percent" type="number" min="1" max="100" value="${credit.percent ?? 95}" required></div>
        <label><input type="checkbox" id="credit_monthly" ${credit.renew_monthly ? 'checked' : ''}> ${tr('models.credit_monthly')}</label></details>
        ${credit.expires_at && credit.expires_at * 1000 <= Date.now() ? `<p class="setting-warning">${tr('models.credit_expired')}</p>` : ''}
        <p class="setting-help" id="routing_credit_usage" aria-live="polite"></p>
      </div>
      ${memoryRows}
    </div>`, tr("save_display"), async () => {
      const candidate = cloneState(), form = $('routing_image_mode').closest('.routing-settings'), controls = [...form.querySelectorAll('input,select,button')];
      candidate.auto_review_fallback = $('routing_review').getAttribute('aria-checked') === 'true';
      const mode = $('routing_image_mode').value, account = $('routing_image_account').value;
      if (mode === 'account' && !accounts(getState()).some(item => item.id === account)) throw new Error(tr("models.choose_an_available_subscription_account"));
      candidate.image_routing = {mode, account_id:mode === 'account' ? account : ''};
      if (mode === 'credit') {
        const imageCredit = readCredit();
        if (!imageCredit.provider || ![...$('routing_image_credit').querySelectorAll('input')].every(input => input.checkValidity()) || !(imageCredit.cycle_started_at < imageCredit.expires_at)) {
          throw new Error(tr('models.credit_complete_fields'));
        }
        candidate.image_routing.credit = imageCredit;
      } else if (state.image_routing?.credit) {
        candidate.image_routing.credit = state.image_routing.credit;
      }
      for (const stage of memoryStages) {
        candidate[stage.field] = $(stage.id).value;
        candidate[stage.effort] = $(stage.id + '_effort').value;
      }
      const memoryChanged = memoryStages.some(stage => candidate[stage.field] !== (getState()[stage.field] ?? ''));
      const effortChanged = memoryStages.some(stage => candidate[stage.effort] !== (getState()[stage.effort] ?? ''));
      controls.forEach(control => control.disabled = true);
      try {
        await persistState(tr(memoryChanged ? 'models.memory_routing_saved' : effortChanged ? 'models.memory_effort_saved' : 'models.routing_settings_saved'), candidate);
        if (form.isConnected) (back || closeModal)();
      } finally { controls.forEach(control => control.disabled = false); }
    }, null, {back, reopen:() => open({back}, read())});
    function readCredit() {
      return {provider:$('credit_provider').value, model:$('credit_model').value,
        budget_cents:Math.round($('credit_budget').valueAsNumber * 100), percent:$('credit_percent').valueAsNumber,
        cycle_started_at:new Date($('credit_start').value).getTime() / 1000, expires_at:new Date($('credit_expiry').value).getTime() / 1000, renew_monthly:$('credit_monthly').checked};
    }
    function read() { return {review:$('routing_review').getAttribute('aria-checked') === 'true', mode:$('routing_image_mode').value, account:$('routing_image_account').value, credit:readCredit(), ...Object.fromEntries(memoryStages.flatMap(stage => [[stage.field, $(stage.id).value], [stage.effort, $(stage.id + '_effort').value]]))}; }
    $('routing_review').onclick = () => $('routing_review').setAttribute('aria-checked', String($('routing_review').getAttribute('aria-checked') !== 'true'));
    const creditUsage = $('routing_credit_usage');
    if (state.image_routing?.credit) void api('/api/image-credit').then(usage => {
      if (!creditUsage.isConnected || !usage) return;
      const money = new Intl.NumberFormat(document.documentElement.lang, {style:'currency',currency:'USD',maximumFractionDigits:4});
      const spent = money.format(usage.estimated_spent_nanos / 1e9), limit = money.format(usage.limit_nanos / 1e9);
      creditUsage.textContent = tr('models.credit_usage', {spent,limit});
      if (usage.active && credit.renew_monthly) creditUsage.textContent += ' · ' + tr('models.credit_next_boundary', {date:new Date(usage.cycle_ends_at * 1000).toLocaleString(document.documentElement.lang)});
      if (usage.unknown_requests) creditUsage.textContent += ' ' + tr('models.credit_unknown');
      else if (usage.estimated_spent_nanos >= usage.limit_nanos) creditUsage.textContent += ' ' + tr('models.credit_reached');
    }).catch(() => {
      if (creditUsage.isConnected) creditUsage.textContent = tr('models.credit_status_unavailable');
    });
    $('routing_image_mode').onchange = () => {
      $('routing_image_account_row').hidden = $('routing_image_mode').value !== 'account';
      $('routing_image_credit').hidden = $('routing_image_mode').value !== 'credit';
    };
    for (const stage of memoryStages) {
      const select = $(stage.id), effort = $(stage.id + '_effort');
      function updateEfforts(selected) {
        const levels = models.get(select.value || defaults?.[stage.defaultKey])?.levels || [];
        effort.innerHTML = `<option value="">${esc(tr('models.memory_effort_default', {effort:stage.defaultEffort}))}</option>${selected && !levels.includes(selected) ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr('core.unavailable')}</option>` : ''}${levels.map(level => `<option value="${esc(level)}" ${level === selected ? 'selected' : ''}>${esc(level)}</option>`).join('')}`;
      }
      select.onchange = () => {
        select.title = select.selectedOptions[0].textContent;
        const levels = models.get(select.value || defaults?.[stage.defaultKey])?.levels || [];
        updateEfforts(levels.includes(effort.value) ? effort.value : '');
      };
      select.title = select.selectedOptions[0].textContent;
      updateEfforts(draft[stage.effort]);
    }
    refresh();
  }
  return {open, refresh, connectCredit};
}
