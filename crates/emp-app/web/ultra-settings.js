// Ultra keeps Codex's collaboration behavior; this changes its concrete effort.
function createUltraSettings({getState, cloneState, persistState, openModal, closeModal, $, esc, tr, help}) {
  function models(state) {
    const eligible = (state.catalog_models || []).filter(model => model.ultra_reasoning_levels?.length);
    return (state.catalog_families || []).flatMap(family => {
      const routes = eligible.filter(model => family.routes.some(route => route.id === model.id));
      if (!routes.length) return [];
      const levels = routes[0].ultra_reasoning_levels.filter(level => routes.every(route => route.ultra_reasoning_levels.includes(level)));
      const defaults = [...new Set(routes.map(route => route.ultra_default_effort).filter(Boolean))];
      return levels.length ? [{...family, levels, defaults}] : [];
    });
  }
  function open({back} = {}) {
    const state = getState(), entries = models(state);
    const body = entries.map((model, index) => {
      const selected = state.ultra_reasoning_efforts?.[model.id] || 'auto';
      const unavailable = selected !== 'auto' && !model.levels.includes(selected);
      const official = tr('models.ultra_follow_official', {effort:model.defaults.join(' / ') || tr('models.catalog_default_unavailable')});
      return `<div class="setting-row routing-mode"><label class="setting-title" for="ultra_effort_${index}">${esc(model.default_display_name || model.id)}</label><select id="ultra_effort_${index}" data-ultra-family="${esc(model.id)}"><option value="auto" ${selected === 'auto' ? 'selected' : ''}>${esc(official)}</option>${unavailable ? `<option value="${esc(selected)}" selected disabled>${esc(selected)} · ${tr('core.unavailable')}</option>` : ''}${model.levels.map(level => `<option value="${esc(level)}" ${selected === level ? 'selected' : ''}>${esc(level)}</option>`).join('')}</select></div>`;
    }).join('');
    openModal(tr('models.ultra_actual_effort'), `<div id="ultra_settings" class="routing-settings"><div class="collaboration-title"><p class="setting-help">${tr('models.ultra_hint')}</p>${help(tr('models.ultra_help'))}</div>${body || `<p class="muted">${tr('models.ultra_unavailable')}</p>`}</div>`, entries.length ? tr('save_display') : '', entries.length ? async () => {
      const form = $('ultra_settings'), controls = [...form.querySelectorAll('select')];
      const candidate = cloneState();
      candidate.ultra_reasoning_efforts ||= {};
      for (const control of controls) {
        const family = control.dataset.ultraFamily;
        if (control.value === 'auto') delete candidate.ultra_reasoning_efforts[family];
        else candidate.ultra_reasoning_efforts[family] = control.value;
      }
      controls.forEach(control => control.disabled = true);
      try {
        await persistState(tr('models.ultra_saved'), candidate);
        if (form.isConnected) (back || closeModal)();
      } finally { controls.forEach(control => control.disabled = false); }
    } : null, null, {back});
  }
  return {open};
}
