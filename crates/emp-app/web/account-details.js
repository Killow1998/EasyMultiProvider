// Owns account detail reads and alias interaction; routing and configuration stay outside.
function createAccountDetails({getState, $, tr, esc, api, services, serviceUsage, persistState,
  uiIcon, renderInfo, confirmRemoval, notice,
  onRemoved, closeModal, syncSavedCatalog}) {
  let current = null;
  const removing = new Set();
  const usage = new Map(), maxAge = 60_000;
  let revision = 0;
  // Leaving a tab detaches its view, not the account's shared read.
  function stop() { current = null; }
  function findAccount(id) { return [getState().native_account, ...(getState().accounts || [])].filter(Boolean).find(account => account.id === id); }
  const identity = account => JSON.stringify([account?.auth_mode, account?.credential_set, account?.credential_status, account?.quota?.account_label]);
  function discard(id) { usage.get(id)?.controller?.abort(); usage.delete(id); }
  function entryFor(id) {
    for (const [key, entry] of usage) {
      const account = findAccount(key);
      if (!account || entry.identity !== identity(account)) discard(key);
    }
    if (!findAccount(id)) return null;
    if (!usage.has(id)) usage.set(id, {id, identity:identity(findAccount(id)), data:null, at:0, revision:-1, pending:null});
    return usage.get(id);
  }
  function ownsView(entry) {
    return current?.entry === entry && $('account_usage') === current.node && usage.get(entry.id) === entry && entry.identity === identity(findAccount(entry.id));
  }
  function paintUsage(entry) {
    if (ownsView(entry) && entry.data) current.node.innerHTML = serviceUsage.render(entry.data);
  }
  function readUsage(entry) {
    if (entry.pending) return entry.pending;
    const startedRevision = revision;
    entry.controller = new AbortController();
    entry.pending = (async () => {
      let succeeded = false;
      try {
        const data = await api('/api/usage?account_id='+encodeURIComponent(entry.id)+'&start=0&end='+Date.now()/1000, {signal:entry.controller.signal});
        if (usage.get(entry.id) !== entry || entry.identity !== identity(findAccount(entry.id))) return;
        // The lifetime view needs aggregates, not the report's minute-by-minute series.
        entry.data = {groups:data.groups, first_record_at:data.first_record_at};
        entry.at = Date.now(); entry.revision = startedRevision; succeeded = true;
        paintUsage(entry);
      } catch (error) {
        if ([401,403,404].includes(error.status)) entry.data = null;
        if (ownsView(entry)) {
          current.node.innerHTML = (entry.data ? serviceUsage.render(entry.data) : '') + `<p class="refresh-error" role="status">${esc(error.message)}</p>`;
        }
      } finally {
        entry.pending = null;
        // A usage event during the read needs one follow-up, never a cancelled restart.
        if (succeeded && ownsView(entry) && startedRevision !== revision) void readUsage(entry);
      }
    })();
    return entry.pending;
  }
  function showUsage() {
    if (!current || $('account_usage') !== current.node) return;
    const entry = entryFor(current.id);
    if (!entry) return;
    if (current.entry !== entry) {
      current.node.innerHTML = `<p class="muted">${tr('services.loading_usage')}</p>`;
      current.entry = entry;
      paintUsage(entry);
    }
    if (!entry.data || entry.revision !== revision || Date.now() - entry.at >= maxAge) return readUsage(entry);
  }
  function refreshAccountDetailsUsage() { revision++; return showUsage(); }
  function reset() { for (const id of usage.keys()) discard(id); }
  async function openAccountDetails(id, navigation = {}) {
    const account = findAccount(id); if (!account) return;
    services.open(id, 'account', 'details', renderInfo(account), '', null, navigation);
    current = {id, node:$('account_usage')};
    await showUsage();
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
      discard(id);
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
  return {open:openAccountDetails, refresh:refreshAccountDetailsUsage, sync:showUsage, reset, saveAlias:saveAccountAlias, remove, stop};
}
