// Connection drafts depend only on service state, their own controls and saving.
function createServiceConnections({getState,cloneState,$,tr,esc,api,openModal,closeModal,persistState,notice,presets,protocolLabel,authLabel,randomId,confirmRemoval}) {
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
  const switching = editing ? `<p class="muted">${tr('选择新的连接方式后保存，已有模型保持关联。','Choose a connection method and save; existing models stay linked.')}</p>` : '';
  openModal(editing ? tr('选择 Claude 连接方式','Choose Claude connection') : tr('Claude 连接方式','Claude connection'), `${switching}<div class="choice-grid"><button class="choice" data-ui-action="claude-local-choose" data-id="${id}"><strong>${tr('本机 Claude 订阅','Local Claude subscription')}</strong><small>${tr('连接本机 Claude Code。','Connect local Claude Code.')}</small></button><button class="choice" data-ui-action="claude-cpa-choose" data-id="${id}"><strong>CPA</strong><small>${tr('填写地址和 API Key。','Enter an address and API key.')}</small></button></div>`, '', null, null, {...navigation, reopen:() => openClaudeConnectionChooser(existing, navigation)});
  $('modal_submit').style.display = 'none';
}
function providerProtocolOptions() {
  const options = [['auto', tr('自动检测','Auto detect')], ['chat_completions', 'Chat Completions'], ['responses', 'Responses'], ['anthropic_messages', 'Anthropic Messages']];
  return options.map(([value, label]) => `<option value="${value}">${label}</option>`).join('');
}
async function refreshClaudeCliAvailability() {
  const status = $('modal_claude_cli_status');
  if (!status) return;
  status.textContent = tr('正在检查 Claude Code CLI…','Checking Claude Code CLI…');
  try {
    const result = await api('/api/runtime/claude-cli');
    if (!status.isConnected) return;
    const guidance = typeof result.guidance === 'string' ? result.guidance.trim() : '';
    status.textContent = result.available
      ? tr('已找到 Claude Code CLI，EMP 可以访问。', guidance || 'Claude Code CLI is available to EMP.')
      : tr('未找到可用的 Claude Code CLI。请安装或检查 EMP 是否能够访问。', guidance || 'Claude Code CLI was not found. Install it or check that EMP can access it, then reopen this form.');
  } catch (_) {
    if (!status.isConnected) return;
    status.textContent = tr('无法检查 Claude Code CLI。请重新加载管理页后重试。','Could not check Claude Code CLI availability. Reload this page to try again.');
  }
}
// Saved keys stay server-side; the six-star placeholder is never a submitted value.
function apiKeyField(id, saved) {
  return `<label>API Key<input id="${id}" type="text" placeholder="${saved ? '******' : ''}" onfocus="this.placeholder=''" autocomplete="off" spellcheck="false" autocapitalize="off"></label>`;
}
function openClaudeCpaForm(existing = {}, serviceContext = null, navigation = {}) {
  serviceContext ||= existing.id ? {id:existing.id, kind:'provider', active:'edit'} : null;
  const editing = Boolean(existing.id);
  const identity = editing ? {} : defaultClaudeProviderIdentity('cpa');
  const switchingFromLocal = existing.auth_mode === 'claude_login';
  const switchButton = editing ? `<button type="button" class="secondary" data-ui-action="claude-change-connection" data-id="${esc(existing.id)}">${tr('更改连接方式','Change connection method')}</button>` : '';
  const body = `<p class="muted">${tr('连接 CPA，通过本机 Claude Code 使用模型。','Connect CPA models through local Claude Code.')}</p><div class="grid"><div><label>${tr('名称','Name')}<input id="modal_cpa_name" value="${esc(existing.name || identity.name)}" placeholder="${tr('留空使用默认名称','Leave blank to use the default name')}"></label></div><div class="wide"><label>CPA Base URL<input id="modal_cpa_base" type="url" value="${esc(existing.base_url || '')}" placeholder="https://cpa.example.com" required></label></div><div class="wide">${apiKeyField('modal_cpa_key', existing.api_key_set)}</div><div class="wide"><p id="modal_claude_cli_status" class="muted" role="status" aria-live="polite">${tr('正在检查 Claude Code CLI…','Checking Claude Code CLI…')}</p>${switchButton}</div></div>`;
  openModal(editing ? tr('编辑 Claude CPA','Edit Claude CPA') : tr('添加 Claude CPA','Add Claude CPA'), body, tr('保存服务','Save service'), async () => {
    const id = existing.id || identity.id;
    const baseUrl = $('modal_cpa_base').value.trim();
    const apiKey = $('modal_cpa_key').value.trim();
    if (!id || !baseUrl) throw new Error(tr('CPA Base URL 不能为空','CPA Base URL is required'));
    if (!editing && providerIdentityTaken(id)) throw new Error(tr('服务引用发生冲突，请重新打开此窗口','Service reference conflict; reopen this dialog'));
    if (!apiKey && !existing.api_key_set) throw new Error(tr('请填写 CPA API Key','Enter the CPA API key'));
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
  const switchButton = editing ? `<button type="button" class="secondary" data-ui-action="claude-change-connection" data-id="${esc(existing.id)}">${tr('更改连接方式','Change connection method')}</button>` : '';
  const body = `<p class="muted">${tr('连接本机 Claude Code 订阅。','Connect your local Claude Code subscription.')}</p><p class="muted">${tr('在“模型设置”中更新列表并选择模型，即可在 Codex 模型选择器中使用。','Update the list and select models in Model settings to use them in the Codex model picker.')}</p><div class="grid"><div><label>${tr('名称','Name')}<input id="modal_local_claude_name" value="${esc(existing.name || identity.name)}" placeholder="${tr('留空使用默认名称','Leave blank to use the default name')}"></label></div><div class="wide"><p id="modal_claude_cli_status" class="muted" role="status" aria-live="polite">${tr('正在检查 Claude Code CLI…','Checking Claude Code CLI…')}</p>${switchButton}</div></div>`;
  openModal(editing ? tr('编辑本机 Claude 订阅','Edit local Claude subscription') : tr('本机 Claude 订阅','Local Claude subscription'), body, tr('保存服务','Save service'), async () => {
    const id = existing.id || identity.id;
    if (!id) throw new Error(tr('无法创建服务，请重新打开此窗口','Unable to create a service; reopen this dialog'));
    if (!editing && providerIdentityTaken(id)) throw new Error(tr('服务引用发生冲突，请重新打开此窗口','Service reference conflict; reopen this dialog'));
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
  const keyField = preset.auth_mode === 'forward' ? `<p class="muted">${tr('使用 Codex 登录','Use Codex sign-in')}</p>` : apiKeyField('modal_provider_key', existing?.api_key_set);
  openModal(tr('配置 ','Configure ') + preset.name, `<p class="muted">Endpoint: <code>${esc(preset.base_url)}</code><br>${tr('协议','Protocol')}: ${esc(protocolLabel(preset.protocol))}<br>${tr('认证','Authentication')}: ${esc(authLabel(preset.auth_mode))}</p>${keyField}`, tr('保存服务','Save service'), async () => {
    const keyValue = preset.auth_mode === 'forward' ? '' : $('modal_provider_key').value.trim(); if (preset.auth_mode !== 'forward' && !keyValue && !(existing && existing.api_key_set)) throw new Error(tr('请填写 API Key','Enter an API Key'));
    await saveAndClose(Object.assign({}, preset, {api_key:keyValue}));
  }, serviceContext, navigation);
}
function openCustomProviderForm(existing, serviceContext = null, navigation = {}) {
  existing = existing || {};
  if (existing.execution_backend === 'claude_cli') return editClaudeProvider(existing, serviceContext, navigation);
  const forwardAuthOption = existing.auth_mode === 'forward' ? `<option value="forward">Forward Codex auth</option>` : '';
  openModal(existing.id ? tr('编辑自建 Provider','Edit custom Provider') : tr('添加自建 Provider','Add custom Provider'), `<div class="grid"><div><label>${tr('名称','Name')}</label><input id="modal_provider_name" value="${esc(existing.name || '')}" placeholder="${tr('例如 工作服务','for example Work service')}"></div><div class="wide"><label>Base URL</label><input id="modal_provider_base" value="${esc(existing.base_url || '')}" placeholder="https://api.example.com"></div><div><label>${tr('协议','Protocol')}</label><select id="modal_provider_protocol"></select></div><div><label>${tr('认证','Authentication')}</label><select id="modal_provider_auth"><option value="api_key">Bearer API Key</option><option value="anthropic_api_key">Anthropic API Key</option>${forwardAuthOption}</select></div><div class="wide">${apiKeyField('modal_provider_key', existing.api_key_set)}</div></div>`, tr('保存服务','Save service'), async () => {
    const id = existing.id || 'service-'+randomId().slice(0,8); const base = $('modal_provider_base').value.trim(); if (!base) throw new Error(tr('Base URL 不能为空','Base URL is required')); const keyValue = $('modal_provider_key').value.trim();
    const protocolValue = $('modal_provider_protocol').value; const authMode = $('modal_provider_auth').value;
    if (authMode !== 'forward' && !keyValue && !existing.api_key_set) throw new Error(tr('请填写 API Key','Enter an API Key'));
    await saveAndClose({id, name:$('modal_provider_name').value.trim() || tr('API 服务','API service'), base_url:base, protocol:protocolValue, auth_mode:authMode, api_key:keyValue});
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
  if (!saved || (saved.auth_mode !== 'forward' && saved.auth_mode !== 'claude_login' && !saved.api_key_set)) throw new Error(tr('API Key 未保存，请重新输入','The API Key was not saved; enter it again'));
  notice(tr('服务已保存','Service saved'));
}
async function removeProvider(id) {
  const name=(getState().providers||[]).find(p=>p.id===id)?.name||tr('未命名服务','Unnamed service');
  if(!confirmRemoval(tr(`移除服务 ${name} 及其模型？`,`Remove service ${name} and its models?`)))return;
  const owner=$('modal_body').firstElementChild,candidate=cloneState();
  candidate.providers=candidate.providers.filter(p=>p.id!==id);
  candidate.models=candidate.models.filter(m=>m.provider!==id);
  try {
    await persistState(tr('服务已移除','Service removed'),candidate);
    if(owner?.isConnected)closeModal();
  } catch(error) { notice(error.message,true); }
}


return {openClaudeConnectionChooser,openClaudeCpaForm,openLocalClaudeSubscriptionForm,chooseOfficialProvider,openCustomProviderForm,editProvider,removeProvider};
}
