// Connection drafts depend only on service state, their own controls and saving.
function createServiceConnections({getState,getLanguage=()=> 'en',cloneState,$,tr,esc,api,openModal,closeModal,persistState,notice,presets,protocolLabel,authLabel,randomId,confirmRemoval}) {
function normalizedIdentity(value) { return String(value || '').trim().toLocaleLowerCase(); }
function providerIdentityTaken(id) {
  const identity = normalizedIdentity(id);
  if (!identity) return false;
  const accounts = [getState().native_account, ...(getState().accounts || [])].filter(Boolean);
  return (getState().providers || []).some(provider => normalizedIdentity(provider.id) === identity)
    || accounts.some(account => [account.id, account.prefix].some(value => normalizedIdentity(value) === identity));
}
function providerNameTaken(name) {
  const identity = normalizedIdentity(name);
  if (!identity) return false;
  const accounts = [getState().native_account, ...(getState().accounts || [])].filter(Boolean);
  return (getState().providers || []).some(provider => normalizedIdentity(provider.name || provider.id) === identity)
    || accounts.some(account => normalizedIdentity(account.name || account.id) === identity);
}
function defaultClaudeProviderIdentity(connection) {
  const idBase = connection === 'local' ? 'claude' : 'claude-cpa';
  const nameBase = connection === 'local' ? 'Claude Subscription' : 'Claude CPA';
  for (let suffix = 1; ; suffix++) {
    const id = suffix === 1 ? idBase : `${idBase}-${suffix}`;
    const name = suffix === 1 ? nameBase : `${nameBase} ${suffix}`;
    if (!providerIdentityTaken(id) && !providerNameTaken(name)) return {id, name};
  }
}
function openClaudeConnectionChooser(existing = {}, navigation = {}) {
  const editing = Boolean(existing.id);
  const id = esc(existing.id || '');
  const switching = editing ? `<p class="muted">${tr("services.choose_a_connection_method_and_save_existing_models_stay")}</p>` : '';
  openModal(editing ? tr("services.choose_claude_connection") : tr("services.claude_connection"), `${switching}<div class="choice-grid"><button class="choice" data-ui-action="claude-local-choose" data-id="${id}"><strong>${tr("transfers.local_claude_subscription")}</strong><small>${tr("services.connect_local_claude_code_2")}</small></button><button class="choice" data-ui-action="claude-cpa-choose" data-id="${id}"><strong>CPA</strong><small>${tr("services.enter_an_address_and_api_key")}</small></button></div>`, '', null, null, {...navigation, reopen:() => openClaudeConnectionChooser(existing, navigation)});
  $('modal_submit').style.display = 'none';
}
function providerProtocolOptions() {
  const options = [['auto', tr("core.auto_detect")], ['chat_completions', 'Chat Completions'], ['responses', 'Responses'], ['anthropic_messages', 'Anthropic Messages']];
  return options.map(([value, label]) => `<option value="${value}">${label}</option>`).join('');
}
async function refreshClaudeCliAvailability() {
  const status = $('modal_claude_cli_status');
  if (!status) return;
  status.textContent = tr("services.checking_claude_code_cli");
  try {
    const result = await api('/api/runtime/claude-cli');
    if (!status.isConnected) return;
    const guidance = typeof result.guidance === 'string' ? result.guidance.trim() : '';
    status.textContent = getLanguage() === 'en' && guidance ? guidance
      : tr(result.available ? 'services.cli_available' : 'services.cli_unavailable');
  } catch (_) {
    if (!status.isConnected) return;
    status.textContent = tr("services.could_not_check_claude_code_cli_availability_reload_this");
  }
}
// Saved keys stay server-side; the six-star placeholder is never a submitted value.
function apiKeyField(id, saved) {
  return `<label>${tr("common.api_key")}<input id="${id}" type="text" placeholder="${saved ? '******' : ''}" onfocus="this.placeholder=''" autocomplete="off" spellcheck="false" autocapitalize="off"></label>`;
}
function openClaudeCpaForm(existing = {}, serviceContext = null, navigation = {}) {
  serviceContext ||= existing.id ? {id:existing.id, kind:'provider', active:'edit'} : null;
  const editing = Boolean(existing.id);
  const identity = editing ? {} : defaultClaudeProviderIdentity('cpa');
  const switchingFromLocal = existing.auth_mode === 'claude_login';
  const switchButton = editing ? `<button type="button" class="secondary" data-ui-action="claude-change-connection" data-id="${esc(existing.id)}">${tr("services.change_connection_method")}</button>` : '';
  const body = `<p class="muted">${tr("services.connect_cpa_models_through_local_claude_code")}</p><div class="grid"><div><label>${tr("services.name")}<input id="modal_cpa_name" value="${esc(existing.name || identity.name)}" placeholder="${tr("services.leave_blank_to_use_the_default_name")}"></label></div><div class="wide"><label>CPA Base URL<input id="modal_cpa_base" type="url" value="${esc(existing.base_url || '')}" placeholder="https://cpa.example.com" required></label></div><div class="wide">${apiKeyField('modal_cpa_key', existing.api_key_set)}</div><div class="wide"><p id="modal_claude_cli_status" class="muted" role="status" aria-live="polite">${tr("services.checking_claude_code_cli")}</p>${switchButton}</div></div>`;
  openModal(editing ? tr("services.edit_claude_cpa") : tr("services.add_claude_cpa"), body, tr("services.save_service"), async () => {
    const id = existing.id || identity.id;
    const baseUrl = $('modal_cpa_base').value.trim();
    const apiKey = $('modal_cpa_key').value.trim();
    if (!id || !baseUrl) throw new Error(tr("services.cpa_base_url_is_required"));
    if (!editing && providerIdentityTaken(id)) throw new Error(tr("services.service_reference_conflict_reopen_this_dialog"));
    if (!apiKey && !existing.api_key_set) throw new Error(tr("services.enter_the_cpa_api_key"));
    await saveAndClose({
      id,
      name:$('modal_cpa_name').value.trim() || existing.name || identity.name,
      base_url:baseUrl,
      protocol:editing && !switchingFromLocal ? existing.protocol : existing.protocol || 'anthropic_messages',
      auth_mode:editing && !switchingFromLocal ? existing.auth_mode : 'api_key',
      execution_backend:'claude_cli',
      api_key:apiKey
    });
  }, serviceContext, navigation);
  void refreshClaudeCliAvailability();
}
function openLocalClaudeSubscriptionForm(existing = {}, serviceContext = null, navigation = {}) {
  serviceContext ||= existing.id ? {id:existing.id, kind:'provider', active:'edit'} : null;
  const editing = Boolean(existing.id);
  const identity = editing ? {} : defaultClaudeProviderIdentity('local');
  const protocol = ['auto', 'anthropic_messages'].includes(existing.protocol) ? existing.protocol : 'anthropic_messages';
  const switchButton = editing ? `<button type="button" class="secondary" data-ui-action="claude-change-connection" data-id="${esc(existing.id)}">${tr("services.change_connection_method")}</button>` : '';
  const body = `<p class="muted">${tr("services.connect_your_local_claude_code_subscription")}</p><p class="muted">${tr("services.update_the_list_and_select_models_in_model_settings")}</p><div class="grid"><div><label>${tr("services.name")}<input id="modal_local_claude_name" value="${esc(existing.name || identity.name)}" placeholder="${tr("services.leave_blank_to_use_the_default_name")}"></label></div><div class="wide"><p id="modal_claude_cli_status" class="muted" role="status" aria-live="polite">${tr("services.checking_claude_code_cli")}</p>${switchButton}</div></div>`;
  openModal(editing ? tr("services.edit_local_claude_subscription") : tr("transfers.local_claude_subscription"), body, tr("services.save_service"), async () => {
    const id = existing.id || identity.id;
    if (!id) throw new Error(tr("services.unable_to_create_a_service_reopen_this_dialog"));
    if (!editing && providerIdentityTaken(id)) throw new Error(tr("services.service_reference_conflict_reopen_this_dialog"));
    await saveAndClose({
      id,
      name:$('modal_local_claude_name').value.trim() || existing.name || identity.name,
      execution_backend:'claude_cli',
      auth_mode:'claude_login',
      protocol,
      base_url:'',
      api_key:'',
      api_key_file:''
    });
  }, serviceContext, navigation);
  void refreshClaudeCliAvailability();
}
function editClaudeProvider(provider, serviceContext = null, navigation = {}) {
  return provider.auth_mode === 'claude_login'
    ? openLocalClaudeSubscriptionForm(provider, serviceContext, navigation)
    : openClaudeCpaForm(provider, serviceContext, navigation);
}
function chooseOfficialProvider(key, serviceContext = null, navigation = {}) {
  const preset = presets[key]; const existing = (getState().providers || []).find(p => p.id === preset.id);
  if (existing?.execution_backend === 'claude_cli') return editClaudeProvider(existing, serviceContext, navigation);
  const keyField = preset.auth_mode === 'forward' ? `<p class="muted">${tr("services.use_codex_sign_in")}</p>` : apiKeyField('modal_provider_key', existing?.api_key_set);
  openModal(tr("services.configure") + preset.name, `${keyField}<details class="provider-connection-details"><summary>${tr("services.connection_details")}</summary><p class="muted">${tr("common.base_url")}: <code>${esc(preset.base_url)}</code><br>${tr("reports.protocol")}: ${esc(protocolLabel(preset.protocol))}<br>${tr("services.authentication")}: ${esc(authLabel(preset.auth_mode))}</p></details>`, tr("services.save_service"), async () => {
    const keyValue = preset.auth_mode === 'forward' ? '' : $('modal_provider_key').value.trim(); if (preset.auth_mode !== 'forward' && !keyValue && !(existing && existing.api_key_set)) throw new Error(tr("services.enter_an_api_key"));
    await saveAndClose(Object.assign({}, preset, {api_key:keyValue}));
  }, serviceContext, navigation);
}
function openCustomProviderForm(existing, serviceContext = null, navigation = {}) {
  existing = existing || {};
  if (existing.execution_backend === 'claude_cli') return editClaudeProvider(existing, serviceContext, navigation);
  const forwardAuthOption = existing.auth_mode === 'forward' ? `<option value="forward">Forward Codex auth</option>` : '';
  openModal(existing.id ? tr("services.edit_custom_provider") : tr("services.add_custom_provider"), `<div class="grid"><div><label>${tr("services.name")}</label><input id="modal_provider_name" value="${esc(existing.name || '')}" placeholder="${tr("services.for_example_work_service")}"></div><div class="wide"><label>${tr("common.base_url")}</label><input id="modal_provider_base" value="${esc(existing.base_url || '')}" placeholder="https://api.example.com"></div><div><label>${tr("reports.protocol")}</label><select id="modal_provider_protocol"></select></div><div><label>${tr("services.authentication")}</label><select id="modal_provider_auth"><option value="api_key">Bearer API Key</option><option value="anthropic_api_key">Anthropic API Key</option>${forwardAuthOption}</select></div><div class="wide">${apiKeyField('modal_provider_key', existing.api_key_set)}</div></div>`, tr("services.save_service"), async () => {
    const id = existing.id || 'service-'+randomId().slice(0,8); const base = $('modal_provider_base').value.trim(); if (!base) throw new Error(tr("services.base_url_is_required")); const keyValue = $('modal_provider_key').value.trim();
    const protocolValue = $('modal_provider_protocol').value; const authMode = $('modal_provider_auth').value;
    if (authMode !== 'forward' && !keyValue && !existing.api_key_set) throw new Error(tr("services.enter_an_api_key"));
    await saveAndClose({id, name:$('modal_provider_name').value.trim() || tr("services.api_service"), base_url:base, protocol:protocolValue, auth_mode:authMode, api_key:keyValue});
  }, serviceContext, navigation);
  const protocol = $('modal_provider_protocol'); protocol.innerHTML = providerProtocolOptions(); protocol.value = existing.protocol || 'auto';
  if (existing.auth_mode) $('modal_provider_auth').value = existing.auth_mode;
}
function editProvider(id, navigation = {}) {
  const provider = (getState().providers || []).find(p => p.id === id); if (!provider) return;
  const context = {id, kind:'provider', active:'edit'};
  if (provider.execution_backend === 'claude_cli') editClaudeProvider(provider, context, navigation);
  else openCustomProviderForm(provider, context, navigation);
}
async function saveAndClose(provider) {
  const owner=$('modal_body').firstElementChild;
  await saveProvider(provider);
  if(owner?.isConnected)closeModal();
}
async function saveProvider(provider) {
  const old = (getState().providers || []).find(p => p.id === provider.id); const localLogin = provider.execution_backend === 'claude_cli' && provider.auth_mode === 'claude_login'; const apiKey = localLogin ? '' : provider.api_key || (old && old.api_key_set ? '••••••••' : '');
  const candidate = cloneState(); const next = Object.assign({}, old || {}, provider, {api_key:apiKey, enabled:true});
  if (localLogin) { next.base_url = ''; next.api_key = ''; next.api_key_file = ''; delete next.api_key_set; }
  const index = candidate.providers.findIndex(p => p.id === next.id); if (index >= 0) candidate.providers[index] = next; else candidate.providers.push(next); await persistState('', candidate);
  const saved = (getState().providers || []).find(p => p.id === provider.id);
  if (!saved || (saved.auth_mode !== 'forward' && saved.auth_mode !== 'claude_login' && !saved.api_key_set)) throw new Error(tr("services.the_api_key_was_not_saved_enter_it_again"));
  notice(tr("services.service_saved"));
}
async function removeProvider(id) {
  const name=(getState().providers||[]).find(p=>p.id===id)?.name||tr("core.unnamed_service");
  if(!confirmRemoval(tr("services.remove_service_and_its_models", {name:name})))return;
  const owner=$('modal_body').firstElementChild,candidate=cloneState();
  candidate.providers=candidate.providers.filter(p=>p.id!==id);
  candidate.models=candidate.models.filter(m=>m.provider!==id);
  try {
    await persistState(tr("services.service_removed"),candidate);
    if(owner?.isConnected)closeModal();
  } catch(error) { notice(error.message,true); }
}


return {openClaudeConnectionChooser,openClaudeCpaForm,openLocalClaudeSubscriptionForm,chooseOfficialProvider,openCustomProviderForm,editProvider,removeProvider};
}
