// Owns data and outstanding work for one usage window.
function createUsageReport({api, $, tr, esc, getState, getLanguage, persistState, openModal,
  serviceUsage, callReports, eventsConnected, periods}) {
  const {periodPickerHtml, registerPeriodPicker, setPeriod, periodPickers, periodLocalInput, timeAxisSvg} = periods;
  let usageQuery = null, usagePayload = null, usagePoll = null, scanController = null;
  const usageSource = value => ({native:'Native', subscription:tr('其他 Subscription','Other Subscription'), external:'External Provider', unknown:tr('来源未确认','Unknown source')}[value] || value);
  const usageMoney = nanos => new Intl.NumberFormat(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {style:'currency', currency:'USD', minimumFractionDigits:2, maximumFractionDigits:4}).format(nanos / 1e9);
  const usageNumber = value => new Intl.NumberFormat(getLanguage() === 'en' ? 'en-US' : 'zh-CN').format(value || 0);
  function usageBucketFor(period) { const span = period.end - period.start; return period.preset === 'all' || span > 3*86400 ? 'day' : span > 3*3600 ? 'hour' : 'minute'; }
  function openUsage() {
    usagePayload = null;
    openModal(tr('用量与 API 估算','Usage & API estimates'), `<div id="usage_report">
      ${periodPickerHtml('usage', ['1h','1d','7d','all'])}
      <div class="usage-filters"><label>${tr('来源','Source')}<select id="usage_category" data-usage-change="load"><option value="all">${tr('全部来源','All sources')}</option><option value="native">Native</option><option value="subscription">${tr('其他 Subscription','Other Subscription')}</option><option value="external">External Provider</option><option value="unknown">${tr('来源未确认','Unknown source')}</option></select></label><label>${tr('时间粒度','Group by')}<select id="usage_bucket" data-usage-change="render"><option value="minute">${tr('分钟','Minute')}</option><option value="hour">${tr('小时','Hour')}</option><option value="day">${tr('天','Day')}</option></select></label><button type="button" class="secondary" data-report-action="scan" data-usage-action="scan">${tr('扫描历史','Scan history')}</button><button type="button" class="secondary" data-usage-action="calls">${tr('调用详情','Call details')}</button></div>
      <div id="usage_status" class="report-status" data-report-status role="status" aria-live="polite"></div><div id="usage_result"></div>
      <p class="usage-notice">${tr('扫描本机 Codex 历史，并记录 EMP 实时用量，跨重启保留。输入包含缓存，输出包含推理；子项不重复相加。','Scans local Codex history and records EMP usage across restarts. Input includes cache; output includes reasoning. Subsets are not added twice.')}</p></div>`, '', null);
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
    registerPeriodPicker('usage', '1d', (period, options) => { if (options.user && $('usage_bucket')) $('usage_bucket').value = usageBucketFor(period); return loadUsage(); });
  }
  function openUsageCalls() {
    const period = periodPickers.usage;
    const category = $('usage_category')?.value;
    const filters = period ? {start:period.start,end:period.end} : {};
    if (category && category !== 'all') filters.category = category;
    callReports.open(filters);
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
      usageQuery.cancel(); setReportState(root,false,tr('请选择有效的时间段。','Choose a valid period.'),tr); return;
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
  const usageRowSource = row => row.category === 'unknown' && row.owner.startsWith('history:') ? tr('历史路由','Historical route') : usageSource(row.category);
  function usageOwner(row) {
    if (row.owner.startsWith('history:')) return row.category === 'unknown' ? row.owner.slice(8) : tr('历史路由 · ','Historical route · ') + row.owner.slice(8);
    if (row.category === 'external') return row.owner_name || row.owner;
    if (row.owner.startsWith('account:')) return row.owner_name || tr('账号 · ','Account · ') + row.owner.slice(8,16);
    if (row.owner.startsWith('credential:')) return (row.owner_name || row.owner.slice(11,19)) + tr(' · 身份未确认',' · identity unconfirmed');
    return tr('旧记录或路由 · 账号身份未确认','Legacy record or route · account identity unconfirmed');
  }
  // Models without a public price cost 0 unless they are priced as another model.
  function usagePricingEditor(data) {
    const aliases = getState()?.pricing_aliases || {};
    const counts = new Map((data.unpriced_models || []).map(row => [row.route_model, row.requests]));
    const models = [...new Set([...counts.keys(), ...Object.keys(aliases)])].filter(Boolean);
    if (!models.length) return '';
    const rows = models.map(model => `<tr><td><code>${esc(model)}</code>${counts.has(model) ? `<small>${usageNumber(counts.get(model))} ${tr('次按 0 计','requests counted as 0')}</small>` : ''}</td><td><input data-pricing-alias="${esc(model)}" value="${esc(aliases[model] || '')}" placeholder="${tr('按哪个模型计价，例如 gpt-5.5','Price as, e.g. gpt-5.5')}"></td></tr>`).join('');
    return `<details class="usage-notice"><summary>${tr('计价对照','Pricing references')}${counts.size ? ` · ${counts.size} ${tr('个模型没有公开价格，按 0 计','models have no public price and count as 0')}` : ''}</summary><p class="muted">${tr('可以为这些模型指定按哪个公开模型估价；留空则按 0 计。保存后会重新计算历史记录。','Optionally choose a public model to estimate these with; leave empty to count them as 0. Saving re-prices past records.')}</p><table class="pricing-aliases">${rows}</table><div class="toolbar"><button class="secondary" data-icon="save" data-usage-action="pricing">${tr('保存计价对照','Save pricing references')}</button></div></details>`;
  }
  async function saveUsagePricing() {
    const root = $('usage_report'); if (!root) return;
    const aliases = {...(getState().pricing_aliases || {})};
    root.querySelectorAll('[data-pricing-alias]').forEach(input => {
      const value = input.value.trim();
      if (value) aliases[input.dataset.pricingAlias] = value; else delete aliases[input.dataset.pricingAlias];
    });
    const candidate = structuredClone(getState()); candidate.pricing_aliases = aliases;
    try { await persistState(tr('计价对照已保存，正在重新计价…','Pricing references saved; re-pricing…'), candidate); }
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
    box.innerHTML = `<p class="usage-notice">${history.running || history.queued ? tr('正在后台扫描历史…','Scanning history in the background…') : history.last_scan_at ? tr('历史已扫描 · ','History scanned · ') + usageNumber(history.files) + tr(' 个文件',' files') : tr('等待历史扫描','Waiting for history scan')}${history.errors ? ' · ' + tr('部分文件读取失败','Some files could not be read') : ''}</p>
      <div class="usage-totals"><div class="usage-total"><span>Tokens</span><strong>${totals.input_reports || totals.output_reports ? usageNumber(totals.input_tokens+totals.output_tokens) : '—'}</strong><span>${usageNumber(totals.reported_requests)} / ${usageNumber(totals.requests)} ${tr('次请求有完整用量','requests reported usage')}</span></div><div class="usage-total"><span>${tr('API 等价估算 · USD','API equivalent · USD')}</span><strong>${cost(totals)}</strong><span>${usageNumber(totals.priced_requests)} / ${usageNumber(totals.requests)} ${tr('次请求已计价','requests priced')}</span></div></div>
      ${totals.requests ? `<svg class="usage-chart" viewBox="0 0 ${width} 190" aria-label="${tr('Token 用量，点击柱形查看明细','Token usage; select a bar to zoom')}"><path d="M70 20V150H698" fill="none" stroke="var(--border)"/><text x="60" y="28" text-anchor="end">${new Intl.NumberFormat('en',{notation:'compact'}).format(max)}</text><text x="60" y="153" text-anchor="end">0</text>${bars}${timeAxisSvg(data.start, data.end, time => 70 + (time-data.start)/range*plot, 170, 70, 698)}</svg>${groups}` : `<p>${tr('这个时间段还没有用量记录。','No usage recorded in this period.')}</p>`}
      ${historical ? `<p class="usage-notice">${usageNumber(historical)} ${tr('条来自本机历史，按扫描时可用价格折算；旧账号身份未确认，类别仅依据历史模型路由和现有配置推断。未记录的服务档位按普通模式估算。','records from local history, estimated at available scan-time prices. Old account identities are unconfirmed; categories are inferred from model routes and current configuration. Unrecorded service tiers use standard rates.')}</p>` : ''}
      ${data.unmatched_overlap || (historical && data.uncorrelated_realtime) ? `<p class="usage-notice usage-warning">${tr('部分历史与实时用量无法可靠匹配（缺少轮次信息或上报值不一致），合计可能包含重叠。','Some history and live usage cannot be reconciled (missing turn metadata or differing counts); totals may include overlap.')}</p>` : ''}
      ${data.issues?.some(row => ['inconsistent_usage','aggregate_usage'].includes(row.price_issue)) ? `<p class="usage-notice usage-warning">${tr('部分旧记录的用量字段不一致，或只有累计值；保留上报 token，暂不计价。','Some old usage fields disagree or contain only cumulative counts. Reported tokens are retained without pricing.')}</p>` : ''}
      ${missing ? `<p class="usage-notice">${missing} ${tr('次请求没有回报用量（多为中断或失败的请求），未计入费用。','requests reported no usage (mostly interrupted or failed) and are not included in cost.')}</p>` : ''}
      ${usagePricingEditor(data)}
      ${price.stale || price.error ? `<p class="usage-notice usage-warning">${tr('价格未更新，已有费率仅供参考。','Prices are not up to date; saved rates are for reference.')}</p>` : ''}
      ${data.write_error ? `<p class="usage-notice usage-warning">${tr('用量写入失败，当前统计可能不完整。请查看 EMP 日志。','Usage could not be saved; totals may be incomplete. Check the EMP log.')}</p>` : ''}
      <details class="usage-notice"><summary>${tr('计价说明与价格更新时间','Pricing basis & updates')}</summary><p>${tr('按请求完成时的本地 API 价目估算，缺价时在价格补齐后计价，已计价历史不再改写。包含缓存折扣和可确认的长上下文、服务档位费率。并非订阅账单，不含工具调用费、缓存存储费、税费或转售加价。缺少缓存明细时无法精确计价。','Uses local API rates at request completion, or the first available rate if initially missing. Priced history is frozen. Includes cache discounts and known context/service tiers. Not a subscription bill; excludes tool fees, cache storage, tax and reseller markups. Missing cache details prevent a precise estimate.')}</p><p><a href="${esc(price.url)}" target="_blank" rel="noopener noreferrer">LiteLLM</a> · ${tr('每 24 小时后台更新','Updated in the background every 24 hours')} · ${price.fetched_at ? esc(new Date(price.fetched_at*1000).toLocaleString()) : tr('尚未获取价格','Prices not downloaded')}</p>${price.stale || price.error ? `<p class="usage-warning">${tr('价格尚未更新，暂用已有价目；没有价目的模型保留为未计价。','Prices are not up to date. Saved rates are used where available; other models remain unpriced.')}</p>` : ''}</details>`;
  }

  return {open:openUsage, refresh:loadUsage, stop:stopUsage};
}
