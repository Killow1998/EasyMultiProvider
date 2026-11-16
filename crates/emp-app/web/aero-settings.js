function createAeroSettings({getState, cloneState, persistState, openModal, $, esc, tr, help}) {
  function models() { return (getState().catalog_models || []).filter(model => model.source_type !== 'aero'); }
  function label(model) {
    if (!model) return tr('aggregate.unavailable');
    const state = getState(), name = canonicalDisplayName(model.display_name || model.id);
    const source = model.source_type === 'native' ? 'Native' : model.source_type === 'aggregate' ? tr('aggregate.title') : [...state.accounts, ...state.providers].find(source => source.id === model.source_id)?.name;
    return source && !name.startsWith(source + ' · ') ? `${source} · ${name}` : name;
  }
  function open() {
    const profiles = getState().aero_profiles || [], available = models();
    openModal(tr('aero.title'), `<div class="section-tools"><span>${help(tr('aero.help'))}</span><button type="button" data-icon="plus" data-aero-add>${tr('aero.add')}</button></div><div class="settings-list">${profiles.map((profile,index) => `<div class="setting-row"><div class="aero-pair-info"><strong>${esc(profile.name)}</strong><p class="setting-help aero-pair-summary"><span>${esc(label(available.find(model => model.id === profile.planner.model)))}</span><span aria-hidden="true">→</span><span>${esc(label(available.find(model => model.id === profile.executor.model)))}</span></p></div><div class="aggregate-actions"><button type="button" class="secondary danger" data-icon="trash" data-aero-remove="${index}" title="${esc(tr('services.remove'))}" aria-label="${esc(tr('services.remove'))} ${esc(profile.name)}"></button><button type="button" class="secondary" data-icon="chevron-right" data-aero-edit="${index}" title="${esc(tr('aero.edit'))}" aria-label="${esc(tr('aero.edit'))} ${esc(profile.name)}"></button></div></div>`).join('') || `<p class="muted">${tr('aero.empty')}</p>`}</div>`, '', null, null, {reopen:open});
    const add = $('modal_body').querySelector('[data-aero-add]');
    add.disabled = !available.length; add.onclick = () => edit();
    $('modal_body').querySelectorAll('[data-aero-edit]').forEach(button => button.onclick = () => edit(Number(button.dataset.aeroEdit)));
    $('modal_body').querySelectorAll('[data-aero-remove]').forEach(button => button.onclick = () => {
      const profile = profiles[Number(button.dataset.aeroRemove)];
      openModal(tr('services.remove'), `<p>${esc(profile.name)}</p>`, tr('services.remove'), async () => {
        const candidate = cloneState(); candidate.aero_profiles = candidate.aero_profiles.filter(entry => entry.id !== profile.id);
        await persistState(tr('aero.saved'), candidate); open();
      }, null, {back:open});
    });
  }
  function edit(index) {
    const available = models(), profile = getState().aero_profiles?.[index];
    const id = profile?.id || 'aero/' + crypto.randomUUID();
    const selected = {planner:profile?.planner.model || available[0]?.id, executor:profile?.executor.model || available.at(-1)?.id};
    let customName = Boolean(profile?.name);
    openModal(profile ? tr('aero.edit') : tr('aero.add'), `<div class="aero-editor"><label class="field" for="aero_name"><span>${tr('aero.name')}</span><input id="aero_name" maxlength="64" value="${esc(profile?.name || '')}" required></label><div class="aero-roles">${['planner','executor'].map(role => `<fieldset class="aero-role"><legend>${tr('aero.' + role)}</legend><label class="field" for="aero_${role}"><span>${tr('aggregate.model')}</span><select id="aero_${role}">${available.map(model => `<option value="${esc(model.id)}" ${model.id === selected[role] ? 'selected' : ''}>${esc(label(model))}</option>`).join('')}${selected[role] && !available.some(model => model.id === selected[role]) ? `<option value="${esc(selected[role])}" selected disabled>${tr('aggregate.unavailable')}</option>` : ''}</select></label><label class="field" for="aero_${role}_effort"><span>${tr('aero.effort')}</span><select id="aero_${role}_effort"></select></label></fieldset>`).join('')}</div><output class="muted aero-model-preview" id="aero_preview"></output></div>`, tr('save_display'), async () => {
      const owner = $('aero_name'), candidate = cloneState(), updated = {id, name:owner.value.trim()};
      for (const role of ['planner','executor']) {
        updated[role] = {model:$('aero_' + role).value};
        if ($('aero_' + role + '_effort').value) updated[role].effort = $('aero_' + role + '_effort').value;
      }
      candidate.aero_profiles ||= [];
      if (index === undefined) candidate.aero_profiles.push(updated); else candidate.aero_profiles[index] = updated;
      await persistState(tr('aero.saved'), candidate); if (owner.isConnected) open();
    }, null, {back:open});
    function preview() { $('aero_preview').textContent = 'aero/' + $('aero_name').value.trim(); }
    function choose(role, first) {
      const model = available.find(model => model.id === $('aero_' + role).value), effort = $('aero_' + role + '_effort');
      const previous = first ? profile?.[role].effort : effort.value;
      effort.innerHTML = `<option value="">${tr('aero.default_effort')}</option>` + (model?.reasoning_levels || []).map(value => `<option value="${esc(value)}" ${value === previous ? 'selected' : ''}>${esc(value)}</option>`).join('');
      effort.disabled = !model?.reasoning_levels?.length;
      if (!customName) $('aero_name').value = ['planner','executor'].map(key => canonicalDisplayName(available.find(model => model.id === $('aero_' + key).value)?.display_name || '')).join(' → ');
      preview();
    }
    $('aero_name').oninput = () => { customName = true; preview(); };
    for (const role of ['planner','executor']) { $('aero_' + role).onchange = () => choose(role, false); choose(role, true); }
    preview();
  }
  return {open, editProfile:id => edit((getState().aero_profiles || []).findIndex(profile => profile.id === id))};
}
