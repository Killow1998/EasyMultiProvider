// A view of existing accounts/providers; storage and route identities stay with their owners.
function createServiceList({getState, $, esc, tr, presets, icon, activity, accountSummary, quotaMeters,
  refreshingAccounts, refreshErrors, quotaAnimationAccounts, updateActivityDots, openModal, api,
  usageSummary, notice, serviceIcon = (kind,id,label,brand,cpa) => activity(kind,id,label)+icon(brand,cpa), updateServiceErrors = () => {}}) {
  const refreshing = new Set(), errors = new Map();
  let detailsController = null;
  function stop() { detailsController?.abort(); detailsController = null; }
  function providerBrand(provider) {
    if (provider.execution_backend === 'claude_cli') return 'claudecode';
    try {
      const origin = new URL(provider.base_url).origin;
      return Object.entries(presets).find(([key, preset]) => key !== 'chatgpt' && new URL(preset.base_url).origin === origin)?.[0] || 'custom';
    } catch (_) { return 'custom'; }
  }
  function providerType(provider) {
    if (provider.execution_backend !== 'claude_cli') return 'API';
    return provider.auth_mode === 'claude_login' ? 'Native' : 'CPA';
  }
  function action(name, id, label, glyph, disabled = false, extra = '') {
    return `<button type="button" class="secondary" ${glyph ? `data-icon="${glyph}"` : ''} data-ui-action="${name}" data-id="${esc(id)}" ${disabled ? 'disabled' : ''} ${extra}>${label}</button>`;
  }
  function accountRow(account) {
    const busy = refreshingAccounts.has(account.id), duplicate = account.duplicate && account.duplicate_of === '当前 Codex 登录';
    const label = account.native ? 'Native' : account.name || account.id;
    const status = !account.credential_set ? tr('未登录','Not signed in') : account.credential_status === 'invalid' ? tr('登录已失效','Sign-in expired') : '';
    const duplicateText = account.duplicate ? (duplicate ? tr('模型由 Native 管理','Models managed by Native') : tr('重复账号','Duplicate account') + ' · ' + account.duplicate_of) : '';
    const detail = [duplicateText].filter(Boolean).map(esc).join(' · ');
    const actions = action('account-refresh', account.id, busy ? tr('刷新中…','Refreshing…') : tr('刷新','Refresh'), 'refresh', busy || !account.credential_set)
      + action('account-quota-history', account.id, tr('趋势','Trend'), 'chart');
    return `<article class="entity-card service-card service-account${duplicate ? ' account-duplicate' : ''}" data-service="account:${esc(account.id)}">
      <div class="service-main">${serviceIcon('account', account.native ? '@native' : account.id, label, 'codex', false, refreshErrors[account.id] ? {title:tr('额度刷新失败','Quota refresh failed'),message:refreshErrors[account.id]} : status ? {title:tr('登录需要处理','Sign-in needs attention'),message:status} : null)}<div class="service-identity">${accountSummary(account, account.quota?.account_label || label)}${detail ? `<small class="service-note">${detail}</small>` : ''}</div></div>
      <div class="service-actions">${actions}</div>
      <div class="service-quota">${quotaMeters(account, busy, quotaAnimationAccounts.has(account.id))}</div>
    </article>`;
  }
  function providerRow(provider, models) {
    const local = provider.execution_backend === 'claude_cli' && provider.auth_mode === 'claude_login';
    const cpa = provider.execution_backend === 'claude_cli' && !local;
    const hidden = models.length > 0 && models.every(model => !model.enabled);
    const label = provider.name || provider.id;
    const brand = providerBrand(provider);
    const defaults = [provider.id, brand, presets[brand]?.name, ...(local ? ['Claude Code','Claude Subscription'] : cpa ? ['Claude CPA','Claude Code CPA'] : [])];
    const alias = brand === 'custom' || !defaults.some(name => name && name.toLowerCase() === label.toLowerCase()) ? label : '';
    const summary = `<button type="button" class="account-summary service-summary" data-ui-action="provider-details" data-id="${esc(provider.id)}" aria-haspopup="dialog" aria-label="${esc(label + ' · ' + providerType(provider) + ' · Model: ' + models.length)}" title="${esc(label)}">${alias ? `<span class="account-segment account-name"><span>${esc(alias)}</span></span>` : ''}<span class="account-segment ${alias ? 'service-type' : 'account-name'}">${providerType(provider)}</span><span class="account-segment service-model-count">Model: ${models.length}</span></button>`;
    const modelActions = action('provider-model-settings', provider.id, tr('模型设置','Model settings'), 'settings')
      + action('provider-toggle-models', provider.id, hidden ? tr('显示模型','Show models') : tr('隐藏模型','Hide models'), '', !models.length, `data-all-hidden="${hidden}"`);
    const actions = local ? modelActions
      + action('provider-quota-refresh', provider.id, refreshing.has(provider.id) ? tr('刷新中…','Refreshing…') : tr('刷新','Refresh'), 'refresh', refreshing.has(provider.id))
      + action('provider-quota-history', provider.id, tr('趋势','Trend'), 'chart')
      : modelActions;
    return `<article class="entity-card service-card" data-service="provider:${esc(provider.id)}">
      <div class="service-main">${serviceIcon('provider', provider.id, label, brand, cpa, errors.has(provider.id) ? {title:tr('额度刷新失败','Quota refresh failed'),message:errors.get(provider.id)} : null)}<div class="service-identity">${summary}</div></div>
      <div class="service-actions${local ? ' service-actions-with-quota' : ''}">${actions}</div>
      ${local ? `<div class="service-quota" title="${esc(getState().claude_quota?.observed_at ? tr('更新于 ', 'Updated ') + new Date(getState().claude_quota.observed_at * 1000).toLocaleString() : tr('使用模型后更新额度', 'Quota updates after model use'))}">${quotaMeters({quota:getState().claude_quota, quota_pending:true}, refreshing.has(provider.id))}</div>` : ''}
    </article>`;
  }
  function tabs(id, kind, active = 'details', disabled = false) {
    const views = kind === 'account'
      ? [['details', 'account-details', tr('账号信息','Account details')], ['edit', 'account-edit', tr('模型设置','Model settings')]]
      : [['details', 'provider-details', tr('用量','Usage')], ['edit', 'provider-edit', tr('连接设置','Connection settings')]];
    return `<div class="service-detail-tabs" role="tablist" aria-label="${tr('服务设置','Service settings')}">${views.map(([view, action, label]) => `<button id="service-tab-${view}" type="button" class="secondary" role="tab" data-ui-action="${action}" data-id="${esc(id)}" aria-selected="${view === active}" aria-controls="modal_body" tabindex="${view === active ? 0 : -1}" ${view === 'edit' && disabled ? 'disabled' : ''}>${label}</button>`).join('')}</div>`;
  }
  function mount(context) {
    const nav = $('service_detail_nav'), body = $('modal_body');
    if (!nav || !body) return;
    body.closest('.modal').classList.toggle('service-detail-modal', Boolean(context));
    nav.hidden = !context;
    if (!context) { nav.innerHTML = ''; delete nav.dataset.service; nav.onkeydown = null; body.removeAttribute('role'); body.removeAttribute('aria-labelledby'); return; }
    const state = getState(), {id, kind, active} = context;
    const record = kind === 'account' ? [state.native_account, ...(state.accounts || [])].find(item => item?.id === id)
      : (state.providers || []).find(item => item.id === id);
    if (!record) return;
    $('modal_title').textContent = record.native ? 'Native' : record.name || id;
    const key = kind + ':' + id;
    if (nav.dataset.service !== key) {
      nav.dataset.service = key;
      nav.innerHTML = tabs(id, kind, active, record.duplicate && record.duplicate_of === '当前 Codex 登录');
    }
    for (const button of nav.querySelectorAll('[role="tab"]')) {
      const selected = button.dataset.uiAction.endsWith(active === 'edit' ? '-edit' : '-details');
      button.setAttribute('aria-selected', String(selected));
      button.tabIndex = selected ? 0 : -1;
    }
    nav.onkeydown = event => {
      const buttons = [...nav.querySelectorAll('[role="tab"]')].filter(button => !button.disabled);
      const index = buttons.indexOf(event.target);
      if (index < 0 || !['ArrowLeft','ArrowRight','Home','End'].includes(event.key)) return;
      event.preventDefault();
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1
        : (index + (event.key === 'ArrowRight' ? 1 : -1) + buttons.length) % buttons.length;
      buttons[next].focus(); buttons[next].click();
    };
    body.setAttribute('role', 'tabpanel');
    body.setAttribute('aria-labelledby', 'service-tab-' + active);
  }
  function open(id, kind, active, body, label, submit) {
    openModal('', body, label, submit, {id, kind, active});
  }
  function render() {
    const state = getState(), box = $('services');
    const focused = box.contains(document.activeElement) ? document.activeElement?.dataset : null;
    const accounts = [...(state.native_account ? [state.native_account] : []), ...(state.accounts || [])];
    const groupedModels = new Map();
    for (const model of state.models || []) {
      if (!groupedModels.has(model.provider)) groupedModels.set(model.provider, []);
      groupedModels.get(model.provider).push(model);
    }
    const rows = accounts.map(accountRow).concat((state.providers || []).map(provider => providerRow(provider, groupedModels.get(provider.id) || [])));
    box.innerHTML = rows.join('') || `<p class="muted">${tr('添加服务，开始使用模型。','Add a service to start using models.')}</p>`;
    quotaAnimationAccounts.clear();
    updateActivityDots(); updateServiceErrors();
    if (focused?.uiAction) {
      [...box.querySelectorAll('[data-ui-action]')].find(button => button.dataset.uiAction === focused.uiAction && button.dataset.id === focused.id)?.focus({preventScroll:true});
    }
  }
  async function details(id) {
    const provider = (getState().providers || []).find(item => item.id === id);
    if (!provider) return;
    open(id, 'provider', 'details', `<div class="service-detail-actions"><button type="button" class="danger" data-icon="trash" data-ui-action="provider-remove" data-id="${esc(id)}">${tr('移除','Remove')}</button></div>
      <div id="service_usage">${tr('正在读取用量…','Loading usage…')}</div>`, '', null);
    const box = $('service_usage');
    const controller = new AbortController(); detailsController = controller;
    try {
      const data = await api('/api/usage?category=external&start=0&end='+Date.now()/1000, {signal:controller.signal});
      if (controller.signal.aborted || $('service_usage') !== box) return;
      const rows = (data.groups || []).filter(row => row.category === 'external' && row.owner === id);
      box.innerHTML = usageSummary.render({groups:rows});
    } catch (error) { if (!controller.signal.aborted && $('service_usage') === box) box.textContent = error.message; }
  }
  async function refresh(id, notify = true) {
    if (refreshing.has(id)) return false;
    refreshing.add(id); errors.delete(id); render();
    try {
      const result = await api('/api/providers/' + encodeURIComponent(id) + '/quota', {method:'POST', body:'{}'});
      getState().claude_quota = result.quota;
      if (result.history_saved === false) errors.set(id, tr('额度已更新，趋势记录保存失败。','Quota updated; saving trend history failed.'));
      if (notify) notice(result.history_saved === false ? errors.get(id) : tr('额度已更新','Quota updated'), result.history_saved === false);
      return result.history_saved !== false;
    } catch (error) {
      const code = error.payload?.error?.code || error.message;
      const messages = {
        claude_quota_not_available:tr('Claude Code 未返回订阅额度。','Claude Code returned no subscription quota.'),
        claude_quota_unsupported:tr('更新 Claude Code 后可查询额度。','Update Claude Code to query quota.'),
        claude_cli_login_required:tr('请先登录 Claude Code 订阅。','Sign in to a Claude Code subscription.'),
        claude_cli_subscription_required:tr('当前 Claude Code 使用 API 连接。','Claude Code is using an API connection.'),
        claude_cli_timeout:tr('额度查询超时，请重试。','Quota query timed out. Try again.'),
      };
      errors.set(id, messages[code] || tr('额度查询失败，请重试。','Quota query failed. Try again.'));
      return false;
    } finally { refreshing.delete(id); render(); }
  }
  return {render, details, refresh, tabs, open, mount, stop};
}
