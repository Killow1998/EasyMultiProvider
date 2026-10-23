function createServiceChooser({tr, esc, presets, icon, openModal, getState}) {
  function card(action, name, brand, description, attributes = '', cpa = false) {
    return `<button type="button" class="choice service-choice" data-ui-action="${action}" ${attributes}>${icon(brand, cpa)}<span><strong>${name}</strong><small>${description}</small></span></button>`;
  }
  function group(name, cards) {
    return `<div class="service-choice-group"><h3>${name}</h3><div class="choice-grid">${cards}</div></div>`;
  }
  function codexChoices() {
    return card('account-import-device', tr('login.device_sign_in'), 'codex', tr('login.import_help'))
      + card('account-import-auth', 'auth.json', 'codex', tr('services.auth_import_help'));
  }
  function open() {
    const existingClaude = (getState().providers || []).find(p => p.execution_backend === 'claude_cli' && p.auth_mode === 'claude_login');
    const claude = card('claude-local-choose', tr("services.local_subscription"), 'claudecode', tr("services.connect_local_claude_code"), `data-id="${esc(existingClaude?.id || '')}"`)
      + card('claude-cpa-choose', 'CPA', 'claudecode', tr("services.enter_address_and_api_key"), '', true);
    const apiKeys = ['openai', 'anthropic', ...Object.keys(presets).filter(key => !['chatgpt','openai','anthropic'].includes(key))];
    const api = apiKeys.filter(key => presets[key]).map(key => card('official-provider-choose', esc(presets[key].name), key, tr("services.connect_an_api_service"), `data-provider="${esc(key)}"`)).join('')
      + card('service-custom-api', tr("services.custom_api"), 'custom', tr("services.enter_address_and_api_key"));
    openModal(tr("add_provider"), group(tr("services.codex_subscriptions"), codexChoices()) + group('Claude Code', claude) + group('API', api), '', null, null, {reopen:open});
  }
  return {open};
}
