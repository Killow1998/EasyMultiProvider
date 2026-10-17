// Owns account detail reads and alias interaction; routing and configuration stay outside.
function createAccountDetails({getState, $, tr, esc, api, services, serviceUsage, persistState,
  uiIcon, subscriptionPlanLabel, resetCreditCount, renderInfo, confirmRemoval, notice,
  onRemoved, closeModal, syncSavedCatalog}) {
  let current = null;
  const removing = new Set();
  function stop() { current?.controller.abort(); current = null; }
  function findAccount(id) { return [getState().native_account, ...(getState().accounts || [])].filter(Boolean).find(account => account.id === id); }
  async function openAccountDetails(id, navigation = {}) {
    const account = findAccount(id); if (!account) return;
    const name = account.native ? 'Native' : account.name || account.id;
    const mail = account.quota?.account_label || '';
    const alias = account.native ? `<div class="account-alias-edit">${tr("common.native")}</div>` : `<div class="account-alias-edit"><input id="account_alias" value="${esc(name)}" aria-label="${esc(tr("services.name"))}"><button type="button" class="secondary" id="account_alias_save" data-ui-action="account-alias-save" data-id="${esc(id)}" title="${esc(tr("services.save_name"))}" aria-label="${esc(tr("services.save_name"))}">${uiIcon('save')}</button></div>`;
    const remove = account.native ? '' : `<button type="button" class="danger" data-icon="trash" data-ui-action="account-remove" data-id="${esc(id)}">${tr("services.remove")}</button>`;
    const credit = account.quota?.credits;
    const monthly = credit?.individual_limit?.remaining_percent;
    const reset = resetCreditCount(account) > 0 ? `<button type="button" class="secondary" data-ui-action="account-reset-credits" data-id="${esc(id)}">${uiIcon('reset')} ${tr("services.choose_reset")} · ${resetCreditCount(account)}</button>` : '';
    const info = {alias,remove,monthly,credit,reset};
    services.open(id, 'account', 'details', renderInfo ? renderInfo(account,info) : `<div class="account-info-grid"><div><span class="account-info-label">${tr("services.name")}</span>${alias}</div><div><span class="account-info-label">${tr("services.account")}</span><div class="account-value-row"><div class="account-value">${esc(mail || id)}${mail ? `<small>${esc(id)}</small>` : ''}</div>${remove}</div></div><div><span class="account-info-label">${tr("services.sign_in")}</span><span class="account-value">${account.native ? tr("services.local_codex_sign_in") : tr("services.imported_codex_sign_in")} · ${account.credential_set ? tr("services.saved") : tr("services.not_detected")}</span></div><div><span class="account-info-label">${tr("services.subscription")}</span><span class="account-value">${esc(subscriptionPlanLabel(account.quota?.plan_type) || '—')}</span></div></div>${typeof monthly === 'number' ? `<p class="muted">${tr("services.monthly_quota")} · ${esc(monthly)}%</p>` : ''}${credit?.spend_control_reached ? `<p class="refresh-error">${tr("services.spend_limit_reached")}</p>` : ''}<div id="account_usage" class="account-usage"><p class="muted">${tr("services.loading_usage")}</p></div>${reset}`, '', null, navigation);
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
    if (!name) { $('modal_status').textContent = tr("services.enter_a_name"); return; }
    const node = $('account_alias');
    account.name = name; element.disabled = true;
    try { await persistState(tr("services.name_saved"), candidate); if ($('account_alias') === node) { services.mount({id:element.dataset.id, kind:'account', active:'details'}); element.innerHTML = uiIcon('check'); } }
    catch (error) { if ($('account_alias') === node) $('modal_status').textContent = error.message; }
    finally { element.disabled = false; }
  }
  async function remove(id, button) {
    const account=findAccount(id);
    if(!account || account.native || removing.has(id) || !confirmRemoval(tr("services.remove_account", {name:account.name || id})))return;
    const owner=current, label=button?.textContent;
    removing.add(id);
    if(button){button.disabled=true;button.textContent=tr("services.removing");}
    try {
      await api('/api/accounts/'+encodeURIComponent(id),{method:'DELETE'});
      onRemoved(await api('/api/config'));
      if(current===owner && owner?.id===id)closeModal();
      notice(tr("services.account_removed"));
      void syncSavedCatalog();
    } catch(error) {notice(error.message,true);}
    finally {
      removing.delete(id);
      if(button?.isConnected){button.disabled=false;button.textContent=label;}
    }
  }
  return {open:openAccountDetails, refresh:refreshAccountDetailsUsage, saveAlias:saveAccountAlias, remove, stop};
}
