function createSettings({getState, cloneState, persistState, openModal, api, notice, modules = null, $, esc, t, tr, quotaDisplay, help, contextHelp, openAppIcon, presentationOptions = () => '', collaborationHelp = () => ''}) {
  async function openDecisionConnection(back) {
    if (modules?.active('gateway') !== false) return modules.openOptions('gateway', back);
    const data = await api('/api/decisions/connection');
    openModal(tr('modules.decision_api'), `<div class="setting-row routing-mode"><label for="decision_local_key">${tr('modules.decision_key')}</label><div class="input-action">
      <input id="decision_local_key" type="password" value="${esc(data.api_key)}" readonly>
      <button type="button" class="secondary" data-copy-decision>${tr('modules.copy_key')}</button></div></div>`,
      '',null,null,{back});
    $('modal_body').querySelector('[data-copy-decision]').onclick = async () => {
      try { await navigator.clipboard.writeText(data.api_key); notice(tr('modules.key_copied')); }
      catch(error) { notice(error.message,true); }
    };
  }
  function openSettings(section) {
    const categories = [
      ['general', 'models.settings_general', 'models.settings_general_help'],
      ['external', 'models.settings_external', 'models.settings_external_help'],
      ['native', 'models.settings_native', 'models.settings_native_help']
    ];
    if (!section) {
      openModal(t('settings_menu'), `<div class="choice-grid settings-choices">${categories.map(([id, title, description]) => `<button type="button" class="choice" data-settings-category="${id}"><strong>${tr(title)}</strong><small>${tr(description)}</small></button>`).join('')}</div>`, '', null, null, {reopen:openSettings});
      $('modal_body').querySelectorAll('[data-settings-category]').forEach(button => button.onclick = () => openSettings(button.dataset.settingsCategory));
      return;
    }
    const reopen = () => openSettings(section);
    const state = getState();
    const toggle = (id, enabled, label, description) => `<div class="setting-row"><div class="setting-title collaboration-title"><span id="setting-${id}-label">${esc(tr(label))}</span>${help(tr(description))}</div><button type="button" class="setting-switch" role="switch" aria-checked="${enabled}" aria-labelledby="setting-${id}-label" data-ui-action="setting-toggle" data-id="${id}"></button></div>`;
    const quotaChoice = `<div class="setting-row"><div class="setting-title" id="quota-display-label">${tr("models.quota_display")}</div><div class="quota-display-choice" role="group" aria-labelledby="quota-display-label">${['remaining','used'].map(mode => `<button type="button" class="secondary" data-ui-action="quota-display" data-mode="${mode}" aria-pressed="${quotaDisplay.mode() === mode}">${mode === 'remaining' ? tr("core.remaining") : tr("core.used")}</button>`).join('')}</div></div>`;
    const backgroundEnabled = typeof state.background_compaction === 'boolean' ? state.background_compaction : state.codex_context_settings?.background_enabled === true;
    const background = `<div class="setting-row"><div><div class="setting-title collaboration-title"><span id="background-compaction-label">${tr('models.background_compaction')}</span>${help(tr('models.background_compaction_hint') + '\n\n' + contextHelp.background())}</div></div><button type="button" class="setting-switch" role="switch" aria-checked="${backgroundEnabled}" aria-labelledby="background-compaction-label" data-ui-action="setting-toggle" data-id="background_compaction"></button></div>`;
    const presentation = presentationOptions(quotaChoice) || quotaChoice;
    const mode = state.external_collaboration === 'off' ? 'off' : 'on';
    const collaboration = `<div class="setting-row collaboration-setting"><div><div class="setting-title collaboration-title"><span id="collaboration-label">${tr("models.external_collaboration")}</span>${collaborationHelp()}</div></div><div class="quota-display-choice" role="group" aria-labelledby="collaboration-label">${['off','on'].map(value => `<button type="button" class="secondary" data-ui-action="setting-toggle" data-id="external_collaboration" data-mode="${value}" aria-pressed="${mode === value}">${tr('models.collaboration_' + value)}</button>`).join('')}</div></div>`;
    const routing = `<div class="setting-row"><div class="setting-title">${tr("models.routing_settings")}</div><button type="button" class="secondary" data-icon="chevron-right" data-ui-action="routing-settings" aria-label="${tr("models.open_routing_settings")}"></button></div>`;
    const ultra = `<div class="setting-row"><div class="setting-title">${tr('models.ultra_actual_effort')}</div><button type="button" class="secondary" data-icon="chevron-right" data-ui-action="ultra-settings" aria-label="${tr('models.ultra_actual_effort')}"></button></div>`;
    const gateway = modules?.active('gateway') === false ? '' : `<div class="setting-row"><div class="setting-title">${tr('core.gateway_connection')}</div><button type="button" class="secondary" data-icon="chevron-right" data-gateway-open aria-label="${tr('core.gateway_connection')}"></button></div>`;
    const decision = modules?.active('decision') === false ? '' : `<div class="setting-row"><div><div class="setting-title">${tr('core.decision_models')}</div><p class="setting-help">${esc(state.providers.find(provider => provider.id === state.decision_provider)?.decision_api?.model || tr('core.decision_unconfigured'))}</p></div><button type="button" class="secondary" data-icon="chevron-right" data-decision-open aria-label="${tr('core.decision_models')}"></button></div>`;
    const quit = window.EMP_PREVIEW ? '' : `<div class="setting-row"><div class="setting-title">${t('quit_emp')}</div><button type="button" class="secondary" data-icon="chevron-right" data-quit-open aria-label="${t('quit_emp')}"></button></div>`;
    const rows = {
      general:presentation + `<div class="setting-row"><div class="setting-title">${tr('icons.title')}</div><button type="button" class="secondary" data-icon="chevron-right" data-app-icon-open aria-label="${tr('icons.title')}"></button></div>` + toggle('auto_enable_on_start', state.auto_enable_on_start !== false, 'models.start_emp_automatically', 'models.enable_emp_when_the_app_opens') + gateway + quit,
      external:collaboration + toggle('subscription_search', state.subscription_search?.enabled !== false, 'models.codex_web_search_for_external_models', 'models.use_a_codex_subscription_for_external_model_web_searches') + decision,
      native:background + ultra + routing
    };
    openModal(tr(categories.find(([id]) => id === section)[1]), `<div class="settings-list" data-settings-section="${section}">${rows[section]}</div>`, '', null, null, {back:openSettings, reopen});
    const decisionButton = $('modal_body').querySelector('[data-decision-open]');
    const iconButton = $('modal_body').querySelector('[data-app-icon-open]');
    if (iconButton) iconButton.onclick = () => openAppIcon(reopen);
    if (decisionButton) decisionButton.onclick = () => modules.openOptions('decision', reopen).catch(error => notice(error.message, true));
    const gatewayButton = $('modal_body').querySelector('[data-gateway-open]');
    const quitButton = $('modal_body').querySelector('[data-quit-open]');
    if (quitButton) quitButton.onclick = () => openModal(t('quit_emp'), `<p>${tr('core.quit_confirm')}</p>`, t('quit_emp'), async () => {
      await api('/api/quit', {method:'POST', body:'{}'});
      notice(tr('core.quitting'));
    }, null, {back:reopen});
    if (!gatewayButton) return;
    gatewayButton.onclick = async () => {
      gatewayButton.disabled = true;
      try {
        await modules.openOptions('gateway', reopen);
      } catch (error) { notice(error.message, true); }
      finally { gatewayButton.disabled = false; }
    };
  }
  async function toggleSetting(element) {
    if (element.disabled) return;
    const enabled = element.getAttribute('aria-checked') !== 'true';
    const candidate = cloneState(), key = element.dataset.id;
    if (key === 'subscription_search') candidate.subscription_search = {enabled, account_id:''};
    else if (key === 'auto_enable_on_start' || key === 'background_compaction') candidate[key] = enabled;
    else if (key === 'external_collaboration' && ['off','on'].includes(element.dataset.mode)) candidate[key] = element.dataset.mode;
    else return;
    const controls = [...$('modal_body').querySelectorAll('[data-ui-action="setting-toggle"]')].map(button => ({button,disabled:button.disabled}));
    controls.forEach(({button}) => button.disabled = true);
    try {
      await persistState(tr("models.settings_saved"), candidate);
      if (key === 'external_collaboration' && element.isConnected) {
        controls.filter(({button}) => button.dataset.id === key).forEach(({button}) => button.setAttribute('aria-pressed', String(button.dataset.mode === candidate[key])));
      } else element.setAttribute('aria-checked', String(enabled));
    } catch (error) { if (element.isConnected) $('modal_status').textContent = error.message; }
    finally { controls.forEach(({button,disabled}) => button.disabled = disabled); }
  }

  return {open:openSettings, toggle:toggleSetting, openDecisionConnection};
}
