function compactContext(value) {
  const tokens = Number(value || 0); if (!(tokens > 0)) return '?';
  if (tokens >= 1000000) return (tokens / 1000000).toFixed(2).replace(/0+$/, '').replace(/\.$/, '') + 'M';
  if (tokens >= 1000) return Math.round(tokens / 1000) + 'K';
  return String(Math.round(tokens));
}
// Codex only uses this share of a model's window (its own default is 95%).
function usableContext(model) {
  const maximum = Number(model?.context_window || 0); const percent = Number(model?.effective_context_window_percent) || 95;
  return maximum > 0 ? Math.max(1, Math.round(maximum * Math.min(percent, 100) / 100)) : 0;
}
function canonicalDisplayName(value) { return String(value || '').replace(/^\[\s*(?:\d+(?:\.\d+)?(?:K|M)?|\?)\]\s+/, '').replace(/\s+\[\s*(?:\d+(?:\.\d+)?(?:K|M)?|\?)\]$/, ''); }

function createCatalogDisplay({getState, $, tr, t, esc, api, notice, cloneState, persistState, syncSavedCatalog, openModal, closeModal, activityIndicatorHtml, updateActivityDots}) {
  function familyPresentationFor(family) {
    const state = getState();
    const fallback = state.catalog_families.find(item => item.id === family)?.presentation;
    return state.catalog_family_presentations[family] || fallback || {catalog_alias:'', show_context:true, reasoning_summary:'auto'};
  }
  function serviceDisplayName(id, fallback = '') {
    if (id === '@native' || id === 'native') return 'Native';
    const state = getState();
    const source = id && state && [...state.providers, ...state.accounts].find(item => item.id === id || item.prefix === id || item.name === id);
    return source?.native ? 'Native' : source?.name || (fallback !== id && fallback) || tr('reports.unknown_source');
  }
  function modelDisplayName(id) {
    if (!id) return '—';
    const state = getState();
    const label = route => {
      const model = state?.catalog_models.find(item => item.id === route);
      return canonicalDisplayName(model?.display_name || model?.default_display_name || '');
    };
    const direct = label(id);
    if (direct && !/^service-[a-f0-9]{8}(?:\/|\s*·)/i.test(direct)) return direct;
    const slash = id.indexOf('/');
    if (slash < 0) return canonicalDisplayName(id);
    const prefix = id.slice(0, slash), suffix = id.slice(slash + 1);
    const provider = state?.providers.find(item => item.id === prefix || item.name === prefix);
    const account = state?.accounts.find(item => item.id === prefix || item.prefix === prefix || item.name === prefix);
    if (provider) {
      const model = state.models.find(item => item.provider === provider.id && item.upstream_id === suffix);
      const current = model && label(model.id);
      if (current && !/^service-[a-f0-9]{8}(?:\/|\s*·)/i.test(current)) return current;
      return `${serviceDisplayName(provider.id)} · ${canonicalDisplayName(model?.display_name || suffix)}`;
    }
    if (account) return `${serviceDisplayName(account.id)} · ${suffix}`;
    return /^service-[a-f0-9]{8}$/i.test(prefix) ? `${tr('reports.unknown_source')} · ${suffix}` : canonicalDisplayName(id);
  }
  function presentationName(route, fallback) {
    const state = getState();
    const family = state.catalog_families.find(item => item.routes.some(candidate => candidate.id === route));
    const saved = family ? familyPresentationFor(family.id) : state.catalog_presentations[route];
    return saved?.catalog_alias || canonicalDisplayName(fallback) || route;
  }
  function updateCatalogDisplayControls() {
    const showContext = getState()?.catalog_show_context !== false;
    const contextButton = $('catalog_context_toggle');
    const contextLabel = showContext ? tr("core.hide_context_labels") : tr("core.show_context_labels");
    contextButton.setAttribute('aria-pressed', String(showContext));
    contextButton.setAttribute('aria-label', contextLabel);
    contextButton.title = contextLabel;
    contextButton.disabled = catalogContextPreferenceBusy;
    contextButton.querySelector('[data-catalog-eye="open"]').toggleAttribute('hidden', !showContext);
    contextButton.querySelector('[data-catalog-eye="closed"]').toggleAttribute('hidden', showContext);
    const searchButton = $('catalog_search_toggle');
    const searchShell = $('catalog_search_shell');
    const searchInput = $('catalog_display_search');
    const expanded = searchShell.classList.contains('is-open');
    searchButton.setAttribute('aria-expanded', String(expanded));
    const label = expanded ? tr("core.close_model_search") : tr("search_models");
    searchButton.setAttribute('aria-label', label);
    searchButton.title = label;
    searchInput.setAttribute('aria-label', tr("search_models"));
    searchInput.placeholder = tr("search_models");
  }
  function toggleCatalogSearch(expanded) {
    const shell = $('catalog_search_shell');
    const input = $('catalog_display_search');
    const open = expanded === undefined ? !shell.classList.contains('is-open') : Boolean(expanded);
    if (open) {
      input.inert = false;
      input.removeAttribute('aria-hidden');
      input.tabIndex = 0;
      shell.classList.add('is-open');
      updateCatalogDisplayControls();
      input.focus();
    } else {
      input.value = '';
      input.inert = true;
      input.setAttribute('aria-hidden', 'true');
      input.tabIndex = -1;
      shell.classList.remove('is-open');
      renderCatalogDisplay();
      $('catalog_search_toggle').focus();
    }
  }
  function catalogSearchKeydown(event) {
    if (event.key !== 'Escape') return;
    event.preventDefault();
    toggleCatalogSearch(false);
  }
  let catalogContextPreferenceBusy = false;
  async function toggleCatalogContext() {
    if (catalogContextPreferenceBusy) return;
    const previous = getState().catalog_show_context !== false;
    const next = !previous;
    getState().catalog_show_context = next;
    catalogContextPreferenceBusy = true;
    renderCatalogDisplay();

    try {
      const saved = await api('/api/catalog/context-preference', {method:'POST', body:JSON.stringify({catalog_show_context:next})});
      getState().catalog_show_context = saved.catalog_show_context;
      renderCatalogDisplay();

      notice(tr("core.context_label_preference_saved"));
      void syncSavedCatalog();
    } catch (error) {
      getState().catalog_show_context = previous;
      renderCatalogDisplay();

      notice(error.message, true);
    } finally {
      catalogContextPreferenceBusy = false;
      updateCatalogDisplayControls();
    }
  }
  function catalogSourceLabels(route) {
    const state = getState();
    const id = route.id;
    const group=state.model_groups.find(item=>item.id===id);
    if(group)return group.sources.flatMap(id=>catalogSourceLabels({id}));
    const provider=state.providers.find(item=>id.startsWith(item.id+'/'));
    if(provider)return [provider.name || tr("core.unnamed_service")];
    const account=state.accounts.find(item=>item.prefix && id.startsWith(item.prefix+'/'));
    if(account)return [account.name || tr("core.subscription_account")];
    return [id.includes('/') ? tr("core.unlinked_service") : 'Native'];
  }
  function renderCatalogDisplay() {
    updateCatalogDisplayControls();
    const state = getState();
    const all = state.catalog_families;
    const entries = new Map(state.catalog_models.map(model => [model.id, model]));
    const query = $('catalog_display_search').value.trim().toLowerCase();
    const matching = all.filter(model => !query || `${model.id} ${model.default_display_name} ${model.display_name} ${model.routes.map(route => `${route.id} ${entries.get(route.id)?.display_name || ''}`).join(' ')}`.toLowerCase().includes(query));
    const order = new Map(all.filter(model => model.routes[0]?.source_type !== 'aggregate').map((model, index) => [model.id, index]));
    const familyOrder = model => order.get(entries.get(model.routes[0]?.id)?.family_id || model.id) ?? all.length;
    const visible = matching.toSorted((left, right) => familyOrder(left) - familyOrder(right) || Number(left.routes[0]?.source_type === 'aggregate') - Number(right.routes[0]?.source_type === 'aggregate'));
    if (!all.length) { $('catalog_display_models').innerHTML = `<p class="muted">${esc(t('no_models'))}</p>`; return; }
    if (!visible.length) { $('catalog_display_models').innerHTML = `<p class="muted">${esc(t('no_matches'))}</p>`; return; }
    $('catalog_display_models').innerHTML = visible.map(model => {
      const routes = model.routes;
      const route = routes[0]?.id || model.id;
      const name = presentationName(route, model.default_display_name || model.display_name || modelDisplayName(model.id));
      const displayName = state.catalog_show_context !== false ? `[${compactContext(model.context_window).padStart(5, ' ')}]  ${name}` : name;
      const aggregate = routes[0]?.source_type === 'aggregate';
      const sources = [...new Set(routes.flatMap(catalogSourceLabels))].join(' · ');
      const subtitle = aggregate ? `${tr('aggregate.source_pool')} · ${sources}` : sources || modelDisplayName(model.id);
      const content = `<div class="catalog-route-title">${activityIndicatorHtml('models', '', displayName, aggregate ? [route] : routes.map(item => item.id))}<strong>${esc(displayName)}</strong></div><code>${esc(subtitle)}</code>`;
      return `<div class="display-row"><div class="route">${content}</div><button type="button" class="secondary" data-ui-action="catalog-display-editor" data-id="${esc(model.id)}" aria-label="${esc(tr("core.edit_model_display"))}">›</button></div>`;
    }).join('');
    updateActivityDots();
  }
  function openCatalogDisplayEditor(family) {
    const group = getState().catalog_families.find(item => item.id === family); if (!group) return;
    const presentation = familyPresentationFor(family);
    const routes = [...new Set(group.routes.flatMap(catalogSourceLabels))].join(' · ');
    openModal(tr("core.edit_model_display"), `<p><strong>${esc(group.default_display_name || group.display_name || family)}</strong><br><code>${esc(routes)}</code></p><label>${esc(tr("core.display_name"))}<input id="modal_catalog_alias" value="${esc(presentation.catalog_alias || '')}" placeholder="${esc(group.default_display_name || family)}"></label><p class="muted">${esc(tr("core.the_new_name_takes_effect_immediately_after_saving"))}</p>`, tr("core.done"), async () => {
      const owner = $('modal_body').firstElementChild;
      const candidate = cloneState();
      const current = familyPresentationFor(family);
      const presentation = {...current, catalog_alias:$('modal_catalog_alias').value.trim()};
      if (group.routes[0].source_type === 'aggregate') candidate.catalog_presentations[family] = presentation;
      else {
        candidate.catalog_family_presentations[family] = presentation;
        for (const route of group.routes) delete candidate.catalog_presentations[route.id];
      }
      await persistState(tr("core.model_display_saved"), candidate);
      if (owner?.isConnected) closeModal();
    });
  }

  return {render:renderCatalogDisplay, openEditor:openCatalogDisplayEditor, toggleContext:toggleCatalogContext, toggleSearch:toggleCatalogSearch, searchKeydown:catalogSearchKeydown, presentationName, modelName:modelDisplayName, serviceName:serviceDisplayName};
}
