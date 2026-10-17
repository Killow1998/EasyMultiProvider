// Shared call facts and formatting for usage, performance and request details.
function createCallReports({api, getActivity, getState, openModal, $, esc, tr, getLanguage, periodPickerHtml, registerPeriodPicker}) {
  let current = null, timer = null, disposeOutcomes = () => {};
  const n = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
  const number = value => n(value) === null ? '—' : value.toLocaleString();
  const seconds = value => n(value) === null ? '—' : (value / 1000).toFixed(2) + ' s';
  const tps = value => n(value) === null ? '—' : value.toFixed(1) + ' token/s';
  const rate = (cached, input) => n(cached) !== null && n(input) > 0 && cached <= input ? (cached / input * 100).toFixed(1) + '%' : '—';
  const status = value => ({active:tr("reports.responding"),completed:tr("reports.completed_2"),failed:tr("reports.failed_2"),cancelled:tr("reports.cancelled"),interrupted:tr("reports.interrupted_2"),disconnected:tr("reports.disconnected_2"),recovery_required:tr("reports.history_resend_requested"),unknown:tr("reports.outcome_not_recorded")})[value] || '—';
  const evidence = value => ({same:tr("reports.names_match"),mapped:tr("reports.configured_mapping"),different:tr("reports.names_differ"),conflict:tr("reports.conflicting_declarations"),missing:tr("reports.not_reported")})[value] || tr("reports.not_reported");
  const time = value => n(value) === null ? '—' : new Date(value * 1000).toLocaleString(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {hour12:false});
  const {outcomes,bindOutcomes,errorSource} = createCallOutcomes({esc,tr,number,time});
  const definition = label => label === 'TTFT' ? tr("reports.time_from_operation_start_to_the_first_reasoning_text") : label === 'TPS' ? tr("reports.output_tokens_divided_by_the_full_operation_duration_including") : '';
  const field = (label, value, outcome = false) => `<div class="call-fact"${outcome ? ' data-call-outcomes tabindex="0"' : ''}><span title="${esc(definition(label))}">${esc(label)}</span><strong>${esc(value)}</strong></div>`;
  function recordHtml(row) {
    const session = row.session_name || (row.thread_id || row.session_id || '').slice(0,12) || tr("reports.unlinked");
    const selected = row.selected_name || row.client_model || row.model_id || '—';
    const operation = ({image_generation:tr("reports.image_generation"),image_edit:tr("reports.image_edit"),agent_task_recovery:tr("reports.agent_task_recovery")})[row.route];
    const returned = row.response_model || (row.state === 'active' ? tr("reports.awaiting_response") : '—');
    const models = [operation ? field(tr("reports.operation"),operation) : '',field(operation ? tr("reports.image_model") : tr("reports.selected_in_codex"), selected),field(tr("reports.sent_upstream"),row.upstream_model || '—'),field(tr("reports.reported_upstream"),returned)].join('');
    const inputs = n(row.input_tokens), cached = n(row.cached_input_tokens), written = n(row.cache_write_tokens);
    const uncached = inputs !== null && cached !== null && written !== null && cached + written <= inputs ? inputs - cached - written : null;
    const usage = [field(tr("reports.input_tokens"),number(inputs)),field(tr("reports.output_tokens"),number(row.output_tokens)),field(tr("reports.cache_read"),number(cached)),field(tr("reports.cache_write"),number(written)),field(tr("reports.uncached_input"),number(uncached)),field(tr("reports.reasoning_tokens"),number(row.reasoning_tokens)),field(tr("reports.input_cache_rate"),rate(cached,inputs)),field(tr("reports.output_cache_rate"),tr("reports.not_applicable")),field(tr("reports.api_estimate_usd"),n(row.pricing?.cost_nanos) === null ? '—' : (row.pricing.cost_nanos / 1e9).toFixed(6))].join('');
    const performance = [field(tr("reports.duration"),seconds(row.duration_ms)),field('TTFT',seconds(row.ttft_ms)),field(tr("reports.first_text_tool_arguments"),seconds(row.first_content_ms)),field('TPS',tps(row.tokens_per_second)),field(tr("reports.attempts"),number(row.attempts))].join('');
    const declarations = (row.model_declarations || []).map(item => `<li><code>${esc(item.model)}</code> · ${esc(item.source)}${item.terminal ? ' · '+tr("reports.terminal") : ''}</li>`).join('');
    const retries = (row.retries || []).map(item => `<li>${esc(time(item.at))} · ${esc(item.reason)} · ${esc(item.status)}</li>`).join('');
    return `<details class="call-record" data-call-id="${esc(row.request_id+':'+(row.route || 'responses'))}"><summary><span class="call-session">${esc(session)}<small>${esc(time(row.started_at))}</small></span><span class="call-model">${esc(selected)}<small>${operation ? esc(operation)+' · ' : ''}${tr("reports.reported")}: ${esc(returned)}</small></span><span>${esc(status(row.state))}</span><span>${number(inputs)} / ${number(row.output_tokens)}<small>${tr("reports.input_output")}</small></span><span>${rate(cached,inputs)}<small>${tr("reports.cache_rate")}</small></span><span>${seconds(row.duration_ms)}<small>TTFT ${seconds(row.ttft_ms)} · TPS ${tps(row.tokens_per_second)}</small></span></summary><section class="call-detail"><h3>${tr("reports.session_and_models")}</h3><div class="call-facts">${field(tr("reports.session"),session)}${field(tr("reports.session_id"),row.thread_id || row.session_id || '—')}${field(tr("reports.parent_session"),row.parent_thread_id || '—')}${field(tr("reports.turn"),row.turn_id || '—')}${models}${field(tr("reports.model_declaration"),evidence(row.model_name_status))}</div><h3>${tr("usage")}</h3><div class="call-facts">${usage}</div><h3>${tr("diagnostics")}</h3><div class="call-facts">${performance}</div><details class="call-technical"><summary>${tr("reports.technical_details")}</summary><div class="call-facts">${field(tr("reports.request_id"),row.request_id)}${field(tr("reports.service"),row.provider_name || row.account_id || row.provider_id || '—')}${field(tr("reports.transport"),row.transport || '—')}${field(tr("reports.protocol"),row.resolved_protocol || '—')}${field(tr("reports.upstream_status"),row.response_status || status(row.state))}${field(tr("reports.delivery"),row.delivery?.delivery || '—')}${field(tr("reports.error_source"),errorSource(row.error_origin))}${field(tr("reports.failure_reason"),row.failure_reason || row.error_code || row.error_class || '—')}</div>${declarations ? '<ul>'+declarations+'</ul>' : ''}${retries ? '<h3>'+tr("reports.retries")+'</h3><ol>'+retries+'</ol>' : ''}</details></section></details>`;
  }
  function toolbar() {
    const state = getState() || {};
    const services = [['',tr("reports.all_services")],['account:@native','Native'],...(state.accounts || []).filter(row => !row.native).map(row => ['account:'+row.id,row.name || row.id]),...(state.providers || []).map(row => ['provider:'+row.id,row.name || row.id])];
    const models = (state.models || []).map(row => [row.id,row.display_name || row.id]);
    if (current.filters.model && !models.some(([id]) => id === current.filters.model)) models.push([current.filters.model,current.filters.model]);
    const options = values => values.map(([value,label]) => `<option value="${esc(value)}">${esc(label)}</option>`).join('');
    return `${periodPickerHtml('calls', ['1d','7d','30d'])}<div class="call-toolbar"><select data-call-filter="service" aria-label="${tr("reports.service")}">${options(services)}</select><select data-call-filter="category" aria-label="${tr("reports.source")}">${options([['',tr("reports.all_sources")],['native','Native'],['subscription',tr("services.subscription")],['external','API'],['unknown',tr("reports.unlinked")]])}</select><select data-call-filter="model" aria-label="${tr("reports.model")}">${options([['',tr("reports.all_models")],...models])}</select><select data-call-filter="state" aria-label="${tr("reports.status")}">${options([['',tr("reports.all_states")],['completed',tr("reports.completed_2")],['failed',tr("reports.failed_2")],['interrupted',tr("reports.interrupted_2")],['cancelled',tr("reports.disconnected_2")],['recovery_required',tr("reports.history_resend_requested")],['unknown',tr("reports.outcome_not_recorded")]])}</select><input data-call-filter="session" placeholder="${tr("reports.session_id")}" aria-label="${tr("reports.session_id")}"></div><div class="report-tabs" role="group" aria-label="${tr("reports.view")}">${current.performance ? ['overview','models','calls'].map((view,index) => `<button type="button" class="secondary" data-call-view="${view}">${[tr("reports.overview"),tr("models_routes"),tr("reports.calls")][index]}</button>`).join('') : ''}</div><div class="report-status" data-report-status role="status" aria-live="polite"></div><div data-call-content></div>`;
  }
  const performance = createPerformanceView({esc,tr,getLanguage,number,seconds,tps,rate,outcomes,bindOutcomes,definition});
  function summaryHtml(report) { return performance.html(report,current?.metric || 'calls'); }
  function modelHtml(report) {
    const rows = (report.models || []).map(row => `<tr><td><code>${esc(row.model_id)}</code><small>${esc(row.account_id || row.provider_id)} · ${esc(row.upstream_model)} · ${esc(row.speed_mode)} · ${esc(row.requested_effort)}</small></td><td>${number(row.calls)}</td><td>${seconds(row.duration_ms)}</td><td>${seconds(row.ttft_ms)}</td><td>${tps(row.tokens_per_second)}</td></tr>`).join('');
    return `<div class="usage-table"><table><thead><tr>${[['model',tr("reports.model_service")],['calls',tr("reports.calls")],['duration',tr("reports.mean_duration")],['ttft','TTFT'],['tps','TPS']].map(([key,label]) => '<th><button type="button" class="secondary" data-call-sort="'+key+'">'+label+(current.modelsSort === key ? (key === 'model' ? ' ↑' : ' ↓') : '')+'</button></th>').join('')}</tr></thead><tbody>${rows}</tbody></table></div><div class="call-pages"><button type="button" class="secondary" data-call-action="models-previous" ${report.models_offset ? '' : 'disabled'}>‹</button><span>${number(report.models_total)} ${tr("reports.groups")}</span><button type="button" class="secondary" data-call-action="models-next" ${report.models_offset + report.models_limit < report.models_total ? '' : 'disabled'}>›</button></div>`;
  }
  function matches(row, filters) {
    return (!filters.category || row.usage_category === filters.category) && (!filters.provider || row.provider_id === filters.provider) && (!filters.account || row.account_id === filters.account) && (!filters.model || row.model_id === filters.model) && (!filters.models?.length || filters.models.includes(row.model_id)) && (!filters.session || (row.thread_id || row.session_id) === filters.session) && (!filters.state || row.state === filters.state);
  }
  function render(target, report) {
    if (target.ownerDocument.querySelector('.call-outcome-dialog[open]')) return;
    const focused = target.ownerDocument?.activeElement;
    const focus = focused && target.contains(focused) && ['callMetric','callSort'].find(key => focused.dataset[key]);
    const open = new Set([...target.querySelectorAll('details[data-call-id][open]')].map(node => node.dataset.callId));
    const scrollLeft = target.querySelector('.call-records')?.scrollLeft || 0;
    const active = current.offset === 0 ? (getActivity()?.requests || []).filter(row => row.state === 'active' && matches(row,current.filters) && row.started_at >= report.start && row.started_at <= report.end) : [];
    const ids = new Set(active.map(row => row.request_id));
    const rows = [...active,...(report.records || []).filter(row => !ids.has(row.request_id))];
    const pages = `<div class="call-pages"><button type="button" class="secondary" data-call-action="previous" ${report.offset ? '' : 'disabled'}>‹</button><span>${number(report.total)} ${tr("reports.records")}</span><button type="button" class="secondary" data-call-action="next" ${report.offset + report.limit < report.total ? '' : 'disabled'}>›</button></div>`;
    $(current.id).querySelectorAll('[data-call-view]').forEach(button => {
      const active = button.dataset.callView === current.view;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    });
    const content = current.view === 'models' ? modelHtml(report) : current.view === 'overview' ? summaryHtml(report) : `<div class="call-records">${rows.map(recordHtml).join('')}</div>` + pages;
    disposeOutcomes();
    performance.stop();
    target.innerHTML = (rows.length || current.view !== 'calls' ? content : `<p class="muted">${tr("reports.no_calls_recorded")}</p>${pages}`);
    const records = target.querySelector('.call-records'); if (records) records.scrollLeft = scrollLeft;
    disposeOutcomes = () => {};
    if(current.view === 'overview') performance.bind(target,report,current.metric || 'calls');
    target.querySelectorAll('details[data-call-id]').forEach(node => { node.open = open.has(node.dataset.callId); });
    if (focus) target.querySelectorAll('button').forEach(button => {
      if (button.dataset[focus] === focused.dataset[focus]) button.focus();
    });
  }
  function load() {
    if (!current || !$(current.id)) return;
    const {models,...filters} = current.filters;
    const query = new URLSearchParams({...filters,offset:current.offset,limit:50,models_offset:current.modelsOffset,models_sort:current.modelsSort});
    for (const model of models || []) query.append('models',model);
    return current.query.run('/api/calls?'+query);
  }
  function refresh() {
    if (!current || !$(current.id) || timer !== null) return;
    timer = setTimeout(() => { timer = null; void load(); },200);
  }
  function live() { if (current?.report && $(current.id)) render($(current.id).querySelector('[data-call-content]'),current.report); }
  function stop() { performance.stop(); disposeOutcomes(); disposeOutcomes = () => {}; current?.query.cancel(); current = null; if (timer !== null) clearTimeout(timer); timer = null; }
  function mount(id, filters = {}, performance = false, report = null) {
    // Embedded statistics already fetched the first page with these filters.
    if (report && current?.id === id) {
      if (JSON.stringify(current.filters) !== JSON.stringify(filters)) {
        current.query.cancel();
        current.filters = {...filters}; current.offset = 0; current.modelsOffset = 0;
      }
      if (current.offset === 0) {
        current.report = report;
        render($(id).querySelector('[data-call-content]'),report);
      } else if (current.seedReport !== report) void load();
      current.seedReport = report;
      return;
    }
    stop();
    const root = $(id); if (!root) return;
    current = {id,filters:{...filters},offset:0,modelsOffset:0,modelsSort:'calls',view:performance ? 'overview' : 'calls',performance};
    root.innerHTML = report ? '<div class="report-status" data-report-status role="status" aria-live="polite"></div><div data-call-content></div>' : toolbar();
    const selected = current;
    current.query = createReportQuery({api,
      onState:(loading,error) => setReportState(root,loading,error,tr),
      onResult:report => { selected.report = report; render(root.querySelector('[data-call-content]'),report); },
    });
    if (!report) {
      for (const field of ['model','state','session','category']) { if (filters[field]) root.querySelector(`[data-call-filter="${field}"]`).value = filters[field]; }
      root.querySelector('[data-call-filter="service"]').value = filters.provider ? 'provider:'+filters.provider : filters.account ? 'account:'+filters.account : '';
    }
    root.onchange = event => {
      if (!current || current.id !== id) return;
      const field = event.target.dataset.callFilter; if (!field) return;
      const value = event.target.value;
      current.offset = 0; current.modelsOffset = 0;
      if (field === 'service') { delete current.filters.provider; delete current.filters.account; if (value) { const index = value.indexOf(':'); current.filters[value.slice(0,index)] = value.slice(index+1); } }
      else if (value) current.filters[field] = value; else delete current.filters[field];
      void load();
    };
    root.onclick = event => {
      const button = event.target.closest('[data-call-action],[data-call-view],[data-call-metric],[data-call-sort]'); if (!button || !current) return;
      if (button.dataset.callSort) { current.modelsSort=button.dataset.callSort; current.modelsOffset=0; void load(); return; }
      if (button.dataset.callMetric) { current.metric = button.dataset.callMetric; if (current.report) render(root.querySelector('[data-call-content]'),current.report); return; }
      if (button.dataset.callView) { current.view = button.dataset.callView; if (current.report) render(root.querySelector('[data-call-content]'),current.report); return; }
      if (button.dataset.callAction === 'models-next') current.modelsOffset += 50;
      if (button.dataset.callAction === 'models-previous') current.modelsOffset = Math.max(0,current.modelsOffset-50);
      if (button.dataset.callAction === 'next') current.offset += 50;
      if (button.dataset.callAction === 'previous') current.offset = Math.max(0,current.offset-50);
      void load();
    };
    if (report) { current.report = report; current.seedReport = report; render(root.querySelector('[data-call-content]'),report); return; }
    const range = filters.start !== undefined || filters.end !== undefined
      ? {start:filters.start ?? 0,end:filters.end ?? Date.now()/1000} : null;
    registerPeriodPicker('calls','1d', period => {
      current.filters.start = period.start; current.filters.end = period.end;
      current.offset = 0; current.modelsOffset = 0;
      return load();
    },range);
  }
  function open(filters = {}, navigation = {}) { openModal(tr("reports.call_details"),'<div id="call_report"></div>','',null,null,navigation); mount('call_report',filters); }
  function openActivity(element) {
    const id = element.dataset.activityId || '';
    const kind = element.dataset.activityKind;
    const filters = kind === 'provider' ? {provider:id} : kind === 'account' ? {account:id} : {model:id};
    if (kind === 'models') {
      let ids = [];
      try { ids = JSON.parse(element.dataset.activityModels || '[]'); } catch (_) {}
      filters.models = Array.isArray(ids) ? ids.slice(0,64).filter(id => typeof id === 'string') : [];
      delete filters.model;
    }
    open(filters);
  }
  function normalize(value) {
    if (!Array.isArray(value)) return [];
    return value.slice(0,128).filter(record => record && typeof record.model_id === 'string' && typeof record.request_id === 'string')
      .map(record => ({...record,retries:Array.isArray(record.retries) ? record.retries.slice(0,16) : []}));
  }
  return {open,openActivity,normalize,mount,refresh,live,stop,recordHtml,number,seconds,tps,rate,outcomes,bindOutcomes};
}
