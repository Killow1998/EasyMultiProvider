function createServiceChooser({tr, esc, presets, icon, openModal, getState}) {
  function card(action, name, brand, description, attributes = '', cpa = false) {
    return `<button type="button" class="choice service-choice" data-ui-action="${action}" ${attributes}>${icon(brand, cpa)}<span><strong>${name}</strong><small>${description}</small></span></button>`;
  }
  function group(name, cards) {
    return `<div class="service-choice-group"><h3>${name}</h3><div class="choice-grid">${cards}</div></div>`;
  }
  function open() {
    const native = getState().native_account;
    const existingClaude = (getState().providers || []).find(p => p.execution_backend === 'claude_cli' && p.auth_mode === 'claude_login');
    const codex = card('service-native', tr("services.current_sign_in"), 'codex', native?.credential_set ? tr("services.view_native_account") : tr("services.connect_local_codex"))
      + card('service-import-account', tr("services.import_subscription"), 'codex', tr("services.add_a_codex_subscription"));
    const claude = card('claude-local-choose', tr("services.local_subscription"), 'claudecode', tr("services.connect_local_claude_code"), `data-id="${esc(existingClaude?.id || '')}"`)
      + card('claude-cpa-choose', 'CPA', 'claudecode', tr("services.enter_address_and_api_key"), '', true);
    const apiKeys = ['openai', 'anthropic', ...Object.keys(presets).filter(key => !['chatgpt','openai','anthropic'].includes(key))];
    const api = apiKeys.filter(key => presets[key]).map(key => card('official-provider-choose', esc(presets[key].name), key, tr("services.connect_an_api_service"), `data-provider="${esc(key)}"`)).join('')
      + card('service-custom-api', tr("services.custom_api"), 'custom', tr("services.enter_address_and_api_key"));
    openModal(tr("add_provider"), group(tr("services.codex_subscriptions"), codex) + group('Claude Code', claude) + group('API', api), '', null, null, {reopen:open});
  }
  function importCodex(navigation = {}) {
    openModal(tr("services.import_codex_subscription"), `<div id="codex_import_choices" class="choice-grid">`
      + card('account-import-paste', tr("services.paste_contents"), 'codex', tr("services.paste_the_complete_auth_json_contents"))
      + card('account-import-file', tr("services.choose_a_file"), 'codex', tr("services.choose_an_auth_json_file")) + '</div>', '', null, null, {...navigation, reopen:() => importCodex(navigation)});
  }
  return {open, importCodex};
}
