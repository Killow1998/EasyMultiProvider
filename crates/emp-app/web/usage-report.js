// Owns data and outstanding work for one usage window.
function createUsageReport({api, $, tr, esc, getState, getLanguage, persistState, openModal,
  serviceUsage, callReports, eventsConnected, periods}) {
  const {periodPickerHtml, registerPeriodPicker, setPeriod, periodPickers, periodLocalInput, timeAxisSvg} = periods;
  let usageQuery = null, usagePayload = null, usagePoll = null, scanController = null;
  const usageSource = value => ({native:'Native', subscription:tr("reports.other_subscription"), external:'External Provider', unknown:tr("reports.unknown_source")}[value] || value);
  const usageMoney = nanos => new Intl.NumberFormat(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {style:'currency', currency:'USD', minimumFractionDigits:2, maximumFractionDigits:4}).format(nanos / 1e9);
  const usageNumber = value => new Intl.NumberFormat(getLanguage() === 'en' ? 'en-US' : 'zh-CN').format(value || 0);
  function usageBucketFor(period) { const span = period.end - period.start; return period.preset === 'all' || span > 3*86400 ? 'day' : span > 3*3600 ? 'hour' : 'minute'; }
  function openUsage(selection = {}) {
    usagePayload = null;
    openModal(tr("reports.usage_api_estimates"), `<div id="usage_report">
      ${periodPickerHtml('usage', ['1h','1d','7d','all'])}
      <div class="usage-filters"><label>${tr("reports.source")}<select id="usage_category" data-usage-change="load"><option value="all">${tr("reports.all_sources")}</option><option value="native">${tr("common.native")}</option><option value="subscription">${tr("reports.other_subscription")}</option><option value="external">${tr("common.external_provider")}</option><option value="unknown">${tr("reports.unknown_source")}</option></select></label><label>${tr("reports.group_by")}<select id="usage_bucket" data-usage-change="render"><option value="minute">${tr("reports.minute")}</option><option value="hour">${tr("reports.hour")}</option><option value="day">${tr("reports.day")}</option></select></label><button type="button" class="secondary" data-report-action="scan" data-usage-action="scan">${tr("reports.scan_history")}</button><button type="button" class="secondary" data-usage-action="calls">${tr("reports.call_details")}</button></div>
      <div id="usage_status" class="report-status" data-report-status role="status" aria-live="polite"></div><div id="usage_result"></div>
      <p class="usage-notice">${tr("reports.scans_local_codex_history_and_records_emp_usage_across")}</p></div>`, '', null);
    const root = $('usage_report');
    root.onchange = event => { if (event.target.dataset.usageChange === 'load') loadUsage(); else if (event.target.dataset.usageChange === 'render') renderUsage(); };
    root.onclick = event => {
      const target = event.target.closest('[data-usage-action]'); if (!target) return;
      const action = target.dataset.usageAction;
      if (action === 'scan') scanUsageHistory(); else if (action === 'calls') openUsageCalls();
      else if (action === 'pricing') saveUsagePricing(); else if (action === 'zoom') zoomUsage(Number(target.dataset.start),Number(target.dataset.end));
    };
    root.onkeydown = event => { if (['Enter',' '].includes(event.key) && event.target.dataset.usageAction === 'zoom') { event.preventDefault(); event.target.click(); } };
    usageQuery = createReportQuery({api,
      onState:(loading,error) => setReportState(root,loading,error,tr),
      onResult:result => {
        if (periodPickers.usage.preset === 'all' && result.first_record_at) {
          const first = new Date(result.first_record_at*1000); first.setHours(0,0,0,0);
          result.start = Math.max(0,first.getTime()/1000); $('usage_start').value = periodLocalInput(first);
        }
        usagePayload = result; renderUsage();
        if (!eventsConnected() && (result.history?.running || result.history?.queued)) usagePoll = setTimeout(() => { if ($('usage_report') === root) loadUsage(); },1500);
      },
    });
    $('usage_category').value = selection.category || 'all';
    $('usage_bucket').value = selection.bucket || 'hour';
    registerPeriodPicker('usage', '1d', (period, options) => { if (options.user && $('usage_bucket')) $('usage_bucket').value = usageBucketFor(period); return loadUsage(); }, selection.period);
    if (selection.bucket) $('usage_bucket').value = selection.bucket;
  }
  function openUsageCalls() {
    const period = periodPickers.usage;
    const category = $('usage_category')?.value;
    const filters = period ? {start:period.start,end:period.end} : {};
    if (category && category !== 'all') filters.category = category;
    const selection = {category, bucket:$('usage_bucket').value, period:period && {...period}};
    callReports.open(filters, {back:() => openUsage(selection)});
  }
  async function scanUsageHistory() {
    const root = $('usage_report'); if (!root || root.getAttribute('aria-busy') === 'true') return;
    scanController = new AbortController();
    const controller = scanController;
    setReportState(root,true,'',tr);
    try {
      await api('/api/usage/scan', {method:'POST',body:'{}',signal:controller.signal});
      if ($('usage_report') === root) await loadUsage();
    } catch (error) { if ($('usage_report') === root && error.name !== 'AbortError') setReportState(root,false,error.message,tr); }
    finally { if (scanController === controller) scanController = null; }
  }
  function loadUsage() {
    clearTimeout(usagePoll);
    const root = $('usage_report'), period = periodPickers.usage;
    if (!root || !period || !usageQuery) return;
    const {start,end} = period;
    if (!Number.isFinite(start) || !Number.isFinite(end) || start >= end) {
      usageQuery.cancel(); setReportState(root,false,tr("core.choose_a_valid_period"),tr); return;
    }
    return usageQuery.run('/api/usage?' + new URLSearchParams({start,end,category:$('usage_category').value}));
  }
  function stopUsage() { clearTimeout(usagePoll); scanController?.abort(); scanController = null; usageQuery?.cancel(); usageQuery = null; usagePayload = null; }

  function usagePeriods() {
    const groups = new Map(), bucket = $('usage_bucket').value;
    for (const row of usagePayload.periods) {
      const date = new Date(row.start*1000); if (bucket === 'day') date.setHours(0,0,0,0); else if (bucket === 'hour') date.setMinutes(0,0,0); else date.setSeconds(0,0);
      const start = date.getTime()/1000, until = new Date(date); if (bucket === 'day') until.setDate(until.getDate()+1); else until.setTime(until.getTime()+(bucket === 'hour' ? 3600000 : 60000));
      if (!groups.has(start)) groups.set(start, {start,end:until.getTime()/1000,input_tokens:0,output_tokens:0,cost_nanos:0,requests:0,priced_requests:0});
      const group = groups.get(start); for (const key of ['input_tokens','output_tokens','cost_nanos','requests','priced_requests']) group[key] += row[key];
    }
    return [...groups.values()].sort((a,b) => a.start-b.start);
  }
  function zoomUsage(start,end) {
    setPeriod('usage', {preset:'', start:Math.max(start,usagePayload.start), end:Math.min(end,usagePayload.end)}, {user:true});
  }
  // History rows only know the route they were recorded under, so that route is the source.
  const usageRowSource = row => row.category === 'unknown' && row.owner.startsWith('history:') ? tr("reports.historical_route") : usageSource(row.category);
  function usageOwner(row) {
    if (row.owner.startsWith('history:')) return row.category === 'unknown' ? row.owner.slice(8) : tr("reports.historical_route_2") + row.owner.slice(8);
    if (row.category === 'external') return row.owner_name || row.owner;
    if (row.owner.startsWith('account:')) return row.owner_name || tr("reports.account") + row.owner.slice(8,16);
    if (row.owner.startsWith('credential:')) return (row.owner_name || row.owner.slice(11,19)) + tr("reports.identity_unconfirmed");
    return tr("reports.legacy_record_or_route_account_identity_unconfirmed");
  }
  // Models without a public price cost 0 unless they are priced as another model.
  function usagePricingEditor(data) {
    const aliases = getState()?.pricing_aliases || {};
    const counts = new Map((data.unpriced_models || []).map(row => [row.route_model, row.requests]));
    const models = [...new Set([...counts.keys(), ...Object.keys(aliases)])].filter(Boolean);
    if (!models.length) return '';
    const rows = models.map(model => `<tr><td><code>${esc(model)}</code>${counts.has(model) ? `<small>${usageNumber(counts.get(model))} ${tr("reports.requests_counted_as_0")}</small>` : ''}</td><td><input data-pricing-alias="${esc(model)}" value="${esc(aliases[model] || '')}" placeholder="${tr("reports.price_as_e_g_gpt_5_5")}"></td></tr>`).join('');
    return `<details class="usage-notice"><summary>${tr("reports.pricing_references")}${counts.size ? ` · ${counts.size} ${tr("reports.models_have_no_public_price_and_count_as_0")}` : ''}</summary><p class="muted">${tr("reports.optionally_choose_a_public_model_to_estimate_these_with")}</p><table class="pricing-aliases">${rows}</table><div class="toolbar"><button class="secondary" data-icon="save" data-usage-action="pricing">${tr("reports.save_pricing_references")}</button></div></details>`;
  }
  async function saveUsagePricing() {
    const root = $('usage_report'); if (!root) return;
    const aliases = {...(getState().pricing_aliases || {})};
    root.querySelectorAll('[data-pricing-alias]').forEach(input => {
      const value = input.value.trim();
      if (value) aliases[input.dataset.pricingAlias] = value; else delete aliases[input.dataset.pricingAlias];
    });
    const candidate = structuredClone(getState()); candidate.pricing_aliases = aliases;
    try { await persistState(tr("reports.pricing_references_saved_re_pricing"), candidate); }
    catch (error) { if ($('usage_report') === root) setReportState(root,false,error.message,tr); return; }
    if ($('usage_report') !== root) return;
    clearTimeout(usagePoll);
    usagePoll = setTimeout(() => { if ($('usage_report') === root) loadUsage(); },1500);
  }
  function renderUsage() {
    const box = $('usage_result'); if (!box || !usagePayload) return;
    const data = usagePayload, totals = data.totals, periods = usagePeriods();
    const cost = row => row.priced_requests ? usageMoney(row.cost_nanos) : '—';
    const price = data.pricing, missing = totals.requests - totals.priced_requests;
    const max = Math.max(1,...periods.map(row=>row.input_tokens+row.output_tokens));
    const width = 720, plot = 628, range = data.end-data.start;
    const bars = periods.map(row => {
      const x = 70 + (Math.max(data.start,row.start)-data.start)/range*plot;
      const w = Math.max(1,(Math.min(data.end,row.end)-Math.max(data.start,row.start))/range*plot-2);
      const tokens = row.input_tokens+row.output_tokens, h = tokens/max*130;
      const label = new Date(row.start*1000).toLocaleString() + ' · ' + usageNumber(tokens) + ' tokens · ' + cost(row);
      return `<rect x="${x}" y="${150-h}" width="${w}" height="${Math.max(1,h)}" rx="2" tabindex="0" role="button" aria-label="${esc(label)}" data-usage-action="zoom" data-start="${Number(row.start)}" data-end="${Number(row.end)}"><title>${esc(label)}</title></rect>`;
    }).join('');
    const groups = serviceUsage.renderGroups(data, getState()?.providers, row => usageRowSource(row) + ' · ' + usageOwner(row));
    const history = data.history || {}, historical = data.sources?.find(row => row.origin === 'history')?.requests || 0;
    box.innerHTML = `<p class="usage-notice">${history.running || history.queued ? tr("reports.scanning_history_in_the_background") : history.last_scan_at ? tr("reports.history_scanned") + usageNumber(history.files) + tr("reports.files") : tr("reports.waiting_for_history_scan")}${history.errors ? ' · ' + tr("reports.some_files_could_not_be_read") : ''}</p>
      <div class="usage-totals"><div class="usage-total"><span>${tr("common.tokens")}</span><strong>${totals.input_reports || totals.output_reports ? usageNumber(totals.input_tokens+totals.output_tokens) : '—'}</strong><span>${usageNumber(totals.reported_requests)} / ${usageNumber(totals.requests)} ${tr("reports.requests_reported_usage")}</span></div><div class="usage-total"><span>${tr("services.api_equivalent_usd")}</span><strong>${cost(totals)}</strong><span>${usageNumber(totals.priced_requests)} / ${usageNumber(totals.requests)} ${tr("services.requests_priced")}</span></div></div>
      ${totals.requests ? `<svg class="usage-chart" viewBox="0 0 ${width} 190" aria-label="${tr("reports.token_usage_select_a_bar_to_zoom")}"><path d="M70 20V150H698" fill="none" stroke="var(--border)"/><text x="60" y="28" text-anchor="end">${new Intl.NumberFormat('en',{notation:'compact'}).format(max)}</text><text x="60" y="153" text-anchor="end">0</text>${bars}${timeAxisSvg(data.start, data.end, time => 70 + (time-data.start)/range*plot, 170, 70, 698)}</svg>${groups}` : `<p>${tr("reports.no_usage_recorded_in_this_period")}</p>`}
      ${historical ? `<p class="usage-notice">${usageNumber(historical)} ${tr("reports.records_from_local_history_estimated_at_available_scan_time")}</p>` : ''}
      ${data.unmatched_overlap || (historical && data.uncorrelated_realtime) ? `<p class="usage-notice usage-warning">${tr("reports.some_history_and_live_usage_cannot_be_reconciled_missing")}</p>` : ''}
      ${data.issues?.some(row => ['inconsistent_usage','aggregate_usage'].includes(row.price_issue)) ? `<p class="usage-notice usage-warning">${tr("reports.some_old_usage_fields_disagree_or_contain_only_cumulative")}</p>` : ''}
      ${missing ? `<p class="usage-notice">${missing} ${tr("reports.requests_reported_no_usage_mostly_interrupted_or_failed_and")}</p>` : ''}
      ${usagePricingEditor(data)}
      ${price.stale || price.error ? `<p class="usage-notice usage-warning">${tr("reports.prices_are_not_up_to_date_saved_rates_are")}</p>` : ''}
      ${data.write_error ? `<p class="usage-notice usage-warning">${tr("reports.usage_could_not_be_saved_totals_may_be_incomplete")}</p>` : ''}
      <details class="usage-notice"><summary>${tr("reports.pricing_basis_updates")}</summary><p>${tr("reports.uses_local_api_rates_at_request_completion_or_the")}</p><p><a href="${esc(price.url)}" target="_blank" rel="noopener noreferrer">LiteLLM</a> · ${tr("reports.updated_in_the_background_every_24_hours")} · ${price.fetched_at ? esc(new Date(price.fetched_at*1000).toLocaleString()) : tr("reports.prices_not_downloaded")}</p>${price.stale || price.error ? `<p class="usage-warning">${tr("reports.prices_are_not_up_to_date_saved_rates_are_2")}</p>` : ''}</details>`;
  }

  return {open:openUsage, refresh:loadUsage, stop:stopUsage};
}
