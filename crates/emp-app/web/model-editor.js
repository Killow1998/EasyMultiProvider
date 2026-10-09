// Model draft and async metadata belong to the editor that opened them.
function createModelEditor({getState, $, tr, esc, api, persistState, openModal, closeModal, notice,
  upstreamErrorText, testModelAudio, testModelVision, movePresentation, format}) {
  const {visionStatus, modalityLabel, compactContext, usableContext} = format;
  const COMMON_MODALITIES = ['text','image'];
  let metadataController = null;
  function stop() { metadataController?.abort(); metadataController = null; }
  let modalReasoningLevels = null;
  let modalReasoningSupport = null;
  let modalReasoningSummarySupport = null;
  let editingModelId = '';
  // Modalities with a test the edit panel can run against the saved model.
  let modalVisionStatus = 'unknown';
  let modalModalitiesEdited = false;
  let modalReasoningEdited = false;
  function modalityOption(value, checked, testable = false) {
    const test = testable && value === 'image' ? `<button type="button" class="secondary modality-test" data-editor-action="vision">${tr('测试','Test')}</button>` : '';
    return `<span class="modality-option"><label><input type="checkbox" data-modality value="${esc(value)}" ${checked ? 'checked' : ''} data-editor-change="modality">${esc(modalityLabel(value))}</label>${test}</span>`;
  }
  function modalityOptions(selected, vision, testable) {
    const audio = testable ? `<button type="button" class="secondary modality-test" data-editor-action="audio">${tr('测试音频','Test audio')}</button>` : '';
    return COMMON_MODALITIES.map(value => modalityOption(value, selected.includes(value), testable)).join('') + audio;
  }
  async function testModalAudio() {
    if (!editingModelId) return notice(tr('请先保存模型再测试','Save the model before testing'), true);
    await testModelAudio(editingModelId);
  }
  async function testModalVision() {
    if (!editingModelId) return notice(tr('请先保存模型再测试','Save the model before testing'), true);
    const root = $('model_editor');
    if (await testModelVision(editingModelId) && $('model_editor') === root) { const box = document.querySelector('[data-modality][value="image"]'); if (box) box.checked = true; modalVisionStatus = 'supported'; modalModalitiesEdited = true; }
  }
  function openManualModelModal(modelId = '', preferredProviderId = '') {
    const existing = (getState().models || []).find(m => m.id === modelId) || {};
    const selectedProviderId = existing.provider || preferredProviderId || (getState().providers || [])[0]?.id || '';
    const selectedProvider = (getState().providers || []).find(provider => provider.id === selectedProviderId);
    const localClaude = selectedProvider?.auth_mode === 'claude_login';
    editingModelId = modelId;
    modalReasoningLevels = Array.isArray(existing.reasoning_levels) ? existing.reasoning_levels : null;
    modalReasoningSupport = typeof existing.supports_reasoning === 'boolean' ? existing.supports_reasoning : null;
    modalReasoningSummarySupport = typeof existing.supports_reasoning_summaries === 'boolean' ? existing.supports_reasoning_summaries : null;
    const options = (getState().providers || []).map(p => `<option value="${esc(p.id)}">${esc(p.name || p.id)} (${esc(p.id)})</option>`).join('');
    const vision = visionStatus(existing);
    const inputModalities = (Array.isArray(existing.input_modalities) ? existing.input_modalities : ['text']).filter(value => value !== 'image').concat(vision === 'supported' ? ['image'] : []);
    const upstreamLabel = localClaude ? tr('模型别名或完整 ID','Model alias or full ID') : tr('上游模型名','Upstream model name');
    const upstreamPlaceholder = localClaude ? tr('输入 Claude Code 支持的别名或完整 ID','Enter a Claude Code model alias or full ID') : tr('例如 glm','for example glm');
    modalVisionStatus = vision;
    modalModalitiesEdited = false;
    modalReasoningEdited = false;
    const body = `<div class="grid"><div><label>Provider</label><select id="modal_model_provider" data-editor-change="route">${options}</select></div><div><label id="modal_model_upstream_label">${upstreamLabel}</label><input id="modal_model_upstream" value="${esc(existing.upstream_id || '')}" placeholder="${upstreamPlaceholder}" data-editor-input="route"></div><div class="wide"><label>${tr('最大上下文窗口（可选）','Maximum context window (optional)')}</label><div class="input-action"><input id="modal_model_context" type="text" inputmode="numeric" pattern="[0-9]*" value="${Number(existing.context_window || 0)}" data-editor-input="context"><button id="modal_model_context_inspect" type="button" class="secondary" data-editor-action="inspect" ${localClaude ? 'hidden' : ''}>${tr('获取上游最大值','Fetch upstream maximum')}</button></div><small class="muted" id="modal_model_context_usable">${existing.context_window ? tr(`Codex 可用 ${compactContext(usableContext(existing))}`,`Codex uses ${compactContext(usableContext(existing))}`) : ''}</small></div><div class="wide"><label>${tr('可选思考强度','Available reasoning levels')}</label><div id="modal_model_reasoning" class="modality-options">${reasoningOptions(modalReasoningLevels || [])}</div><small class="muted">${tr('在 Codex 选择模型时切换。','Choose the effort when selecting a model in Codex.')}</small></div><div class="wide"><label>${tr('输入模态','Input modalities')}</label><div id="modal_model_modalities" class="modality-options">${modalityOptions(inputModalities, vision, Boolean(modelId))}</div></div><div class="wide"><p class="muted">${tr('不可变路由','Stable route')}: <code id="modal_model_route"></code></p></div></div><p class="muted">${tr('模型名称和上下文标识请在右侧“模型显示”中设置。','Set the model name and context label in Model display on the right.')}</p>`;
    openModal(modelId ? tr('编辑模型','Edit model') : tr('手动添加模型','Add model manually'), `<div id="model_editor">${body}</div>`, tr('保存模型','Save model'), async () => { const node = $('model_editor'); await saveManualModel(); if ($('model_editor') === node) closeModal(); });
    const root = $('model_editor');
    root.onchange = event => {
      const action = event.target.dataset.editorChange;
      if (action === 'route') updateManualModelRoute();
      else if (action === 'reasoning') modalReasoningEdited = true;
      else if (action === 'modality') { modalModalitiesEdited = true; if (event.target.value === 'image') modalVisionStatus = event.target.checked ? 'supported' : 'unsupported'; }
    };
    root.oninput = event => { if (event.target.dataset.editorInput === 'route') updateManualModelRoute(); else if (event.target.dataset.editorInput === 'context') { event.target.value = event.target.value.replace(/\D/g,''); previewModalContext(); } };
    root.onclick = event => { const action = event.target.closest('[data-editor-action]')?.dataset.editorAction; if (action === 'inspect') inspectModalModel(); else if (action === 'vision') testModalVision(); else if (action === 'audio') testModalAudio(); };
    if (selectedProviderId) $('modal_model_provider').value = selectedProviderId;
    updateManualModelRoute();
  }
  function reasoningOptions(selected) {
    const levels = [...new Set(['low','medium','high','xhigh','max', ...selected])];
    return levels.map(level => `<span class="modality-option"><label><input type="checkbox" data-reasoning-level value="${esc(level)}" ${selected.includes(level) ? 'checked' : ''} data-editor-change="reasoning">${esc(level)}</label></span>`).join('');
  }
  function manualModelRoute() { const provider = $('modal_model_provider')?.value || ''; const entered = $('modal_model_upstream')?.value.trim() || ''; const prefix = provider + '/'; return provider && entered ? prefix + (entered.startsWith(prefix) ? entered.slice(prefix.length) : entered) : ''; }
  function updateManualModelRoute() {
    metadataController?.abort(); metadataController = null;
    const route = manualModelRoute();
    if ($('modal_model_route')) $('modal_model_route').textContent = route || tr('保存后生成','Generated after save');
    const provider = (getState().providers || []).find(item => item.id === $('modal_model_provider')?.value);
    const localClaude = provider?.auth_mode === 'claude_login';
    const label = $('modal_model_upstream_label');
    const upstream = $('modal_model_upstream');
    const inspect = $('modal_model_context_inspect');
    if (label) label.textContent = localClaude ? tr('模型别名或完整 ID','Model alias or full ID') : tr('上游模型名','Upstream model name');
    if (upstream) upstream.placeholder = localClaude ? tr('输入 Claude Code 支持的别名或完整 ID','Enter a Claude Code model alias or full ID') : tr('例如 glm','for example glm');
    if (inspect) inspect.hidden = Boolean(localClaude);
  }
  function previewModalContext() { const value = Number($('modal_model_context').value) || 0; $('modal_model_context_usable').textContent = value ? tr(`Codex 可用 ${compactContext(usableContext({context_window:value}))}`,`Codex uses ${compactContext(usableContext({context_window:value}))}`) : ''; }
  async function inspectModalModel() {
    const provider = $('modal_model_provider').value, upstream = $('modal_model_upstream').value.trim();
    if (!provider || !upstream) return notice(tr('请填写 Provider 和上游模型名','Enter a Provider and upstream model name'), true);
    if ((getState().providers || []).some(item => item.id === provider && item.auth_mode === 'claude_login')) return notice(tr('在“模型设置”中更新 Claude Code 模型列表。','Update the Claude Code model list in Model settings.'), true);
    metadataController?.abort(); metadataController = new AbortController();
    const controller = metadataController, root = $('model_editor');
    try {
      const result = await api('/api/models/metadata', {method:'POST',body:JSON.stringify({provider,model:upstream}),signal:controller.signal});
      if (controller.signal.aborted || $('model_editor') !== root) return;
      $('modal_model_context').value = result.context_window;
      modalReasoningLevels = Array.isArray(result.reasoning_levels) ? result.reasoning_levels : [];
      modalReasoningSupport = typeof result.supports_reasoning === 'boolean' ? result.supports_reasoning : null;
      modalReasoningSummarySupport = typeof result.supports_reasoning_summaries === 'boolean' ? result.supports_reasoning_summaries : null;
      $('modal_model_reasoning').innerHTML = reasoningOptions(modalReasoningLevels); modalReasoningEdited = true;
      updateManualModelRoute();
      notice(tr(`已获取上限：输入 ${Number(result.input_token_limit).toLocaleString()} · 输出 ${Number(result.output_token_limit).toLocaleString()}`,`Limits fetched: input ${Number(result.input_token_limit).toLocaleString()} · output ${Number(result.output_token_limit).toLocaleString()}`));
    } catch (error) { if (!controller.signal.aborted && $('model_editor') === root) notice(tr('获取失败：','Fetch failed: ') + upstreamErrorText(error),true); }
  }
  async function saveManualModel() {
    const root = $('model_editor');
    const provider = $('modal_model_provider').value;
    const entered = $('modal_model_upstream').value.trim();
    if (!provider || !entered) throw new Error(tr('Provider 和上游模型名不能为空','Provider and upstream model name are required'));
    const prefix = provider + '/';
    const upstream = entered.startsWith(prefix) ? entered.slice(prefix.length) : entered;
    const id = prefix + upstream;
    const previous = getState().models.find(m => m.id === editingModelId);
    const collision = getState().models.find(m => m.id === id && m.id !== editingModelId);
    if (collision) throw new Error(tr('模型路由已存在，不能覆盖另一个模型','That model route already exists and cannot overwrite another model'));
    const imageChecked = Boolean(document.querySelector('[data-modality][value="image"]:checked'));
    // Unchecked image keeps "unknown" until the user or a test has confirmed it either way.
    const vision = imageChecked ? 'supported' : (modalVisionStatus === 'unknown' ? 'unknown' : 'unsupported');
    const kept = (previous?.input_modalities || []).filter(value => !COMMON_MODALITIES.includes(value));
    const modalities = [...new Set([...document.querySelectorAll('[data-modality]:checked')].map(input => input.value).concat(kept))].filter(value => value !== 'image');
    if (modalities.length > 16 || modalities.some(value => !/^[a-z0-9][a-z0-9._:-]{0,63}$/.test(value) || value === 'image')) {
      throw new Error(tr('输入模态无效；图像请使用上方选项','Invalid input modality; use the vision selector for images'));
    }
    if (vision === 'supported') modalities.push('image');
    if (!modalities.length) modalities.push('text');
    if (modalities.length > 16) throw new Error(tr('输入模态不能超过 16 项','Input modalities cannot exceed 16 entries'));
    const levels = modalReasoningEdited ? [...document.querySelectorAll('[data-reasoning-level]:checked')].map(input => input.value) : (modalReasoningLevels || previous?.reasoning_levels || []);
    const support = levels.length ? true : (modalReasoningSupport ?? previous?.supports_reasoning ?? null);
    const summarySupport = typeof modalReasoningSummarySupport === 'boolean' ? modalReasoningSummarySupport : (previous?.supports_reasoning_summaries ?? null);
    const model = {
      ...(previous || {}), id, provider, upstream_id:upstream,
      display_name:previous?.display_name || upstream, description:previous?.description || '',
      family_id:previous?.family_id || '', supports_reasoning:support,
      supports_reasoning_summaries:summarySupport, reasoning_levels:levels,
      context_window:Number($('modal_model_context').value) || 0,
      input_modalities:modalities, created_at:previous?.created_at || 0,
      capability_sources:{...(previous?.capability_sources || {}),
        ...(modalModalitiesEdited ? {input_modalities:{source:'manual'}} : {}),
        ...(modalReasoningEdited ? {reasoning_levels:{source:'manual'}, supports_reasoning:{source:'manual'}} : {})},
      enabled:previous ? previous.enabled : true,
    };
    const candidate = structuredClone(getState());
    if (editingModelId && editingModelId !== id) {
      candidate.models = candidate.models.filter(m => m.id !== editingModelId);
      movePresentation(editingModelId, id, candidate);
    }
    const index = candidate.models.findIndex(m => m.id === id);
    if (index >= 0) candidate.models[index] = model; else candidate.models.push(model);
    await persistState(previous ? tr('模型已修改','Model updated') : tr('模型已添加','Model added'), candidate);
    if ($('model_editor') === root) {
      editingModelId = ''; modalReasoningLevels = null;
      modalReasoningSupport = null; modalReasoningSummarySupport = null;
    }
  }
  return {open:openManualModelModal, stop};
}
