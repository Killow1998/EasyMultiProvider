// A view of existing accounts/providers; storage and route identities stay with their owners.
function createServiceList({getState, $, esc, tr, presets, icon, activity, accountSummary, quotaMeters,
  refreshingAccounts, refreshErrors, quotaAnimationAccounts, updateActivityDots, openModal, api,
  usageSummary, notice, balances, poolQuotas, settingsConflict = () => false, serviceIcon = (kind,id,label,brand,cpa) => activity(kind,id,label)+icon(brand,cpa), updateServiceErrors = () => {}, onMount = () => {}}) {
  const refreshing = new Set(), errors = new Map();
  let detailsController = null;
  function stop() { detailsController?.abort(); detailsController = null; }
  function providerBrand(provider) {
    if (provider.execution_backend === 'claude_cli') return 'claudecode';
    if (provider.vertex) return 'gemini';
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
    const description = label ? `aria-label="${esc(label)}" title="${esc(label)}"` : '';
    return `<button type="button" class="secondary" ${glyph ? `data-icon="${glyph}"` : ''} data-ui-action="${name}" data-id="${esc(id)}" ${disabled ? 'disabled' : ''} ${description} ${extra}>${glyph ? '' : label}</button>`;
  }
  function accountRow(account) {
    const loginAction = account.native ? action('native-login',account.id,'','user',false,`aria-haspopup="dialog" aria-label="${esc(tr('login.change_method'))}" title="${esc(tr('login.change_method'))}"`) : '';
    if(account.native && ['local_key','missing','api_key','unavailable'].includes(account.auth_mode)) {
      const label=account.auth_mode==='local_key'?tr('login.local_key'):'Native';
      const summary=`<button type="button" class="account-summary service-summary" data-ui-action="account-details" data-id="@native" aria-haspopup="dialog" title="${esc(tr('login.mode_'+account.auth_mode))}"><span class="account-segment account-name"><span>${esc(label)}</span></span><span class="account-segment service-type">${tr(account.auth_mode==='local_key'?'login.local_connection':account.auth_mode==='api_key'?'login.api_key':'login.connection')}</span></button>`;
      return `<article class="entity-card service-card service-account" data-service="account:@native"><div class="service-main">${serviceIcon('account','@native',label,'codex',false)}<div class="service-identity">${summary}</div></div><div class="service-actions">${loginAction}</div></article>`;
    }
    const busy = refreshingAccounts.has(account.id), duplicate = account.duplicate && account.duplicate_of === '当前 Codex 登录';
    const label = account.native ? 'Native' : account.name || account.id;
    const status = !account.credential_set ? tr("services.not_signed_in") : account.credential_status === 'invalid' ? tr("services.sign_in_expired") : '';
    const duplicateText = account.duplicate ? (duplicate ? tr("services.models_managed_by_native") : tr("services.duplicate_account") + ' · ' + account.duplicate_of) : '';
    const detail = [duplicateText,settingsConflict(account)?tr("services.review_model_visibility_and_context_settings"):''].filter(Boolean).map(esc).join(' · ');
    const actions = loginAction
      + action('account-refresh', account.id, busy ? tr("services.refreshing") : tr("refresh"), 'refresh', busy || !account.credential_set)
      + action('account-quota-history', account.id, tr("services.trend"), 'chart')
      + action('account-edit', account.id, tr("models.manage_models"), 'settings', duplicate);
    return `<article class="entity-card service-card service-account${duplicate ? ' account-duplicate' : ''}" data-service="account:${esc(account.id)}">
      <div class="service-main">${serviceIcon('account', account.native ? '@native' : account.id, label, 'codex', false, refreshErrors[account.id] ? {title:tr("services.quota_refresh_failed"),message:refreshErrors[account.id]} : status ? {title:tr("services.sign_in_needs_attention"),message:status} : null)}<div class="service-identity">${accountSummary(account, account.quota?.account_label || label)}${detail ? `<small class="service-note">${detail}</small>` : ''}</div></div>
      <div class="service-actions">${actions}</div>
      <div class="service-quota">${quotaMeters(account, busy, quotaAnimationAccounts.has(account.id))}</div>
    </article>`;
  }
  function providerRow(provider, models) {
    const local = provider.execution_backend === 'claude_cli' && provider.auth_mode === 'claude_login';
    const cpa = provider.execution_backend === 'claude_cli' && !local;
    const label = provider.name || provider.id;
    const brand = providerBrand(provider);
    const defaults = [provider.id, brand, presets[brand]?.name, ...(local ? ['Claude Code','Claude Subscription'] : cpa ? ['CPA','Claude CPA','Claude Code CPA'] : [])];
    const alias = brand === 'custom' || !defaults.some(name => name && name.toLowerCase() === label.toLowerCase()) ? label : '';
    const summary = `<button type="button" class="account-summary service-summary" data-ui-action="provider-details" data-id="${esc(provider.id)}" aria-haspopup="dialog" aria-label="${esc(label + ' · ' + providerType(provider) + ' · Model: ' + models.length)}" title="${esc(label)}">${alias ? `<span class="account-segment account-name"><span>${esc(alias)}</span></span>` : ''}<span class="account-segment ${alias ? 'service-type' : 'account-name'}">${providerType(provider)}</span><span class="account-segment service-model-count">Model: ${models.length}</span>${balances.segment(provider.id)}</button>`;
    const modelActions = action('provider-model-settings', provider.id, tr("models.manage_models"), 'settings');
    const actions = local ? action('provider-quota-refresh', provider.id, refreshing.has(provider.id) ? tr("services.refreshing") : tr("refresh"), 'refresh', refreshing.has(provider.id))
      + action('provider-quota-history', provider.id, tr("services.trend"), 'chart')
      + modelActions
      : (poolQuotas.canRefresh(provider.id) && poolQuotas.canViewHistory(provider.id)?action('provider-quota-refresh',provider.id,tr('refresh'),'refresh',poolQuotas.busy.has(provider.id))
        + action('provider-quota-history',provider.id,tr('services.trend'),'chart'):'')
        + (balances.canRefresh(provider.id)?action('provider-balance-refresh',provider.id,tr('services.refresh_balance'),'refresh',balances.busy.has(provider.id)):'') + modelActions;
    return `<article class="entity-card service-card" data-service="provider:${esc(provider.id)}">
      <div class="service-main">${serviceIcon('provider', provider.id, label, brand, cpa, errors.has(provider.id) ? {title:tr("services.quota_refresh_failed"),message:errors.get(provider.id)} : null)}<div class="service-identity">${summary}</div></div>
      <div class="service-actions">${actions}</div>
      ${local ? `<div class="service-quota" title="${esc(getState().claude_quota?.observed_at ? tr("services.updated") + new Date(getState().claude_quota.observed_at * 1000).toLocaleString() : tr("services.quota_updates_after_model_use"))}">${quotaMeters({quota:getState().claude_quota, quota_pending:true}, refreshing.has(provider.id))}</div>` : ''}
      ${poolQuotas.markup(provider.id)}
    </article>`;
  }
  function mount(context) {
    const body = $('modal_body');
    body.closest('.modal').classList.toggle('service-detail-modal', Boolean(context));
    if (!context) return;
    const state = getState(), {id, kind} = context;
    const record = kind === 'account' ? [state.native_account, ...(state.accounts || [])].find(item => item?.id === id)
      : (state.providers || []).find(item => item.id === id);
    if (!record) return;
    $('modal_title').textContent = record.native ? 'Native' : record.name || id;
    onMount(id,kind,record);
  }
  function open(id, kind, active, body, label, submit, navigation = {}) {
    openModal('', body, label, submit, {id, kind, active}, navigation);
  }
  function render() {
    const state = getState(), box = $('services');
    const focused = box.contains(document.activeElement) ? document.activeElement?.dataset : null;
    const accounts = visibleAccounts();
    const groupedModels = new Map();
    for (const model of state.models || []) {
      if (!groupedModels.has(model.provider)) groupedModels.set(model.provider, []);
      groupedModels.get(model.provider).push(model);
    }
    const rows = accounts.map(accountRow).concat((state.providers || []).map(provider => providerRow(provider, groupedModels.get(provider.id) || [])));
    box.innerHTML = rows.join('') || `<p class="muted">${tr("services.add_a_service_to_start_using_models")}</p>`;
    quotaAnimationAccounts.clear();
    updateActivityDots(); updateServiceErrors();
    if (focused?.uiAction) {
      [...box.querySelectorAll('[data-ui-action]')].find(button => button.dataset.uiAction === focused.uiAction && button.dataset.id === focused.id)?.focus({preventScroll:true});
    }
  }
  function visibleAccounts() {
    const state=getState(),accounts=[...(state.native_account?[state.native_account]:[]),...(state.accounts||[])];
    return accounts.filter(account=>!account.duplicate||!accounts.some(owner=>owner.credential_set&&
      (account.duplicate_of==='当前 Codex 登录'?owner.native:owner.id===account.duplicate_of)));
  }
  async function details(id, navigation = {}) {
    const provider = (getState().providers || []).find(item => item.id === id);
    if (!provider) return;
    open(id, 'provider', 'details', `<div class="service-detail-actions"></div>${balances.details(id)}<div id="service_usage">${tr("services.loading_usage")}</div>`, '', null, navigation);
    const box = $('service_usage');
    const controller = new AbortController(); detailsController = controller;
    usageSummary.loading(box);
    try {
      const data = await api('/api/usage?lifetime=true&owner='+encodeURIComponent(id), {signal:controller.signal,onProgress:progress=>{if(!controller.signal.aborted && $('service_usage')===box)usageSummary.loading(box,progress);}});
      if (controller.signal.aborted || $('service_usage') !== box) return;
      const rows = (data.groups || []).filter(row => row.category === 'external' && row.owner === id);
      box.innerHTML = usageSummary.render({...data,groups:rows});
    } catch (error) { if (!controller.signal.aborted && $('service_usage') === box) usageSummary.failed(box,error,()=>details(id,navigation)); }
  }
  async function refresh(id, notify = true) {
    if(poolQuotas.has(id))return poolQuotas.refresh(id,notify);
    if (refreshing.has(id)) return false;
    refreshing.add(id); errors.delete(id); render();
    try {
      const result = await api('/api/providers/' + encodeURIComponent(id) + '/quota', {method:'POST', body:'{}'});
      getState().claude_quota = result.quota;
      if (result.history_saved === false) errors.set(id, tr("services.quota_updated_saving_trend_history_failed"));
      if (notify) notice(result.history_saved === false ? errors.get(id) : tr("services.quota_updated"), result.history_saved === false);
      return result.history_saved !== false;
    } catch (error) {
      const code = error.payload?.error?.code || error.message;
      const messages = {
        claude_quota_not_available:tr("services.claude_code_returned_no_subscription_quota"),
        claude_quota_unsupported:tr("services.update_claude_code_to_query_quota"),
        claude_cli_login_required:tr("services.sign_in_to_a_claude_code_subscription"),
        claude_cli_subscription_required:tr("services.claude_code_is_using_an_api_connection"),
        claude_cli_timeout:tr("services.quota_query_timed_out_try_again"),
      };
      errors.set(id, messages[code] || tr("services.quota_query_failed_try_again"));
      return false;
    } finally { refreshing.delete(id); render(); }
  }
  return {render, details, refresh, open, mount, stop, accounts:visibleAccounts};
}
