// The saved provider list is owned by EMP; opening and selecting stay offline.
function createProviderModelSettings({getState, $, api, tr, esc, openModal, closeModal,
  modelSort, capabilitySummary, formatDate, errorText, onSaved}) {
  let active = null;
  function stop() { active?.controller?.abort(); active = null; }
  function failure(error) {
    const messages = {
      claude_cli_login_required:tr('请先登录 Claude Code 订阅。','Sign in to a Claude Code subscription.'),
      claude_cli_subscription_required:tr('当前 Claude Code 使用 API 连接。','Claude Code is using an API connection.'),
      claude_cli_models_unsupported:tr('更新 Claude Code 后，再更新模型列表。','Update Claude Code, then update the model list.'),
      claude_cli_timeout:tr('模型列表查询超时，请重试。','Model list query timed out. Try again.'),
    };
    return messages[error.payload?.error?.code] || errorText(error);
  }
  function current(view) { return active === view && $('provider_model_settings') === view.node; }
  function selected() { return [...document.querySelectorAll('input[name="discovered_model"]:checked')].map(input => input.value); }
  function render(view, payload, keep = null) {
    view.models = [...(payload.models || [])].sort(modelSort);
    view.cached = payload.cached === true || Boolean(payload.updated_at);
    const imported = new Set(keep || (getState().models || []).filter(m => m.provider === view.id && m.enabled).map(m => m.upstream_id));
    $('model_list_updated').textContent = payload.updated_at ? tr('更新于 ', 'Updated ') + new Date(payload.updated_at).toLocaleString() : '';
    $('discovered_models').innerHTML = view.models.map(m => {
      const id = m.upstream_id || '', text = (m.display_name || id) + ' ' + id;
      const capabilities = capabilitySummary(m);
      return `<label class="model-option" data-discovered-option data-search="${esc(text.toLowerCase())}"><input type="checkbox" name="discovered_model" value="${esc(id)}" ${imported.has(id) ? 'checked' : ''} onchange="providerModelSettings.count()"><span><strong>${esc(m.display_name || id)}</strong><small>${esc(id)}${m.context_window > 0 ? ' · ' + tr('上下文 ', 'Context ') + Number(m.context_window).toLocaleString() : ''}${capabilities ? ' · ' + capabilities : ''}${formatDate(m.created_at) ? ' · ' + formatDate(m.created_at) : ''}</small></span></label>`;
    }).join('') || `<p class="muted">${view.cached ? tr('模型列表为空','The model list is empty') : tr('点击“更新模型列表”获取模型','Click Update model list to load models')}</p>`;
    $('modal_submit').disabled = !view.cached;
    filter($('discovered_search').value);
  }
  async function open(id) {
    const provider = (getState().providers || []).find(p => p.id === id);
    if (!provider) return;
    const view = {id, models:[], cached:false, controller:new AbortController()};
    openModal(tr('模型设置','Model settings') + ' · ' + (provider.name || id), `
      <div id="provider_model_settings">
        <div class="model-list-heading"><span id="model_list_updated" class="muted"></span><button type="button" class="secondary" data-icon="refresh" onclick="providerModelSettings.refresh()">${tr('更新模型列表','Update model list')}</button></div>
        <div class="toolbar model-list-controls"><input id="discovered_search" placeholder="${tr('搜索模型名称或 ID','Search model name or ID')}" oninput="providerModelSettings.filter(this.value)"><button id="discovered_select_all" type="button" class="secondary" onclick="providerModelSettings.select(true)">${tr('全选','Select all')}</button><button id="discovered_clear_all" type="button" class="secondary" onclick="providerModelSettings.select(false)">${tr('全不选','Select none')}</button><span id="discovered_count" class="muted"></span></div>
        <div id="discovered_models" class="model-selection">${tr('正在读取模型列表…','Loading saved model list…')}</div>
      </div>`, tr('保存选择','Save selection'), async () => {
        const result = await api('/api/providers/discover', {method:'POST', body:JSON.stringify({provider:id, selected:selected(), cached:true})});
        if (current(view)) closeModal();
        await onSaved(result);
      });
    active = view; view.node = $('provider_model_settings'); $('modal_submit').disabled = true;
    const controller = view.controller;
    try {
      const payload = await api('/api/providers/' + encodeURIComponent(id) + '/models', {signal:controller.signal});
      if (current(view) && !controller.signal.aborted) render(view, payload);
    } catch (error) { if (current(view) && !controller.signal.aborted) $('modal_status').textContent = failure(error); }
  }
  async function refresh() {
    const view = active;
    if (!view || !current(view) || view.refreshing) return;
    const keep = view.cached ? selected() : null;
    view.controller.abort(); view.controller = new AbortController();
    const button = view.node.querySelector('.model-list-heading button');
    view.refreshing = true; button.disabled = true; $('modal_submit').disabled = true;
    $('modal_status').textContent = '';
    try {
      const payload = await api('/api/providers/discover', {method:'POST', body:JSON.stringify({provider:view.id}),signal:view.controller.signal});
      if (current(view)) render(view, {...payload,cached:true}, keep);
    } catch (error) { if (current(view)) $('modal_status').textContent = failure(error); }
    finally {
      view.refreshing = false;
      if (current(view)) { button.disabled = false; $('modal_submit').disabled = !view.cached; }
    }
  }
  function filter(value) {
    const query = String(value || '').trim().toLowerCase();
    document.querySelectorAll('[data-discovered-option]').forEach(option => { option.hidden = Boolean(query) && !option.dataset.search.includes(query); });
    $('discovered_select_all').textContent = query ? tr('全选搜索结果','Select all results') : tr('全选','Select all');
    $('discovered_clear_all').textContent = query ? tr('全不选搜索结果','Select no results') : tr('全不选','Select none');
    count();
  }
  function select(checked) {
    document.querySelectorAll('[data-discovered-option]').forEach(option => { if (!option.hidden) option.querySelector('input').checked = checked; });
    count();
  }
  function count() {
    const options = [...document.querySelectorAll('input[name="discovered_model"]')];
    const visible = options.filter(input => !input.closest('[data-discovered-option]').hidden);
    $('discovered_count').textContent = tr(`已选 ${selected().length}/${options.length}（当前结果 ${visible.filter(input => input.checked).length}/${visible.length}）`, `${selected().length}/${options.length} selected (${visible.filter(input => input.checked).length}/${visible.length} in current results)`);
  }
  return {open, refresh, filter, select, count, stop};
}
