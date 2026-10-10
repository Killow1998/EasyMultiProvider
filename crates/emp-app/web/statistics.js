// One statistics window owns common filters, reads, refresh and chart disposal.
// Call details and model performance keep their existing report owner/pagination.
function createStatistics({api,$,tr,esc,getState,getLanguage,openModal,callReports,periodPickerHtml,registerPeriodPicker,periodPickers,refreshPeriod,loadDiagnostics,loadSupportReport}) {
  const {number,seconds,tps,rate,outcomes,bindOutcomes} = callReports;
  const chart=createPerformanceChart({esc,tr,getLanguage,number,seconds,tps});
  const facts=createPerformanceView({esc,tr,getLanguage,number,seconds,tps,rate,outcomes,bindOutcomes,definition:title=>title === 'TTFT' ? tr('开始调用到首个推理、文本或工具输出块。','Time from operation start to the first reasoning, text or tool output block.') : title === 'TPS' ? tr('输出 token 除以整个调用耗时，包含重试等待。','Output tokens divided by the full operation duration, including retry waits.') : ''});
  const usage=createUsageData({tr,esc,number,getState});
  let root=null, query=null, report=null, view='overview', metric='tokens', grouping='none', filters={}, dispose=() => {}, timer=null;
  const options = rows => rows.map(([id,label])=>`<option value="${esc(id)}">${esc(label)}</option>`).join('');
  function filterHtml() {
    const state=getState();
    const services=[['',tr('全部服务','All services')],['account:@native','Native'],...(state.accounts || []).filter(a=>!a.native).map(a=>['account:'+a.id,a.name || a.id]),...(state.providers || []).map(p=>['provider:'+p.id,p.name || p.id])];
    return `<div class="call-toolbar"><select data-stats-filter="service" aria-label="${tr('服务','Service')}">${options(services)}</select><select data-stats-filter="category" aria-label="${tr('服务类型','Service type')}">${options([['',tr('全部类型','All types')],['native','Codex · Native'],['subscription',tr('Codex 订阅','Codex subscription')],['external',tr('外部服务','External providers')],['unknown',tr('未关联','Unlinked')]])}</select><select data-stats-filter="model" aria-label="${tr('模型','Model')}">${options([['',tr('全部模型','All models')],...(state.models || []).map(m=>[m.id,m.display_name || m.id])])}</select><select data-stats-filter="state" aria-label="${tr('状态','Status')}">${options([['',tr('全部状态','All states')],['completed',tr('已完成','Completed')],['failed',tr('失败','Failed')],['interrupted',tr('已中断','Interrupted')],['cancelled',tr('连接已断开','Disconnected')],['recovery_required',tr('需要重发历史','History resend requested')],['unknown',tr('结果未记录','Outcome not recorded')]])}</select><input data-stats-filter="session" aria-label="${tr('会话 ID','Session ID')}" placeholder="${tr('会话 ID','Session ID')}"></div>`;
  }
  function stop() {
    query?.cancel(); query=null; dispose(); dispose=()=>{}; facts.stop();
    if (timer !== null) clearTimeout(timer); timer=null;
    delete periodPickers.presentation_stats; root=null; report=null;
  }
  function render() {
    if (!root || !report || root.ownerDocument.querySelector('.call-outcome-dialog[open]')) return;
    dispose(); dispose=()=>{};
    const content=root.querySelector('[data-stats-content]');
    const focused=content.contains(document.activeElement) ? document.activeElement : null;
    const focusMetric=focused?.dataset.statsMetric, focusGroup=focused?.hasAttribute('data-stats-group');
    root.querySelectorAll('[data-stats-view]').forEach(button=>{ const selected=button.dataset.statsView===view; button.classList.toggle('active',selected); button.setAttribute('aria-pressed',String(selected)); });
    if (view === 'calls') {
      content.innerHTML='<div id="presentation_stats_calls"></div>';
      callReports.mount('presentation_stats_calls',filters,false); return;
    }
    callReports.stop();
    if (view !== 'overview') {
      content.innerHTML=usage.table(report.usage,view)+(view === 'models' ? `<details data-model-performance><summary>${tr('模型性能','Model performance')}</summary><div id="presentation_stats_models"></div></details>` : '');
      content.querySelector('[data-model-performance]')?.addEventListener('toggle',event=>{
        if (event.target.open) { callReports.mount('presentation_stats_models',filters,true); content.querySelector('[data-call-view="models"]')?.click(); }
        else callReports.stop();
      });
      return;
    }
    const u=report.usage.totals;
    const estimated=usage.usd(u);
    const summary=facts.facts(report.calls)+facts.stat('Token',number(u.input_tokens+u.output_tokens),`${number(u.input_tokens)} ${tr('输入','in')} · ${number(u.output_tokens)} ${tr('输出','out')}`)+facts.stat(tr('API 估算 · USD','API estimate · USD'),estimated,`${number(u.priced_requests)} / ${number(u.requests)} ${tr('条已计价','priced records')}`);
    const isUsage=['tokens','cost_nanos'].includes(metric), selected=isUsage ? usage.chart(report.usage,metric,grouping) : report.calls;
    const metricButtons=[['tokens','Token'],['cost_nanos','USD'],['calls',tr('调用量','Calls')],['tokens_per_second','TPS'],['ttft_ms','TTFT']].map(([id,label])=>`<button type="button" class="secondary${id===metric ? ' active' : ''}" data-stats-metric="${id}" aria-pressed="${id===metric}">${label}</button>`).join('');
    content.innerHTML=`<div class="presentation-stat-grid">${summary}</div><div class="presentation-stats-chart-tools"><div class="report-tabs" role="group" aria-label="${tr('指标','Metric')}">${metricButtons}</div>${isUsage ? `<label>${tr('分色','Color by')}<select data-stats-group>${options([['none',tr('不区分','None')],['services',tr('服务','Services')],['models',tr('模型','Models')]])}</select></label>` : ''}</div>${chart.html(selected,metric)}`;
    content.querySelector('.presentation-chart-heading .report-tabs').remove();
    content.querySelector('.presentation-chart-heading h3').textContent=isUsage ? tr('用量趋势','Usage activity') : tr('调用趋势','Call activity');
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
    return query.run(new URLSearchParams(filters).toString());
  }
  function refresh() {
    if (!root) return false;
    if (timer === null) timer=setTimeout(()=>{timer=null; if (root) void refreshPeriod('presentation_stats',false);},200);
    return true;
  }
  function open() {
    openModal(tr('统计','Statistics'),`<div id="presentation_stats">${periodPickerHtml('presentation_stats',['1h','1d','7d','30d'])}${filterHtml()}<div class="report-tabs" role="group" aria-label="${tr('视图','View')}">${[['overview',tr('总览','Overview')],['services',tr('服务','Services')],['models',tr('模型','Models')],['calls',tr('调用','Calls')]].map(([id,label])=>`<button type="button" class="secondary" data-stats-view="${id}" aria-pressed="false">${label}</button>`).join('')}</div><p class="report-status" data-report-status role="status" aria-live="polite"></p><div data-stats-content></div><details class="diagnostics-details"><summary>${tr('系统诊断','System diagnostics')}</summary><div class="toolbar"><button type="button" class="secondary" data-icon="refresh" onclick="loadDiagnostics();loadSupportReport()">${tr('刷新','Refresh')}</button><button type="button" class="secondary" data-icon="download" onclick="downloadSupportReport()">${tr('下载脱敏报告','Download redacted report')}</button></div><div class="toolbar"><button type="button" class="secondary" onclick="openUsage()">${tr('历史与计价','History and pricing')}</button></div><div id="support_report"></div><div id="diagnostics_summary"></div><div id="health_summary"></div><div id="diagnostics_records"></div></details></div>`,'',null);
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
      if (button.dataset.statsView) view=button.dataset.statsView;
      if (button.dataset.statsMetric) metric=button.dataset.statsMetric;
      render();
    };
    root.querySelector('.diagnostics-details').addEventListener('toggle',event=>{if (event.target.open) {void loadDiagnostics();void loadSupportReport();}});
    registerPeriodPicker('presentation_stats','1d',()=>load());
  }
  return {open,stop,refresh};
}
