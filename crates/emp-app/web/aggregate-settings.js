function createAggregateSettings({getState, cloneState, persistState, openModal, $, esc, tr, help}) {
  function modelFamilies() {
    const families = new Map();
    for (const model of getState().catalog_models || []) {
      if (model.source_type === 'aggregate') continue;
      const family = model.family_id || model.id.split('/').at(-1);
      if (!families.has(family)) families.set(family, []);
      families.get(family).push(model);
    }
    return families;
  }
  function open(back) {
    const groups = getState().model_groups || [];
    openModal(tr('aggregate.title'), `<div class="section-tools"><button type="button" data-icon="plus" data-aggregate-add>${tr('aggregate.add')}</button></div><div class="settings-list">${groups.map((group,index) => {
      const name = getState().catalog_models.find(model => model.id === group.id)?.display_name || group.name || tr('aggregate.unavailable');
      return `<div class="setting-row"><div class="aggregate-info"><strong>${esc(name)}</strong><p class="setting-help">${tr('aggregate.' + group.policy)} · ${tr('aggregate.source_count',{count:group.sources.length})}</p></div><div class="aggregate-actions"><button type="button" class="secondary danger" data-icon="trash" data-aggregate-remove="${index}" title="${esc(tr('services.remove'))}" aria-label="${esc(tr('services.remove'))} ${esc(name)}"></button><button type="button" class="secondary" data-icon="chevron-right" data-aggregate-edit="${index}" title="${esc(tr('aggregate.edit'))}" aria-label="${esc(tr('aggregate.edit'))}"></button></div></div>`;
    }).join('') || `<p class="muted">${tr('aggregate.no_groups')}</p>`}</div>`, '', null, null, {back, reopen:() => open(back)});
    $('modal_body').querySelector('[data-aggregate-add]').onclick = () => edit(back);
    if (![...modelFamilies().values()].some(models => models.length > 1)) {
      const add = $('modal_body').querySelector('[data-aggregate-add]');
      add.disabled = true; add.title = tr('aggregate.empty');
      add.parentElement.insertAdjacentHTML('beforebegin', `<p class="muted">${tr('aggregate.empty')}</p>`);
    }
    $('modal_body').querySelectorAll('[data-aggregate-edit]').forEach(button => button.onclick = () => edit(back,Number(button.dataset.aggregateEdit)));
    $('modal_body').querySelectorAll('[data-aggregate-remove]').forEach(button => button.onclick = async () => {
      const candidate = cloneState(); candidate.model_groups.splice(Number(button.dataset.aggregateRemove),1);
      await persistState(tr('aggregate.saved'), candidate); open(back);
    });
  }
  function edit(back, index) {
    const state = getState(), group = state.model_groups?.[index];
    const id = group?.id || 'auto/' + crypto.randomUUID();
    const families = modelFamilies();
    const selectedFamily = [...families].find(([,models]) => models.some(model => group?.sources.includes(model.id)))?.[0] || [...families.keys()].find(key => families.get(key).length > 1) || '';
    openModal(group ? tr('aggregate.edit') : tr('aggregate.add'), `<div class="routing-settings aggregate-fields">
      <div class="setting-row routing-mode"><div class="collaboration-title"><label class="setting-title" for="aggregate_family">${tr('aggregate.model')}</label>${help(tr('aggregate.models_help'))}</div><select id="aggregate_family">${[...families].filter(([,models]) => models.length > 1).map(([family,models]) => {
        const name = state.catalog_families.find(entry => entry.id === family)?.display_name || models[0].display_name;
        return `<option value="${esc(family)}" data-name="${esc(name)}" ${family === selectedFamily ? 'selected' : ''}>${esc(name)} · ${tr('aggregate.source_count',{count:models.length})}</option>`;
      }).join('')}</select></div>
      <div class="setting-row routing-mode"><label class="setting-title" for="aggregate_prefix">${tr('aggregate.prefix')}</label><div class="input-action aggregate-prefix-field"><input id="aggregate_prefix" maxlength="64" value="${esc(group?.name_prefix ?? 'auto')}"><output id="aggregate_name" for="aggregate_family"></output></div></div>
    </div>
    <fieldset class="login-choice-group aggregate-policy" id="aggregate_policy"><legend><span class="collaboration-title">${tr('aggregate.policy')} ${help(tr('aggregate.help'))}</span></legend><div class="choice-grid settings-choices">${['priority','balanced','round_robin'].map(value => `<label class="choice native-key-choice"><input type="radio" name="aggregate-policy" value="${value}" ${(group?.policy || 'priority') === value ? 'checked' : ''}><span><strong>${tr('aggregate.' + value)}</strong></span></label>`).join('')}</div></fieldset>
    <div class="section-tools"><strong>${tr('aggregate.sources_title')}</strong></div><div class="aggregate-sources" id="aggregate_sources"></div>`, tr('save_display'), async () => {
      const sources = [...$('aggregate_sources').querySelectorAll('[data-aggregate-source]')].filter(row => row.querySelector('input').checked).map(row => row.dataset.aggregateSource);
      const candidate = cloneState(), updated = {id, name_prefix:$('aggregate_prefix').value, policy:$('aggregate_policy').querySelector('input:checked').value, sources};
      candidate.model_groups ||= [];
      if (index === undefined) candidate.model_groups.push(updated); else candidate.model_groups[index] = updated;
      await persistState(tr('aggregate.saved'), candidate);
      open(back);
    }, null, {back:() => open(back)});
    function sources() {
      const family = $('aggregate_family').value, available = families.get(family) || [];
      const chosen = group && family === selectedFamily ? group.sources : available.map(model => model.id);
      const ordered = [...chosen.map(id => available.find(model => model.id === id) || {id,unavailable:true}), ...available.filter(model => !chosen.includes(model.id))];
      $('aggregate_name').textContent = $('aggregate_family').selectedOptions[0]?.dataset.name || '';
      $('aggregate_sources').innerHTML = ordered.map(model => {
        const label = model.source_type === 'native' ? tr('common.native') : (model.source_type === 'account' ? state.accounts : state.providers).find(source => source.id === model.source_id)?.name || model.display_name || model.id;
        return `<div class="setting-row aggregate-source" data-aggregate-source="${esc(model.id)}"><label><input type="checkbox" ${chosen.includes(model.id) ? 'checked' : ''}> <span>${esc(label)}${model.unavailable ? `<small class="muted"> · ${tr('aggregate.unavailable')}</small>` : ''}</span></label><div class="aggregate-actions"><button type="button" class="secondary" data-icon="chevron-right" data-source-up title="${esc(tr('aggregate.up'))}" aria-label="${esc(tr('aggregate.up'))}"></button><button type="button" class="secondary" data-icon="chevron-right" data-source-down title="${esc(tr('aggregate.down'))}" aria-label="${esc(tr('aggregate.down'))}"></button></div></div>`;
      }).join('');
      $('aggregate_sources').querySelectorAll('[data-source-up],[data-source-down]').forEach(button => button.onclick = () => {
        const row = button.closest('[data-aggregate-source]');
        if (button.hasAttribute('data-source-up') && row.previousElementSibling) row.previousElementSibling.before(row);
        if (button.hasAttribute('data-source-down') && row.nextElementSibling) row.nextElementSibling.after(row);
      });
    }
    $('aggregate_family').onchange = sources;
    sources();
  }
  return {open};
}
