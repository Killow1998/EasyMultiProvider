// One statistics window owns common filters, reads, refresh and chart disposal.
// Call details and model performance keep their existing report owner/pagination.
function createStatistics({api,$,tr,esc,getState,getLanguage,openModal,callReports,periodPickerHtml,registerPeriodPicker,periodPickers,refreshPeriod,loadDiagnostics,loadSupportReport}) {
  const {number,seconds,tps,rate,outcomes,bindOutcomes} = callReports;
  const chart=createPerformanceChart({esc,tr,getLanguage,number,seconds,tps});
  const facts=createPerformanceView({esc,tr,getLanguage,number,seconds,tps,rate,outcomes,bindOutcomes,definition:title=>title === 'TTFT' ? tr("reports.time_to_first_content") : title === 'TPS' ? tr("reports.generation_speed_definition") : ''});
  const usage=createUsageData({tr,esc,number});
  const bills=createSessionBills({api,tr,esc,number,getLanguage});
  let root=null, query=null, report=null, view='overview', metric='tokens', grouping='none', filters={}, dispose=() => {}, timer=null;
  const options = rows => rows.map(([id,label])=>`<option value="${esc(id)}">${esc(label)}</option>`).join('');
  function filterHtml() {
    const state=getState();
    const services=[['',tr("reports.all_services")],['account:@native','Native'],...(state.accounts || []).filter(a=>!a.native).map(a=>['account:'+a.id,a.name || a.id]),...(state.providers || []).map(p=>['provider:'+p.id,p.name || p.id])];
    return `<div class="call-toolbar"><select data-stats-filter="service" aria-label="${tr("reports.service")}">${options(services)}</select><select data-stats-filter="category" aria-label="${tr("reports.service_type")}">${options([['',tr("reports.all_types")],['native','Codex · Native'],['subscription',tr("reports.codex_subscription")],['external',tr("reports.external_providers")],['unknown',tr("reports.unlinked")]])}</select><select data-stats-filter="model" aria-label="${tr("reports.model")}">${options([['',tr("reports.all_models")],...(state.models || []).map(m=>[m.id,m.display_name || m.id])])}</select><select data-stats-filter="state" aria-label="${tr("reports.status")}">${options([['',tr("reports.all_states")],['completed',tr("reports.completed_2")],['failed',tr("reports.failed_2")],['interrupted',tr("reports.interrupted_2")],['cancelled',tr("reports.disconnected_2")],['recovery_required',tr("reports.history_resend_requested")],['unknown',tr("reports.outcome_not_recorded")]])}</select><input data-stats-filter="session" aria-label="${tr("reports.session_id")}" placeholder="${tr("reports.session_id")}"></div>`;
  }
  function stop() {
    query?.cancel(); query=null; dispose(); dispose=()=>{}; facts.stop(); bills.stop();
    if (timer !== null) clearTimeout(timer); timer=null;
    delete periodPickers.presentation_stats; root=null; report=null;
  }
  function render(refresh=false) {
    if (!root || root.ownerDocument.querySelector('.call-outcome-dialog[open]')) return;
    dispose(); dispose=()=>{};
    const content=root.querySelector('[data-stats-content]');
    const focused=content.contains(document.activeElement) ? document.activeElement : null;
    const focusMetric=focused?.dataset.statsMetric, focusGroup=focused?.hasAttribute('data-stats-group');
    root.querySelectorAll('[data-stats-view]').forEach(button=>{ const selected=button.dataset.statsView===view; button.classList.toggle('active',selected); button.setAttribute('aria-pressed',String(selected)); });
    const sessionInput=root.querySelector('[data-stats-filter="session"]');
    sessionInput.hidden=view==='sessions' && !sessionInput.value;
    if (view === 'sessions') { callReports.stop(); return bills.mount(content,filters,refresh); }
    bills.stop();
    if (!report) {content.innerHTML='';return;}
    if (view === 'calls') {
      if (!content.querySelector('#presentation_stats_calls')) content.innerHTML='<div id="presentation_stats_calls"></div>';
      callReports.mount('presentation_stats_calls',filters,false,report.calls); return;
    }
    callReports.stop();
    if (view !== 'overview') {
      content.innerHTML=usage.table(report.usage,view)+(view === 'models' ? `<details data-model-performance><summary>${tr("reports.model_performance")}</summary><div id="presentation_stats_models"></div></details>` : '');
      content.querySelector('[data-model-performance]')?.addEventListener('toggle',event=>{
        if (event.target.open) { callReports.mount('presentation_stats_models',filters,true); content.querySelector('[data-call-view="models"]')?.click(); }
        else callReports.stop();
      });
      return;
    }
    const u=report.usage.totals;
    const estimated=usage.usd(u);
    const summary=facts.facts(report.calls)+facts.stat('Token',usage.tokens(u),`${usage.count(u,'input_tokens','input_reports',false)} ${tr("reports.in")} · ${usage.count(u,'output_tokens','output_reports',false)} ${tr("reports.out")}`)+facts.stat(tr("reports.api_estimate_usd"),estimated,`${number(u.priced_requests)} / ${number(u.requests)} ${tr("reports.priced_records")}`);
    const isUsage=['tokens','cost_nanos'].includes(metric), selected=isUsage ? usage.chart(report.usage,metric,grouping) : report.calls;
    const metricButtons=[['tokens','Token'],['cost_nanos','USD'],['calls',tr("reports.calls_3")],['tokens_per_second','TPS'],['ttft_ms','TTFT']].map(([id,label])=>`<button type="button" class="secondary${id===metric ? ' active' : ''}" data-stats-metric="${id}" aria-pressed="${id===metric}">${label}</button>`).join('');
    content.innerHTML=`<div class="presentation-stat-grid">${summary}</div><div class="presentation-stats-chart-tools"><div class="report-tabs" role="group" aria-label="${tr("reports.metric")}">${metricButtons}</div>${isUsage ? `<label>${tr("reports.color_by")}<select data-stats-group>${options([['none',tr("reports.none")],['services',tr("services_title")],['models',tr("models_routes")]])}</select></label>` : ''}</div>${chart.html(selected,metric)}`;
    content.querySelector('.presentation-chart-heading .report-tabs').remove();
    content.querySelector('.presentation-chart-heading h3').textContent=isUsage ? tr("reports.usage_activity") : tr("reports.call_activity");
    if (isUsage && grouping !== 'none') content.querySelector('.presentation-performance-chart').insertAdjacentHTML('beforeend',`<div class="presentation-chart-legend">${selected.chartSeries.map(item=>`<span><i style="background:${esc(item.color)}"></i>${esc(item.label)}</span>`).join('')}</div>`);
    if (isUsage) content.querySelector('[data-stats-group]').value=grouping;
    const chartDispose=chart.bind(content,selected,metric), outcomeDispose=bindOutcomes(content,report.calls.summary || {},report.calls);
    dispose=()=>{chartDispose();outcomeDispose();};
    if (focusMetric) content.querySelector(`[data-stats-metric="${focusMetric}"]`)?.focus();
    if (focusGroup) content.querySelector('[data-stats-group]')?.focus();
  }
  function load() {
    if (!root) return;
    const period=periodPickers.presentation_stats;
    Object.assign(filters,{start:period.start,end:period.end});
    if (view === 'sessions') return render(true);
    return query.run(new URLSearchParams(filters).toString());
  }
  function refresh() {
    if (!root) return false;
    if (timer === null) timer=setTimeout(()=>{timer=null; if (root) void refreshPeriod('presentation_stats',false);},200);
    return true;
  }
  function open() {
    openModal(tr("statistics"),`<div id="presentation_stats">${periodPickerHtml('presentation_stats',['1h','1d','7d','30d'])}${filterHtml()}<div class="report-tabs" role="group" aria-label="${tr("reports.view")}">${[['overview',tr("reports.overview")],['services',tr("services_title")],['models',tr("models_routes")],['sessions',tr("reports.sessions")],['calls',tr("reports.calls")]].map(([id,label])=>`<button type="button" class="secondary" data-stats-view="${id}" aria-pressed="false">${label}</button>`).join('')}</div><p class="report-status" data-report-status role="status" aria-live="polite"></p><div data-stats-content></div><details class="diagnostics-details"><summary>${tr("reports.system_diagnostics")}</summary><div class="toolbar"><button type="button" class="secondary" data-icon="refresh" onclick="loadDiagnostics();loadSupportReport()">${tr("refresh")}</button><button type="button" class="secondary" data-icon="download" onclick="downloadSupportReport()">${tr("reports.download_redacted_report")}</button></div><div id="support_report"></div><div id="diagnostics_summary"></div><div id="health_summary"></div><div id="diagnostics_records"></div></details></div>`,'',null);
    root=$('presentation_stats'); filters={}; view='overview'; metric='tokens'; grouping='none';
    const selectedRoot=root;
    query=createReportQuery({api:async(params,options)=>{const [calls,usage]=await Promise.all([api('/api/calls?'+params,options),api('/api/usage?series=true&'+params,options)]);return {calls,usage};},onState:(loading,error)=>setReportState(selectedRoot,loading,error,tr),onResult:result=>{report=result;render();}});
    root.onchange=event=>{
      if (event.target.hasAttribute('data-stats-group')) { grouping=event.target.value;render();return; }
      const field=event.target.dataset.statsFilter; if (!field) return;
      if (field === 'service') { delete filters.account; delete filters.provider; const value=event.target.value; if (value) { const colon=value.indexOf(':'); filters[value.slice(0,colon)]=value.slice(colon+1); } }
      else if (event.target.value) filters[field]=event.target.value; else delete filters[field];
      void load();
    };
    root.onclick=event=>{
      const button=event.target.closest('[data-stats-view],[data-stats-metric]'); if (!button) return;
      if (button.dataset.statsView) {
        const previous=view;view=button.dataset.statsView;
        if (view === 'sessions') query.cancel();
        else if (previous === 'sessions') {report=null;void load();}
      }
      if (button.dataset.statsMetric) metric=button.dataset.statsMetric;
      render();
    };
    root.querySelector('.diagnostics-details').addEventListener('toggle',event=>{if (event.target.open) {void loadDiagnostics();void loadSupportReport();}});
    registerPeriodPicker('presentation_stats','1d',()=>load());
  }
  return {open,stop,refresh};
}
