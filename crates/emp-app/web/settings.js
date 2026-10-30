// Feature state is private; the page supplies current data and UI operations.
function createSettings({getState, cloneState, persistState, openModal, api, notice, $, esc, t, tr, quotaDisplay, help, contextHelp, presentationOptions = () => '', collaborationHelp = () => ''}) {
  function openSettings() {
    const state = getState();
    const toggle = (id, enabled, label, description) => `<div class="setting-row"><div><div class="setting-title" id="setting-${id}-label">${esc(tr(label))}</div><p class="setting-help" id="setting-${id}-help" title="${esc(tr(description))}">${esc(tr(description))}</p></div><button type="button" class="setting-switch" role="switch" aria-checked="${enabled}" aria-labelledby="setting-${id}-label" aria-describedby="setting-${id}-help" data-ui-action="setting-toggle" data-id="${id}"></button></div>`;
    const quotaChoice = `<div class="setting-row"><div class="setting-title" id="quota-display-label">${tr("models.quota_display")}</div><div class="quota-display-choice" role="group" aria-labelledby="quota-display-label">${['remaining','used'].map(mode => `<button type="button" class="secondary" data-ui-action="quota-display" data-mode="${mode}" aria-pressed="${quotaDisplay.mode() === mode}">${mode === 'remaining' ? tr("core.remaining") : tr("core.used")}</button>`).join('')}</div></div>`;
    const backgroundEnabled = typeof state.background_compaction === 'boolean' ? state.background_compaction : state.codex_context_settings?.background_enabled === true;
    const background = `<div class="setting-row"><div><div class="setting-title collaboration-title"><span id="background-compaction-label">${tr('models.background_compaction')}</span>${help(contextHelp.background())}</div><p class="setting-help">${tr('models.background_compaction_hint')}</p></div><button type="button" class="setting-switch" role="switch" aria-checked="${backgroundEnabled}" aria-labelledby="background-compaction-label" data-ui-action="setting-toggle" data-id="background_compaction"></button></div>`;
    const presentation = presentationOptions(quotaChoice) || quotaChoice;
    const mode = state.external_collaboration || 'off';
    const collaboration = `<div class="setting-row collaboration-setting"><div><div class="setting-title collaboration-title"><span id="collaboration-label">${tr("models.external_collaboration")}</span>${collaborationHelp()}</div><p class="setting-help" id="collaboration-help">${esc(tr('models.collaboration_' + mode + '_help'))}</p></div><div class="quota-display-choice" role="group" aria-labelledby="collaboration-label" aria-describedby="collaboration-help">${['off','on_demand','on'].map(value => `<button type="button" class="secondary" data-ui-action="setting-toggle" data-id="external_collaboration" data-mode="${value}" aria-pressed="${mode === value}">${tr('models.collaboration_' + value)}</button>`).join('')}</div></div>`;
    const routing = `<div class="setting-row"><div class="setting-title">${tr("models.routing_settings")}</div><button type="button" class="secondary" data-ui-action="routing-settings" aria-label="${tr("models.open_routing_settings")}">›</button></div>`;
    const ultra = `<div class="setting-row"><div class="setting-title">${tr('models.ultra_actual_effort')}</div><button type="button" class="secondary" data-ui-action="ultra-settings" aria-label="${tr('models.ultra_actual_effort')}">›</button></div>`;
    const gateway = `<div class="setting-row"><div class="setting-title">${tr('core.gateway_connection')}</div><button type="button" class="secondary" data-gateway-open aria-label="${tr('core.gateway_connection')}">›</button></div>`;
    const groups = [
      ['display', 'models.settings_display', presentation],
      ['general', 'models.settings_general', toggle('auto_enable_on_start', state.auto_enable_on_start !== false, 'models.start_emp_automatically', 'models.enable_emp_when_the_app_opens') + collaboration + gateway],
      ['external', 'models.settings_external', toggle('subscription_search', state.subscription_search?.enabled !== false, 'models.codex_web_search_for_external_models', 'models.use_a_codex_subscription_for_external_model_web_searches')],
      ['native', 'models.settings_native', background + ultra + routing]
    ];
    openModal(t('settings_menu'), `<div class="settings-list">${groups.map(([id, title, rows]) => `<div class="settings-group" role="region" aria-labelledby="settings-${id}-title"><h3 class="settings-group-title" id="settings-${id}-title">${tr(title)}</h3>${rows}</div>`).join('')}</div>`, '', null);
    const gatewayButton = $('modal_body').querySelector('[data-gateway-open]');
    gatewayButton.onclick = async () => {
      gatewayButton.disabled = true;
      try {
        const connection = await api('/api/gateway');
        if (!gatewayButton.isConnected) return;
        openModal(tr('core.gateway_connection'), `<p class="muted">${tr('core.gateway_hint')}</p><label>${tr('core.gateway_base_url')}<input readonly value="${esc(location.origin + '/v1')}"></label><label>${tr('core.gateway_key')}<input id="gateway_key" type="password" autocomplete="off" readonly value="${esc(connection.api_key)}"></label><button type="button" class="secondary" data-gateway-copy>${tr('core.gateway_copy_key')}</button><p class="muted">${tr('core.gateway_forwarding_hint')}</p><p class="muted">${tr('core.gateway_jev_hint')}</p>`, '', null, null, {back:openSettings});
        $('modal_body').querySelector('[data-gateway-copy]').onclick = () => navigator.clipboard.writeText($('gateway_key').value).then(() => notice(tr('core.gateway_key_copied')), error => notice(error.message, true));
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
