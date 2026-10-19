// Feature state is private; the page supplies current data and UI operations.
function createSettings({getState, cloneState, persistState, openModal, $, esc, t, tr, quotaDisplay, presentationOptions = () => '', collaborationHelp = () => ''}) {
  function openSettings() {
    const state = getState();
    const settings = [
      {id:'auto_enable_on_start', enabled:state.auto_enable_on_start !== false, label:tr("models.start_emp_automatically"), help:tr("models.enable_emp_when_the_app_opens")},
      {id:'encrypted_history_recovery', enabled:state.encrypted_history_recovery === true, label:tr("models.encrypted_history_recovery"), help:tr("models.encrypted_history_recovery_help")},
      {id:'subscription_search', enabled:state.subscription_search?.enabled !== false, label:tr("models.codex_web_search_for_external_models"), help:tr("models.use_a_codex_subscription_for_external_model_web_searches")}
    ];
    const quotaChoice = `<div class="setting-row"><div class="setting-title" id="quota-display-label">${tr("models.quota_display")}</div><div class="quota-display-choice" role="group" aria-labelledby="quota-display-label">${['remaining','used'].map(mode => `<button type="button" class="secondary" data-ui-action="quota-display" data-mode="${mode}" aria-pressed="${quotaDisplay.mode() === mode}">${mode === 'remaining' ? tr("core.remaining") : tr("core.used")}</button>`).join('')}</div></div>`;
    const presentation = presentationOptions(quotaChoice) || quotaChoice;
    const mode = state.external_collaboration || 'off';
    const collaboration = `<div class="setting-row collaboration-setting"><div><div class="setting-title collaboration-title"><span id="collaboration-label">${tr("models.external_collaboration")}</span>${collaborationHelp()}</div><p class="setting-help" id="collaboration-help">${esc(tr('models.collaboration_' + mode + '_help'))}</p></div><div class="quota-display-choice" role="group" aria-labelledby="collaboration-label" aria-describedby="collaboration-help">${['off','on_demand','on'].map(value => `<button type="button" class="secondary" data-ui-action="setting-toggle" data-id="external_collaboration" data-mode="${value}" aria-pressed="${mode === value}">${tr('models.collaboration_' + value)}</button>`).join('')}</div></div>`;
    openModal(t('settings_menu'), `<div class="settings-list">${presentation}${collaboration}${settings.map(setting => `<div class="setting-row"><div><div class="setting-title" id="setting-${setting.id}-label">${esc(setting.label)}</div><p class="setting-help" id="setting-${setting.id}-help" title="${esc(setting.help)}">${esc(setting.help)}</p></div><button type="button" class="setting-switch" role="switch" aria-checked="${setting.enabled}" aria-labelledby="setting-${setting.id}-label" aria-describedby="setting-${setting.id}-help" data-ui-action="setting-toggle" data-id="${setting.id}"></button></div>`).join('')}<div class="setting-row"><div class="setting-title">${tr("models.routing_settings")}</div><button type="button" class="secondary" data-ui-action="routing-settings" aria-label="${tr("models.open_routing_settings")}">›</button></div></div>`, '', null);
  }
  async function toggleSetting(element) {
    if (element.disabled) return;
    const enabled = element.getAttribute('aria-checked') !== 'true';
    const candidate = cloneState(), key = element.dataset.id;
    if (key === 'subscription_search') candidate.subscription_search = {enabled, account_id:''};
    else if (key === 'auto_enable_on_start' || key === 'encrypted_history_recovery') candidate[key] = enabled;
    else if (key === 'external_collaboration' && ['off','on_demand','on'].includes(element.dataset.mode)) candidate[key] = element.dataset.mode;
    else return;
    const controls = [...$('modal_body').querySelectorAll('[data-ui-action="setting-toggle"]')].map(button => ({button,disabled:button.disabled}));
    controls.forEach(({button}) => button.disabled = true);
    try {
      await persistState(tr("models.settings_saved"), candidate);
      if (key === 'external_collaboration' && element.isConnected) {
        controls.filter(({button}) => button.dataset.id === key).forEach(({button}) => button.setAttribute('aria-pressed', String(button.dataset.mode === candidate[key])));
        $('collaboration-help').textContent = tr('models.collaboration_' + candidate[key] + '_help');
      } else element.setAttribute('aria-checked', String(enabled));
    } catch (error) { if (element.isConnected) $('modal_status').textContent = error.message; }
    finally { controls.forEach(({button,disabled}) => button.disabled = disabled); }
  }

  return {open:openSettings, toggle:toggleSetting};
}
