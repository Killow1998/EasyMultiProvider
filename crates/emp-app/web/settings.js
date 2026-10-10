// Feature state is private; the page supplies current data and UI operations.
function createSettings({getState, cloneState, persistState, openModal, $, esc, t, tr, quotaDisplay, presentationOptions = () => ''}) {
  function openSettings() {
    const state = getState();
    const available = (state.accounts || []).some(account => account.enabled !== false && account.credential_set && account.credential_status !== 'invalid' && !account.duplicate);
    const settings = [
      {id:'auto_enable_on_start', enabled:state.auto_enable_on_start !== false, label:tr('打开 EMP 后自动启动','Start EMP automatically'), help:tr('打开应用后直接启用 EMP。','Enable EMP when the app opens.')},
      {id:'subscription_search', enabled:state.subscription_search?.enabled !== false, label:tr('外部模型使用 Codex 联网搜索','Codex web search for external models'), help:tr('外部模型搜索时，调用 Codex 订阅账号的联网搜索。','Use a Codex subscription for external model web searches.')},
      {id:'auto_review_fallback', enabled:state.auto_review_fallback !== false, disabled:!available, label:tr('自动审查路由','Auto-review routing'), help:tr('原生订阅无额度时，自动审查路由至其他订阅账号。','Route auto-review to another subscription when Native runs out of quota.')}
    ];
    const quotaChoice = `<div class="setting-row"><div class="setting-title" id="quota-display-label">${tr('额度显示','Quota display')}</div><div class="quota-display-choice" role="group" aria-labelledby="quota-display-label">${['remaining','used'].map(mode => `<button type="button" class="secondary" data-ui-action="quota-display" data-mode="${mode}" aria-pressed="${quotaDisplay.mode() === mode}">${mode === 'remaining' ? tr('剩余','Remaining') : tr('已用','Used')}</button>`).join('')}</div></div>`;
    openModal(t('settings_menu'), `<div class="settings-list">${presentationOptions()}${settings.map(setting => `<div class="setting-row"><div><div class="setting-title" id="setting-${setting.id}-label">${esc(setting.label)}</div><p class="setting-help" id="setting-${setting.id}-help" title="${esc(setting.help)}">${esc(setting.help)}</p></div><button type="button" class="setting-switch" role="switch" aria-checked="${setting.enabled}" aria-labelledby="setting-${setting.id}-label" aria-describedby="setting-${setting.id}-help" data-ui-action="setting-toggle" data-id="${setting.id}" ${setting.disabled ? 'disabled' : ''}></button></div>`).join('')}${quotaChoice}</div>`, '', null);
  }
  async function toggleSetting(element) {
    if (element.disabled) return;
    const enabled = element.getAttribute('aria-checked') !== 'true';
    const candidate = cloneState(), key = element.dataset.id;
    if (key === 'subscription_search') candidate.subscription_search = {enabled, account_id:''};
    else if (['auto_enable_on_start','auto_review_fallback'].includes(key)) candidate[key] = enabled;
    else return;
    const controls = [...$('modal_body').querySelectorAll('.setting-switch')].map(button => ({button,disabled:button.disabled}));
    controls.forEach(({button}) => button.disabled = true);
    try {
      await persistState(tr('设置已保存','Settings saved'), candidate);
      element.setAttribute('aria-checked', String(enabled));
    } catch (error) { if (element.isConnected) $('modal_status').textContent = error.message; }
    finally { controls.forEach(({button,disabled}) => button.disabled = disabled); }
  }

  return {open:openSettings, toggle:toggleSetting};
}
