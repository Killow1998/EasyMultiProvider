// Quota trend owns its reads, timer, chart state and window interactions.
function createQuotaHistory({api,$,tr,esc,openModal,quotaDisplay,afterRead,compactWindowText,localTimeText,utcOffsetLabel,subscriptionPlanKey,subscriptionPlanLabel,subscriptionPlanColor,periods}) {
  const {periodPickerHtml,registerPeriodPicker,periodPickers,setPeriod,periodPresetRange,refreshPeriod,timeAxisTicks,timeAxisSvg} = periods;
  let activeQuotaAccountId = '', activeQuotaServiceKind = 'accounts', quotaHistoryTimer = null,
      quotaHistoryPayload = null, activeQuotaLimit = 'codex', activeQuotaWindow = '',
      quotaHistoryZoom = [], quotaHistoryLoading = false, quotaHistoryLoadingVisible = false, query = null;
  const quotaChartColors = ['#3b82f6','#f59e0b','#10b981','#a855f7','#ef4444','#06b6d4'];
  function quotaLimitLabel(limitId) { return ({codex:'Codex', claude:'Claude', codex_bengalfox:'Codex Spark'})[limitId] || limitId; }
  function quotaSeriesLabel(series) { const duration = compactWindowText(series.window_minutes); const label = duration === '?' ? (series.window_kind === 'primary' ? tr('主窗口','Primary') : tr('次窗口','Secondary')) : duration; return `${!series.limit_id || series.limit_id === 'codex' ? '' : quotaLimitLabel(series.limit_id) + ' · '}${label}`; }
  function quotaSeriesColor(series, index) { return Number(series.window_minutes) === 300 ? '#3b82f6' : Number(series.window_minutes) === 10080 ? '#10b981' : quotaChartColors[index % quotaChartColors.length]; }
  function quotaPointBreak(previous, point, gapLimit) {
    if (!previous) return '';
    const priorReset = Number(previous.resets_at), reset = Number(point.resets_at);
    if (priorReset > 0 && reset > 0 && Math.abs(reset-priorReset) > 60) return tr('新的额度周期','New quota period');
    if (Number(point.remaining_percent) > Number(previous.remaining_percent) + .1) return tr('额度回升','Quota replenished');
    if (Number(point.observed_at) - Number(previous.observed_at) > gapLimit) return tr('无数据记录','No data recorded');
    return '';
  }
  function quotaValidPoint(point) { return point.observed_at != null && point.remaining_percent != null && Number.isFinite(Number(point.observed_at)) && Number.isFinite(Number(point.remaining_percent)); }
  // Samples further apart than this leave a period with no record.
  function quotaGapLimit(payload) { return Math.max(180, 3 * (Number(payload?.interval_seconds) || 44)); }
  function quotaResetBoundaries(series, start, end) {
    const boundaries = new Map();
    for (const item of series) {
      let previous = null;
      const samples = (item.points || []).filter(quotaValidPoint).slice().sort((a,b) => a.observed_at-b.observed_at);
      for (const point of samples) {
        const reset = Number(point.resets_at);
        if (!(reset > 0)) continue;
        if (previous && Math.abs(reset-Number(previous.resets_at)) > 60) {
          const priorReset = Number(previous.resets_at);
          // A reset inside the observation gap is timed by the prior deadline.
          // Early resets can only be located at the first changed observation.
          const scheduled = priorReset > Number(previous.observed_at) && priorReset <= Number(point.observed_at);
          const at = scheduled ? priorReset : Number(point.observed_at);
          if (at > start && at < end) boundaries.set(at, {at, kind:scheduled ? 'reset' : 'observed'});
        }
        previous = point;
      }
    }
    return [...boundaries.values()].sort((a,b) => a.at-b.at);
  }
  // Dividers mark quota resets (and days on long periods); clicking a section zooms into it.
  function quotaChartSections(series, view) {
    // Over long spans reset lines would crowd the chart, so divide by day instead.
    const long = view.end - view.start > 2*86400;
    const boundaries = long ? [] : quotaResetBoundaries(series, view.start, view.end);
    if (long) {
      const next = new Date(view.start*1000); next.setHours(0,0,0,0); next.setDate(next.getDate()+1);
      while (next.getTime()/1000 < view.end) { boundaries.push({at:next.getTime()/1000, kind:'day'}); next.setDate(next.getDate()+1); }
    }
    let edges = [view.start, ...boundaries.map(item => item.at), view.end];
    if (edges.length === 2) {
      const ticks = timeAxisTicks(view.start, view.end).map(tick => tick.at).filter(at => at > view.start && at < view.end);
      edges = [view.start, ...ticks, view.end];
    }
    const sections = edges.slice(0,-1).map((start,index) => ({start, end:edges[index+1]})).map(section => ({...section, zoomable:section.end-section.start >= 900 && section.end-section.start < view.end-view.start}));
    return {boundaries, sections};
  }
  function quotaTimeRangeText(start, end) {
    const options = {month:'numeric',day:'numeric',hour:'2-digit',minute:'2-digit',hour12:false};
    return `${new Date(start*1000).toLocaleString([],options)} — ${new Date(end*1000).toLocaleString([],options)}`;
  }
  let quotaChartData = {points:[], gaps:[]};
  // Draw at the modal's real width so text keeps its size instead of scaling with the SVG.
  function quotaChartWidth() { return Math.round(Math.max(628, Math.min(1200, ($('modal_body')?.clientWidth || 628) - 2))); }
  function quotaGapReason(gap) {
    const incidents = (quotaHistoryPayload?.failures || []).filter(item => Number(item.start_at) < gap.end && Number(item.end_at) >= gap.start);
    if (!incidents.length) return tr('无数据记录','No data recorded');
    const codes = new Set(incidents.map(item => item.diagnostics?.code));
    const cause = codes.has('quota_transport_error') ? tr('额度查询连接失败','Quota connection failed') : codes.has('quota_auth_required') ? tr('额度查询需要重新登录','Quota query requires sign-in') : codes.has('quota_rate_limited') ? tr('额度查询频率受限','Quota query rate limited') : codes.has('quota_timeout') ? tr('额度查询超时','Quota query timed out') : tr('额度查询失败','Quota query failed');
    return cause;
  }
  function quotaChartSvg(series, view, gapLimit) {
    const visible = point => quotaValidPoint(point) && Number(point.observed_at) >= view.start && Number(point.observed_at) <= view.end;
    const samplesBySeries = series.map(item => (item.points || []).filter(visible).slice().sort((a,b) => Number(a.observed_at) - Number(b.observed_at)));
    const points = samplesBySeries.flat(); if (!points.length) return '';
    const minTime = view.start, maxTime = view.end, span = Math.max(1, maxTime - minTime);
    const percent = value => Math.max(0, Math.min(100, Number(value)));
    const values = points.map(point => quotaDisplay.value(point.remaining_percent)); const minValue = Math.min(...values); const maxValue = Math.max(...values);
    // Round percent gridlines (every 5, 10, 20 or 25 points).
    const tickStep = [5, 10, 20, 25].find(step => (Math.ceil(maxValue/step) - Math.floor(minValue/step)) * step / step <= 5) || 25;
    const axisMin = Math.max(0, Math.floor(minValue/tickStep)*tickStep - (minValue % tickStep === 0 && minValue > 0 ? tickStep : 0)); const axisMax = Math.min(100, Math.max(axisMin + tickStep, Math.ceil(maxValue/tickStep)*tickStep)); const axisSpan = axisMax - axisMin;
    const width = quotaChartWidth(), left = 46, right = width - 18, top = 18, bottom = 218; const x = time => left + ((Number(time) - minTime) / span) * (right - left); const y = value => bottom - ((percent(value) - axisMin) / axisSpan) * (bottom - top);
    const grid = Array.from({length:Math.round(axisSpan/tickStep)+1}, (_, index) => axisMin + tickStep * index).map(value => `<line x1="${left}" y1="${y(value)}" x2="${right}" y2="${y(value)}" stroke="var(--border)"/><text x="${left-7}" y="${y(value)+4}" text-anchor="end" fill="var(--muted)" font-size="11">${Number(value.toFixed(2))}%</text>`).join('');
    // Periods without samples are shaded and labelled rather than left blank.
    const times = [...new Set(points.map(point => Number(point.observed_at)))].sort((a,b) => a-b);
    const gaps = []; let cursor = view.start;
    for (const time of times) { if (time - cursor > gapLimit) gaps.push({start:cursor, end:time}); cursor = time; }
    if (view.end - cursor > gapLimit) gaps.push({start:cursor, end:view.end});
    const middle = (top+bottom)/2;
    const gapMarks = `<defs><pattern id="quota-gap-hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="6" height="6" class="quota-gap-base"/><line x1="0" y1="0" x2="0" y2="6" class="quota-gap-stripe"/></pattern></defs>` + gaps.map(gap => {
      const x1 = x(gap.start), width = x(gap.end) - x1, center = (x1+width/2).toFixed(1);
      // Wide gaps get a horizontal label; narrow ones a vertical one.
      const reason = esc(quotaGapReason(gap));
      const label = width >= 72 ? `<text class="quota-gap-label" x="${center}" y="${middle+4}" text-anchor="middle">${reason}</text>` : width >= 14 ? `<text class="quota-gap-label" x="${center}" y="${middle}" text-anchor="middle" dominant-baseline="central" transform="rotate(-90 ${center} ${middle})">${reason}</text>` : '';
      return `<rect class="quota-gap" x="${x1.toFixed(1)}" y="${top}" width="${Math.max(0,width).toFixed(1)}" height="${bottom-top}" fill="url(#quota-gap-hatch)"/>${label}`;
    }).join('');
    const hoverPoints = [];
    const lines = series.map((item, index) => {
      const color = quotaSeriesColor(item, index), samples = samplesBySeries[index];
      if (!samples.length) return '';
      const path = samples.map((point, i) => `${i && !quotaPointBreak(samples[i-1], point, gapLimit) ? 'L' : 'M'}${x(point.observed_at).toFixed(1)},${y(quotaDisplay.value(point.remaining_percent)).toFixed(1)}`).join(' ');
      samples.forEach((point, i) => hoverPoints.push({series:index, x:x(point.observed_at), y:y(quotaDisplay.value(point.remaining_percent)), time:Number(point.observed_at), value:quotaDisplay.value(point.remaining_percent), label:quotaSeriesLabel(item), reset:point.resets_at ?? '', note:quotaPointBreak(samples[i-1], point, gapLimit)}));
      return `<path d="${path}" fill="none" stroke="${color}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke"/><circle class="quota-chart-marker" data-series="${index}" r="4" fill="${color}" stroke="var(--panel)" stroke-width="2" hidden/>`;
    }).join('');
    quotaChartData = {points:hoverPoints, gaps:gaps.map(gap => ({...gap, x1:x(gap.start), x2:x(gap.end)}))};
    const {boundaries, sections} = quotaChartSections(series, view);
    const dividers = boundaries.map(({at,kind}) => `<line class="quota-period-divider${kind === 'observed' ? ' observed' : ''}" x1="${x(at)}" y1="${top}" x2="${x(at)}" y2="${bottom}"/>`).join('');
    const targets = sections.map(section => {
      const label = quotaTimeRangeText(section.start,section.end);
      return `<rect class="quota-hover-target${section.zoomable ? ' quota-segment' : ''}" x="${x(section.start)}" y="${top}" width="${Math.max(0,x(section.end)-x(section.start))}" height="${bottom-top}" data-start="${section.start}" data-end="${section.end}"${section.zoomable ? ` role="button" tabindex="0" aria-label="${esc(tr('查看 ','View ') + label)}"` : ''}><title>${esc(label)}</title></rect>`;
    }).join('');
    return `<svg viewBox="0 0 ${width} 266" role="group" aria-label="${esc(tr('Subscription 额度趋势','Subscription quota trend'))}">${grid}${gapMarks}${dividers}${lines}<line class="quota-chart-guide" x1="${left}" y1="${top}" x2="${left}" y2="${bottom}" hidden/>${targets}${timeAxisSvg(minTime, maxTime, x, 240, left, right)}</svg>`;
  }
  function quotaObservedTimeText(value) { const timestamp = Number(value); if (!Number.isFinite(timestamp)) return ''; const date = new Date(timestamp * 1000); return `${date.toLocaleString([], {hour12:false})} (${utcOffsetLabel(date)})`; }
  function quotaChartHover(event) {
    const svg = event.currentTarget?.ownerSVGElement; if (!svg) return;
    const bounds = svg.getBoundingClientRect(); if (!(bounds.width > 0)) return;
    const width = svg.viewBox.baseVal.width, cursorX = Math.max(46, Math.min(width - 18, ((event.clientX - bounds.left) / bounds.width) * width));
    const guide = svg.querySelector('.quota-chart-guide'), tooltip = svg.parentElement?.querySelector('.quota-chart-tooltip');
    const markers = [...svg.querySelectorAll('.quota-chart-marker')]; markers.forEach(marker => marker.setAttribute('hidden',''));
    const gap = quotaChartData.gaps.find(item => cursorX > item.x1 && cursorX < item.x2);
    const points = quotaChartData.points;
    const nearest = !gap && points.length ? points.reduce((best, point) => Math.abs(point.x - cursorX) < Math.abs(best.x - cursorX) ? point : best, points[0]) : null;
    const guideX = nearest ? nearest.x : cursorX;
    if (guide) { guide.removeAttribute('hidden'); guide.setAttribute('x1', guideX); guide.setAttribute('x2', guideX); }
    if (!tooltip) return;
    tooltip.hidden = false; tooltip.style.left = `${Math.max(20,Math.min(80,guideX / width * 100))}%`;
    if (!nearest) { tooltip.style.top = '45%'; tooltip.innerHTML = `<strong>${esc(quotaGapReason(gap))}</strong><br>${esc(quotaTimeRangeText(gap.start, gap.end))}`; return; }
    const active = points.filter(point => point.time === nearest.time);
    for (const point of active) { const marker = markers.find(item => Number(item.dataset.series) === point.series); if (marker) { marker.setAttribute('cx', point.x.toFixed(1)); marker.setAttribute('cy', point.y.toFixed(1)); marker.removeAttribute('hidden'); } }
    tooltip.style.top = `${Math.max(70, Math.min(...active.map(point => point.y))) / 266 * 100}%`;
    tooltip.innerHTML = `<strong>${esc(quotaObservedTimeText(nearest.time))}</strong><br>${active.map(point => { const reset = localTimeText(point.reset); return `${esc(point.label)} · ${quotaDisplay.label(tr)} ${esc(point.value)}%${point.note ? ' · ' + esc(point.note) : ''}${reset ? '<br>' + tr('重置时间：','Resets: ') + esc(reset) : ''}`; }).join('<br>')}`;
  }
  function quotaChartLeave(event) { const svg = event.currentTarget?.ownerSVGElement; if (!svg) return; svg.querySelector('.quota-chart-guide')?.setAttribute('hidden',''); svg.querySelectorAll('.quota-chart-marker').forEach(marker => marker.setAttribute('hidden','')); const tooltip = svg.parentElement?.querySelector('.quota-chart-tooltip'); if (tooltip) tooltip.hidden = true; }
  function quotaPlanTimeline(plans, view) {
    const byTime = new Map();
    for (const item of Array.isArray(plans) ? plans : []) { const observedAt = Number(item?.observed_at); const key = subscriptionPlanKey(item?.plan_type); if (Number.isFinite(observedAt) && observedAt <= view.end && key) byTime.set(observedAt,{observedAt,planType:item.plan_type,key}); }
    const ordered = [...byTime.values()].sort((left,right) => left.observedAt-right.observedAt);
    const prior = ordered.filter(item => item.observedAt <= view.start).pop();
    const visible = [...(prior ? [prior] : []),...ordered.filter(item => item.observedAt > view.start && item.observedAt <= view.end)];
    const changes = [];
    for (const item of visible) { const previous = changes[changes.length-1]; if (previous && previous.key === item.key) continue; changes.push(item); }
    const segments = changes.map((item,index) => { const start = Math.max(view.start,item.observedAt); const end = index + 1 < changes.length ? Math.min(view.end,changes[index+1].observedAt) : view.end; if (!(end > start)) return ''; const left = (start-view.start)/(view.end-view.start)*100; const width = (end-start)/(view.end-view.start)*100; const label = subscriptionPlanLabel(item.planType); const title = `${label} · ${quotaTimeRangeText(start,end)}`; return `<span class="quota-plan-segment" style="left:${left.toFixed(3)}%;width:${width.toFixed(3)}%;--plan-color:${subscriptionPlanColor(item.planType)}" title="${esc(title)}">${width >= 8 ? esc(label) : ''}</span>`; }).join('');
    const covered = changes.length ? Math.max(view.start, changes[0].observedAt) : view.end;
    const emptyWidth = (covered - view.start) / (view.end - view.start) * 100;
    const empty = emptyWidth > 0.5 ? `<span class="quota-plan-empty" style="left:0;width:${emptyWidth.toFixed(3)}%">${emptyWidth >= 10 ? tr('无数据记录','No data recorded') : ''}</span>` : '';
    return segments ? `<div class="quota-plan-history"><span>${tr('订阅','Plan')}</span><div class="quota-plan-track" aria-label="${esc(tr('订阅方案历史','Subscription plan history'))}">${empty}${segments}</div></div>` : '';
  }
  function renderQuotaHistory(payload) {
    quotaHistoryPayload = payload;
    const content = $('quota_history_content'); if (!content) return; const allSeries = (payload.series || []).filter(item => Array.isArray(item.points) && item.points.length);
    if (!content.querySelector('#quota_history_plot')) content.innerHTML = `<div id="quota_history_controls" class="quota-chart-toolbar"></div><div class="quota-history-navigation"><button id="quota_history_back" class="secondary" type="button">${tr('返回','Back')}</button><span id="quota_history_range"></span><span id="quota_history_loading" class="muted" role="status" hidden>${tr('更新中…','Updating…')}</span></div><div id="quota_history_plan"></div><div id="quota_history_legend" class="quota-legend"></div><div id="quota_history_plot" class="quota-chart"></div>`;
    const period = periodPickers.quota_history;
    const view = {start:Number(payload.start_at) || period?.start || 0, end:Number(payload.end_at) || period?.end || 0};
    const groups = [...new Set(allSeries.map(item => item.limit_id || 'codex'))];
    if (!groups.includes(activeQuotaLimit)) activeQuotaLimit = groups.includes('codex') ? 'codex' : groups[0];
    const groupSeries = allSeries.filter(item => (item.limit_id || 'codex') === activeQuotaLimit);
    const windowKey = item => String(item.window_minutes || item.window_kind || 'unknown');
    const windows = [...new Set(groupSeries.map(windowKey))].sort((a,b) => Number(a) - Number(b));
    if (!windows.includes(activeQuotaWindow)) activeQuotaWindow = windows[0];
    const series = groupSeries.filter(item => windowKey(item) === activeQuotaWindow);
    const tabs = groups.length > 1 ? `<div class="quota-ranges quota-groups" aria-label="${esc(tr('额度组','Quota group'))}">${groups.map(id => `<button type="button" class="secondary" data-limit-id="${esc(id)}">${esc(quotaLimitLabel(id))}</button>`).join('')}</div>` : '';
    const windowTabs = windows.length > 1 ? `<div class="quota-ranges quota-windows" aria-label="${esc(tr('额度周期','Quota window'))}">${windows.map(key => `<button type="button" class="secondary" data-window="${esc(key)}">${esc(compactWindowText(key))}</button>`).join('')}</div>` : '';
    const planTimeline = view.end > view.start ? quotaPlanTimeline(payload.plans || [],view) : '';
    const legend = `<span>${quotaDisplay.label(tr)}</span>` + series.map((item,index) => { const samples = item.points.filter(point => quotaValidPoint(point) && point.observed_at >= view.start && point.observed_at <= view.end); const last = samples.length ? samples.reduce((latest, point) => Number(point.observed_at) > Number(latest.observed_at) ? point : latest) : null; const color = quotaSeriesColor(item,index); return `<span><i class="quota-swatch" style="background:${color}"></i>${esc(compactWindowText(item.window_minutes))}<strong>${last ? esc(quotaDisplay.value(last.remaining_percent)) + '%' : tr('无数据','No data')}</strong></span>`; }).join('');
    const chart = view.end > view.start ? quotaChartSvg(series, view, quotaGapLimit(payload)) : '';
    const plot = chart ? `${chart}<div class="quota-chart-tooltip" role="status" hidden></div>` : `<div class="quota-empty">${quotaHistoryLoading ? tr('正在读取记录…','Loading records…') : esc(quotaGapReason(view))}</div>`;
    for (const [id, markup] of [['quota_history_controls',tabs+windowTabs],['quota_history_plan',planTimeline],['quota_history_legend',legend],['quota_history_plot',plot]]) { const node = $(id); if (node.quotaMarkup !== markup) { node.innerHTML = markup; node.quotaMarkup = markup; } }
    content.querySelectorAll('.quota-hover-target').forEach(target => { target.onpointermove = quotaChartHover; target.onpointerleave = quotaChartLeave; });
    content.querySelectorAll('[data-limit-id], [data-window]').forEach(button => { const selected = button.dataset.limitId ? button.dataset.limitId === activeQuotaLimit : button.dataset.window === activeQuotaWindow; button.classList.toggle('active',selected); button.setAttribute('aria-pressed',String(selected)); });
    $('quota_history_range').textContent = view.end > view.start ? quotaTimeRangeText(view.start,view.end) : '';
    $('quota_history_back').hidden = !quotaHistoryZoom.length;
    $('quota_history_loading').hidden = !quotaHistoryLoading || !quotaHistoryLoadingVisible;
  }
  function selectQuotaHistoryGroup(limitId) { activeQuotaLimit = limitId; if (quotaHistoryPayload) renderQuotaHistory(quotaHistoryPayload); }
  function selectQuotaHistoryWindow(windowKey) { activeQuotaWindow = windowKey; if (quotaHistoryPayload) renderQuotaHistory(quotaHistoryPayload); }
  function zoomQuotaHistory(target) {
    const period = periodPickers.quota_history; if (!period) return;
    quotaHistoryZoom.push({preset:period.preset, start:period.start, end:period.end});
    setPeriod('quota_history', {preset:'', start:Number(target.dataset.start), end:Number(target.dataset.end)}, {user:true, zoom:true});
  }
  function backQuotaHistory() { const previous = quotaHistoryZoom.pop(); if (previous) setPeriod('quota_history', previous.preset ? {...previous, ...periodPresetRange(previous.preset)} : previous, {user:true, zoom:true}); }
  // One query owner cancels stale reads and closes with the trend window.
  function loadQuotaHistory(user = false) {
    const period = periodPickers.quota_history; if (!period || !query) return;
    quotaHistoryLoadingVisible = user;
    const selected = query;
    return selected.run('/api/' + activeQuotaServiceKind + '/' + encodeURIComponent(activeQuotaAccountId) + '/quota-history?' + new URLSearchParams({start:Math.floor(period.start), end:Math.ceil(period.end)}))
      .then(() => { if (query === selected) return afterRead(); });
  }
  function stop() {
    if (quotaHistoryTimer !== null) clearInterval(quotaHistoryTimer);
    quotaHistoryTimer = null; query?.cancel(); query = null;
    quotaHistoryLoading = false; quotaHistoryPayload = null;
    delete periodPickers.quota_history;
  }
  function openQuotaHistory(accountId, kind = 'accounts') {
    stop();
    openModal(tr('Subscription 额度趋势','Subscription quota trend'), `${periodPickerHtml('quota_history', ['1h','1d','7d'])}<div id="quota_history_content"></div>`, '', null);
    activeQuotaServiceKind = kind; activeQuotaAccountId = accountId;
    activeQuotaLimit = 'codex'; activeQuotaWindow = ''; quotaHistoryZoom = [];
    const root = $('quota_history_content');
    query = createReportQuery({api,onResult:renderQuotaHistory,onState:(loading,error) => {
      quotaHistoryLoading = loading;
      $('modal_status').textContent = error;
      const indicator = $('quota_history_loading');
      if (indicator) indicator.hidden = !loading || !quotaHistoryLoadingVisible;
    }});
    root.onclick = event => {
      const target = event.target.closest('[data-limit-id],[data-window],.quota-segment,#quota_history_back');
      if (!target) return;
      if (target.dataset.limitId) selectQuotaHistoryGroup(target.dataset.limitId);
      else if (target.dataset.window) selectQuotaHistoryWindow(target.dataset.window);
      else if (target.id === 'quota_history_back') backQuotaHistory();
      else zoomQuotaHistory(target);
    };
    root.onkeydown = event => {
      if ((event.key === 'Enter' || event.key === ' ') && event.target.matches('.quota-segment')) { event.preventDefault(); zoomQuotaHistory(event.target); }
    };
    renderQuotaHistory({series:[]});
    registerPeriodPicker('quota_history', '1d', (period, options) => { if (options.user && !options.zoom) quotaHistoryZoom = []; return loadQuotaHistory(options.user); });
    quotaHistoryTimer = setInterval(refresh, 5 * 60 * 1000);
  }
  function refresh() { if (quotaHistoryTimer !== null && !quotaHistoryLoading) return refreshPeriod('quota_history', false); }
  return {open:openQuotaHistory,stop,refresh,busy:()=>quotaHistoryLoading,redraw:()=>{if (quotaHistoryPayload) renderQuotaHistory(quotaHistoryPayload);}};
}
