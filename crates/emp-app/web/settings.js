// Feature state is private; the page supplies current data and UI operations.
function createSettings({getState, cloneState, persistState, openModal, $, esc, t, tr, quotaDisplay, presentationOptions = () => ''}) {
  function openSettings() {
    const state = getState();
    const settings = [
      {id:'auto_enable_on_start', enabled:state.auto_enable_on_start !== false, label:tr("models.start_emp_automatically"), help:tr("models.enable_emp_when_the_app_opens")},
      {id:'subscription_search', enabled:state.subscription_search?.enabled !== false, label:tr("models.codex_web_search_for_external_models"), help:tr("models.use_a_codex_subscription_for_external_model_web_searches")}
    ];
    const quotaChoice = `<div class="setting-row"><div class="setting-title" id="quota-display-label">${tr("models.quota_display")}</div><div class="quota-display-choice" role="group" aria-labelledby="quota-display-label">${['remaining','used'].map(mode => `<button type="button" class="secondary" data-ui-action="quota-display" data-mode="${mode}" aria-pressed="${quotaDisplay.mode() === mode}">${mode === 'remaining' ? tr("core.remaining") : tr("core.used")}</button>`).join('')}</div></div>`;
    const presentation = presentationOptions(quotaChoice) || quotaChoice;
    openModal(t('settings_menu'), `<div class="settings-list">${presentation}${settings.map(setting => `<div class="setting-row"><div><div class="setting-title" id="setting-${setting.id}-label">${esc(setting.label)}</div><p class="setting-help" id="setting-${setting.id}-help" title="${esc(setting.help)}">${esc(setting.help)}</p></div><button type="button" class="setting-switch" role="switch" aria-checked="${setting.enabled}" aria-labelledby="setting-${setting.id}-label" aria-describedby="setting-${setting.id}-help" data-ui-action="setting-toggle" data-id="${setting.id}"></button></div>`).join('')}<div class="setting-row"><div class="setting-title">${tr("models.routing_settings")}</div><button type="button" class="secondary" data-ui-action="routing-settings" aria-label="${tr("models.open_routing_settings")}">›</button></div></div>`, '', null);
  }
  async function toggleSetting(element) {
    if (element.disabled) return;
    const enabled = element.getAttribute('aria-checked') !== 'true';
    const candidate = cloneState(), key = element.dataset.id;
    if (key === 'subscription_search') candidate.subscription_search = {enabled, account_id:''};
    else if (key === 'auto_enable_on_start') candidate[key] = enabled;
    else return;
    const controls = [...$('modal_body').querySelectorAll('.setting-switch')].map(button => ({button,disabled:button.disabled}));
    controls.forEach(({button}) => button.disabled = true);
    try {
      await persistState(tr("models.settings_saved"), candidate);
      element.setAttribute('aria-checked', String(enabled));
    } catch (error) { if (element.isConnected) $('modal_status').textContent = error.message; }
    finally { controls.forEach(({button,disabled}) => button.disabled = disabled); }
  }

  return {open:openSettings, toggle:toggleSetting};
}
