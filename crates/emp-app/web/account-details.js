// Owns account detail reads and alias interaction; routing and configuration stay outside.
function createAccountDetails({getState, $, tr, esc, api, services, serviceUsage, persistState,
  uiIcon, subscriptionPlanLabel, resetCreditCount}) {
  let current = null;
  function stop() { current?.controller.abort(); current = null; }
  function findAccount(id) { return [getState().native_account, ...(getState().accounts || [])].filter(Boolean).find(account => account.id === id); }
  async function openAccountDetails(id) {
    const account = findAccount(id); if (!account) return;
    const name = account.native ? 'Native' : account.name || account.id;
    const mail = account.quota?.account_label || '';
    const alias = account.native ? '<div class="account-alias-edit">Native</div>' : `<div class="account-alias-edit"><input id="account_alias" value="${esc(name)}" aria-label="${esc(tr('别名','Alias'))}"><button type="button" class="secondary" id="account_alias_save" data-ui-action="account-alias-save" data-id="${esc(id)}" title="${esc(tr('保存别名','Save alias'))}" aria-label="${esc(tr('保存别名','Save alias'))}">${uiIcon('save')}</button></div>`;
    const remove = account.native ? '' : `<button type="button" class="danger" data-icon="trash" data-ui-action="account-remove" data-id="${esc(id)}">${tr('移除','Remove')}</button>`;
    const credit = account.quota?.credits;
    const monthly = credit?.individual_limit?.remaining_percent;
    const reset = resetCreditCount(account) > 0 ? `<button type="button" class="secondary" data-icon="refresh" data-ui-action="account-reset-credits" data-id="${esc(id)}">${tr('选择重置','Choose reset')} · ${resetCreditCount(account)}</button>` : '';
    services.open(id, 'account', 'details', `<div class="account-info-grid"><div><span class="account-info-label">${tr('别名','Alias')}</span>${alias}</div><div><span class="account-info-label">${tr('账号','Account')}</span><div class="account-value-row"><div class="account-value">${esc(mail || id)}${mail ? `<small>${esc(id)}</small>` : ''}</div>${remove}</div></div><div><span class="account-info-label">${tr('登录信息','Sign-in')}</span><span class="account-value">${account.native ? tr('本机 Codex 登录','Local Codex sign-in') : tr('已导入的 Codex 登录信息','Imported Codex sign-in')} · ${account.credential_set ? tr('已保存','Saved') : tr('未检测到','Not detected')}</span></div><div><span class="account-info-label">${tr('订阅','Subscription')}</span><span class="account-value">${esc(subscriptionPlanLabel(account.quota?.plan_type) || '—')}</span></div></div>${typeof monthly === 'number' ? `<p class="muted">${tr('月额度','Monthly quota')} · ${esc(monthly)}%</p>` : ''}${credit?.spend_control_reached ? `<p class="refresh-error">${tr('已达消费上限','Spend limit reached')}</p>` : ''}<div id="account_usage" class="account-usage"><p class="muted">${tr('正在读取用量…','Loading usage…')}</p></div>${reset}`, '', null);
    current = {id, node:$('account_usage'), controller:new AbortController()};
    await refreshAccountDetailsUsage();
  }
  async function refreshAccountDetailsUsage() {
    const view = current; if (!view || $('account_usage') !== view.node) return;
    view.controller.abort(); view.controller = new AbortController();
    const controller = view.controller;
    try {
      const data = await api('/api/usage?account_id='+encodeURIComponent(view.id)+'&start=0&end='+Date.now()/1000, {signal:controller.signal});
      if (current === view && view.controller === controller && !controller.signal.aborted) view.node.innerHTML = serviceUsage.render(data);
    } catch (error) { if (current === view && view.controller === controller && !controller.signal.aborted) view.node.textContent = error.message; }
  }
  async function saveAccountAlias(element) {
    const candidate = structuredClone(getState()), account = candidate.accounts.find(item => item.id === element.dataset.id);
    if (!account) return;
    const name = $('account_alias').value.trim();
    if (!name) { $('modal_status').textContent = tr('请输入别名','Enter an alias'); return; }
    const node = $('account_alias');
    account.name = name; element.disabled = true;
    try { await persistState(tr('别名已保存','Alias saved'), candidate); if ($('account_alias') === node) { services.mount({id:element.dataset.id, kind:'account', active:'details'}); element.innerHTML = uiIcon('check'); } }
    catch (error) { if ($('account_alias') === node) $('modal_status').textContent = error.message; }
    finally { element.disabled = false; }
  }
  return {open:openAccountDetails, refresh:refreshAccountDetailsUsage, saveAlias:saveAccountAlias, stop};
}
