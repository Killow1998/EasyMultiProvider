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
    const test = testable && value === 'image' ? `<button type="button" class="secondary modality-test" data-editor-action="vision">${tr("models.test")}</button>` : '';
    return `<span class="modality-option"><label><input type="checkbox" data-modality value="${esc(value)}" ${checked ? 'checked' : ''} data-editor-change="modality">${esc(modalityLabel(value))}</label>${test}</span>`;
  }
  function modalityOptions(selected, vision, testable) {
    const audio = testable ? `<button type="button" class="secondary modality-test" data-editor-action="audio">${tr("models.test_audio")}</button>` : '';
    return COMMON_MODALITIES.map(value => modalityOption(value, selected.includes(value), testable)).join('') + audio;
  }
  async function testModalAudio() {
    if (!editingModelId) return notice(tr("models.save_the_model_before_testing"), true);
    await testModelAudio(editingModelId);
  }
  async function testModalVision() {
    if (!editingModelId) return notice(tr("models.save_the_model_before_testing"), true);
    const root = $('model_editor');
    if (await testModelVision(editingModelId) && $('model_editor') === root) { const box = document.querySelector('[data-modality][value="image"]'); if (box) box.checked = true; modalVisionStatus = 'supported'; modalModalitiesEdited = true; }
  }
  function openManualModelModal(modelId = '', preferredProviderId = '', navigation = {}) {
    const existing = (getState().models || []).find(m => m.id === modelId) || {};
    const selectedProviderId = existing.provider || preferredProviderId || (getState().providers || [])[0]?.id || '';
    const selectedProvider = (getState().providers || []).find(provider => provider.id === selectedProviderId);
    const localClaude = selectedProvider?.auth_mode === 'claude_login';
    editingModelId = modelId;
    modalReasoningLevels = Array.isArray(existing.reasoning_levels) ? existing.reasoning_levels : null;
    modalReasoningSupport = typeof existing.supports_reasoning === 'boolean' ? existing.supports_reasoning : null;
    modalReasoningSummarySupport = typeof existing.supports_reasoning_summaries === 'boolean' ? existing.supports_reasoning_summaries : null;
    const options = (getState().providers || []).map(p => `<option value="${esc(p.id)}">${esc(p.name || tr("core.unnamed_service"))}</option>`).join('');
    const vision = visionStatus(existing);
    const inputModalities = (Array.isArray(existing.input_modalities) ? existing.input_modalities : ['text']).filter(value => value !== 'image').concat(vision === 'supported' ? ['image'] : []);
    const upstreamLabel = localClaude ? tr("models.model_alias_or_full_id") : tr("models.upstream_model_name");
    const upstreamPlaceholder = localClaude ? tr("models.enter_a_claude_code_model_alias_or_full_id") : tr("models.for_example_glm");
    modalVisionStatus = vision;
    modalModalitiesEdited = false;
    modalReasoningEdited = false;
    const body = `<div class="grid"><div><label>${tr("common.provider")}</label><select id="modal_model_provider" data-editor-change="route">${options}</select></div><div><label id="modal_model_upstream_label">${upstreamLabel}</label><input id="modal_model_upstream" value="${esc(existing.upstream_id || '')}" placeholder="${upstreamPlaceholder}" data-editor-input="route"></div><div class="wide"><label>${tr("models.maximum_context_window")}</label><div class="input-action"><input id="modal_model_context" type="text" inputmode="numeric" pattern="[0-9]*" value="${Number(existing.context_window || 0)}" data-editor-input="context"><button id="modal_model_context_inspect" type="button" class="secondary" data-editor-action="inspect" ${localClaude ? 'hidden' : ''}>${tr("models.fetch_upstream_maximum")}</button></div><small class="muted" id="modal_model_context_usable">${existing.context_window ? tr("models.codex_uses", {context:compactContext(usableContext(existing))}) : ''}</small></div><div class="wide"><label>${tr("models.available_reasoning_levels")}</label><div id="modal_model_reasoning" class="modality-options">${reasoningOptions(modalReasoningLevels || [])}</div><small class="muted">${tr("models.choose_the_effort_when_selecting_a_model_in_codex")}</small></div><div class="wide"><label>${tr("models.input_modalities")}</label><div id="modal_model_modalities" class="modality-options">${modalityOptions(inputModalities, vision, Boolean(modelId))}</div></div><div class="wide"><code id="modal_model_route" hidden></code></div></div><p class="muted">${tr("models.set_the_model_name_and_context_label_in_model")}</p>`;
    openModal(modelId ? tr("models.edit_model") : tr("add_model"), `<div id="model_editor">${body}</div>`, tr("models.save_model"), async () => { const node = $('model_editor'); await saveManualModel(); if ($('model_editor') === node) closeModal(); }, null, navigation);
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
    if ($('modal_model_route')) $('modal_model_route').textContent = route || tr("models.generated_after_save");
    const provider = (getState().providers || []).find(item => item.id === $('modal_model_provider')?.value);
    const localClaude = provider?.auth_mode === 'claude_login';
    const label = $('modal_model_upstream_label');
    const upstream = $('modal_model_upstream');
    const inspect = $('modal_model_context_inspect');
    if (label) label.textContent = localClaude ? tr("models.model_alias_or_full_id") : tr("models.upstream_model_name");
    if (upstream) upstream.placeholder = localClaude ? tr("models.enter_a_claude_code_model_alias_or_full_id") : tr("models.for_example_glm");
    if (inspect) inspect.hidden = Boolean(localClaude);
  }
  function previewModalContext() { const value = Number($('modal_model_context').value) || 0; $('modal_model_context_usable').textContent = value ? tr("models.codex_uses", {context:compactContext(usableContext({context_window:value}))}) : ''; }
  async function inspectModalModel() {
    const provider = $('modal_model_provider').value, upstream = $('modal_model_upstream').value.trim();
    if (!provider || !upstream) return notice(tr("models.enter_a_provider_and_upstream_model_name"), true);
    if ((getState().providers || []).some(item => item.id === provider && item.auth_mode === 'claude_login')) return notice(tr("models.update_the_claude_code_model_list_in_model_settings"), true);
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
      notice(tr("models.limits_fetched_input_output", {inputTokenLimit:Number(result.input_token_limit).toLocaleString(), outputTokenLimit:Number(result.output_token_limit).toLocaleString()}));
    } catch (error) { if (!controller.signal.aborted && $('model_editor') === root) notice(tr("models.fetch_failed") + upstreamErrorText(error),true); }
  }
  async function saveManualModel() {
    const root = $('model_editor');
    const provider = $('modal_model_provider').value;
    const entered = $('modal_model_upstream').value.trim();
    if (!provider || !entered) throw new Error(tr("models.provider_and_upstream_model_name_are_required"));
    const prefix = provider + '/';
    const upstream = entered.startsWith(prefix) ? entered.slice(prefix.length) : entered;
    const id = prefix + upstream;
    const previous = getState().models.find(m => m.id === editingModelId);
    const collision = getState().models.find(m => m.id === id && m.id !== editingModelId);
    if (collision) throw new Error(tr("models.that_model_route_already_exists_and_cannot_overwrite_another"));
    const imageChecked = Boolean(document.querySelector('[data-modality][value="image"]:checked'));
    // Unchecked image keeps "unknown" until the user or a test has confirmed it either way.
    const vision = imageChecked ? 'supported' : (modalVisionStatus === 'unknown' ? 'unknown' : 'unsupported');
    const kept = (previous?.input_modalities || []).filter(value => !COMMON_MODALITIES.includes(value));
    const modalities = [...new Set([...document.querySelectorAll('[data-modality]:checked')].map(input => input.value).concat(kept))].filter(value => value !== 'image');
    if (modalities.length > 16 || modalities.some(value => !/^[a-z0-9][a-z0-9._:-]{0,63}$/.test(value) || value === 'image')) {
      throw new Error(tr("models.invalid_input_modality_use_the_vision_selector_for_images"));
    }
    if (vision === 'supported') modalities.push('image');
    if (!modalities.length) modalities.push('text');
    if (modalities.length > 16) throw new Error(tr("models.input_modalities_cannot_exceed_16_entries"));
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
    await persistState(previous ? tr("models.model_updated") : tr("models.model_added"), candidate);
    if ($('model_editor') === root) {
      editingModelId = ''; modalReasoningLevels = null;
      modalReasoningSupport = null; modalReasoningSummarySupport = null;
    }
  }
  return {open:openManualModelModal, stop};
}
