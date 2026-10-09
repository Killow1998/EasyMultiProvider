// Shared call facts and formatting for usage, performance and request details.
function createCallReports({api, getActivity, getState, openModal, $, esc, tr, getLanguage, periodPickerHtml, registerPeriodPicker}) {
  let current = null, timer = null;
  const n = value => typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
  const number = value => n(value) === null ? '—' : value.toLocaleString();
  const seconds = value => n(value) === null ? '—' : (value / 1000).toFixed(2) + ' s';
  const tps = value => n(value) === null ? '—' : value.toFixed(1) + ' token/s';
  const rate = (cached, input) => n(cached) !== null && n(input) > 0 && cached <= input ? (cached / input * 100).toFixed(1) + '%' : '—';
  const status = value => ({active:tr('正在响应','Responding'),completed:tr('已完成','Completed'),failed:tr('失败','Failed'),cancelled:tr('已取消','Cancelled'),interrupted:tr('已中断','Interrupted'),disconnected:tr('连接已断开','Disconnected')})[value] || '—';
  const evidence = value => ({same:tr('名称一致','Names match'),mapped:tr('已配置映射','Configured mapping'),different:tr('名称不同','Names differ'),conflict:tr('声明冲突','Conflicting declarations'),missing:tr('未返回','Not reported')})[value] || tr('未返回','Not reported');
  const time = value => n(value) === null ? '—' : new Date(value * 1000).toLocaleString(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {hour12:false});
  const definition = label => label === 'TTFT' ? tr('开始调用到首个推理、文本或工具输出块。','Time from operation start to the first reasoning, text or tool output block.') : label === 'TPS' ? tr('输出 token 除以整个调用耗时，包含重试等待。','Output tokens divided by the full operation duration, including retry waits.') : '';
  const field = (label, value) => `<div class="call-fact"><span title="${esc(definition(label))}">${esc(label)}</span><strong>${esc(value)}</strong></div>`;
  function recordHtml(row) {
    const session = row.session_name || (row.thread_id || row.session_id || '').slice(0,12) || tr('未关联','Unlinked');
    const selected = row.selected_name || row.client_model || row.model_id || '—';
    const returned = row.response_model || (row.state === 'active' ? tr('等待返回','Awaiting response') : '—');
    const models = [field(tr('Codex 所选模型','Selected in Codex'), selected),field(tr('发往服务端','Sent upstream'),row.upstream_model || '—'),field(tr('服务端回报','Reported upstream'),returned)].join('');
    const inputs = n(row.input_tokens), cached = n(row.cached_input_tokens), written = n(row.cache_write_tokens);
    const uncached = inputs !== null && cached !== null && written !== null && cached + written <= inputs ? inputs - cached - written : null;
    const usage = [field(tr('输入 token','Input tokens'),number(inputs)),field(tr('输出 token','Output tokens'),number(row.output_tokens)),field(tr('缓存读取','Cache read'),number(cached)),field(tr('缓存写入','Cache write'),number(written)),field(tr('未缓存输入','Uncached input'),number(uncached)),field(tr('推理 token','Reasoning tokens'),number(row.reasoning_tokens)),field(tr('输入缓存率','Input cache rate'),rate(cached,inputs)),field(tr('输出缓存率','Output cache rate'),tr('不适用','Not applicable')),field(tr('API 估算 · USD','API estimate · USD'),n(row.pricing?.cost_nanos) === null ? '—' : (row.pricing.cost_nanos / 1e9).toFixed(6))].join('');
    const performance = [field(tr('耗时','Duration'),seconds(row.duration_ms)),field('TTFT',seconds(row.ttft_ms)),field(tr('首正文/工具参数','First text/tool arguments'),seconds(row.first_content_ms)),field('TPS',tps(row.tokens_per_second)),field(tr('发送次数','Attempts'),number(row.attempts))].join('');
    const declarations = (row.model_declarations || []).map(item => `<li><code>${esc(item.model)}</code> · ${esc(item.source)}${item.terminal ? ' · '+tr('终止','Terminal') : ''}</li>`).join('');
    const retries = (row.retries || []).map(item => `<li>${esc(time(item.at))} · ${esc(item.reason)} · ${esc(item.status)}</li>`).join('');
    return `<details class="call-record" data-call-id="${esc(row.request_id+':'+(row.route || 'responses'))}"><summary><span class="call-session">${esc(session)}<small>${esc(time(row.started_at))}</small></span><span class="call-model">${esc(selected)}<small>${tr('回报','Reported')}: ${esc(returned)}</small></span><span>${esc(status(row.state))}</span><span>${number(inputs)} / ${number(row.output_tokens)}<small>${tr('输入 / 输出','Input / output')}</small></span><span>${rate(cached,inputs)}<small>${tr('缓存率','Cache rate')}</small></span><span>${seconds(row.duration_ms)}<small>TTFT ${seconds(row.ttft_ms)} · TPS ${tps(row.tokens_per_second)}</small></span></summary><section class="call-detail"><h3>${tr('会话与模型','Session and models')}</h3><div class="call-facts">${field(tr('会话','Session'),session)}${field(tr('会话 ID','Session ID'),row.thread_id || row.session_id || '—')}${field(tr('父会话','Parent session'),row.parent_thread_id || '—')}${field(tr('轮次','Turn'),row.turn_id || '—')}${models}${field(tr('模型声明','Model declaration'),evidence(row.model_name_status))}</div><h3>${tr('用量','Usage')}</h3><div class="call-facts">${usage}</div><h3>${tr('性能','Performance')}</h3><div class="call-facts">${performance}</div><details class="call-technical"><summary>${tr('技术详情','Technical details')}</summary><div class="call-facts">${field(tr('请求 ID','Request ID'),row.request_id)}${field(tr('服务','Service'),row.provider_name || row.account_id || row.provider_id || '—')}${field(tr('连接','Transport'),row.transport || '—')}${field(tr('协议','Protocol'),row.resolved_protocol || '—')}${field(tr('上游状态','Upstream status'),row.response_status || status(row.state))}${field(tr('交付结果','Delivery'),row.delivery?.delivery || '—')}${field(tr('失败原因','Failure reason'),row.failure_reason || row.error_class || '—')}</div>${declarations ? '<ul>'+declarations+'</ul>' : ''}${retries ? '<h3>'+tr('重试过程','Retries')+'</h3><ol>'+retries+'</ol>' : ''}</details></section></details>`;
  }
  function toolbar() {
    const state = getState() || {};
    const services = [['',tr('全部服务','All services')],['account:@native','Native'],...(state.accounts || []).filter(row => !row.native).map(row => ['account:'+row.id,row.name || row.id]),...(state.providers || []).map(row => ['provider:'+row.id,row.name || row.id])];
    const models = (state.models || []).map(row => [row.id,row.display_name || row.id]);
    if (current.filters.model && !models.some(([id]) => id === current.filters.model)) models.push([current.filters.model,current.filters.model]);
    const options = values => values.map(([value,label]) => `<option value="${esc(value)}">${esc(label)}</option>`).join('');
    return `${periodPickerHtml('calls', ['1d','7d','30d'])}<div class="call-toolbar"><select data-call-filter="service" aria-label="${tr('服务','Service')}">${options(services)}</select><select data-call-filter="category" aria-label="${tr('来源','Source')}">${options([['',tr('全部来源','All sources')],['native','Native'],['subscription',tr('订阅','Subscription')],['external','API'],['unknown',tr('未关联','Unlinked')]])}</select><select data-call-filter="model" aria-label="${tr('模型','Model')}">${options([['',tr('全部模型','All models')],...models])}</select><select data-call-filter="state" aria-label="${tr('状态','Status')}">${options([['',tr('全部状态','All states')],['completed',tr('已完成','Completed')],['failed',tr('失败','Failed')],['cancelled',tr('已取消','Cancelled')]])}</select><input data-call-filter="session" placeholder="${tr('会话 ID','Session ID')}" aria-label="${tr('会话 ID','Session ID')}"></div><div class="report-tabs" role="group" aria-label="${tr('视图','View')}">${current.performance ? ['overview','models','calls'].map((view,index) => `<button type="button" class="secondary" data-call-view="${view}">${[tr('总览','Overview'),tr('模型','Models'),tr('调用','Calls')][index]}</button>`).join('') : ''}</div><div class="report-status" data-report-status role="status" aria-live="polite"></div><div data-call-content></div>`;
  }
  function trendHtml(report) {
    const metric = current?.metric || 'tokens_per_second';
    const rows = (report.periods || []).filter(row => n(row[metric]) !== null);
    const bucket = Math.max(60,(report.end-report.start)/48);
    const segments = [];
    for (const row of rows) { if (!segments.length || row.start - segments.at(-1).at(-1).start > bucket * 1.5) segments.push([]); segments.at(-1).push(row); }
    const maximum = Math.max(1,...rows.map(row => row[metric]));
    const start = report.start, span = report.end-start || 1;
    const points = rows.map(row => ({row,x:20+(row.start-start)/span*720,y:110-row[metric]/maximum*90}));
    const format = value => metric === 'ttft_ms' ? seconds(value) : metric === 'tokens_per_second' ? tps(value) : number(value);
    const buttons = [['tokens_per_second','TPS'],['ttft_ms','TTFT'],['calls',tr('调用量','Calls')]].map(([key,label]) => `<button type="button" class="secondary${metric === key ? ' active' : ''}" aria-pressed="${metric === key}" data-call-metric="${key}">${label}</button>`).join('');
    return `<div class="report-tabs" role="group" aria-label="${tr('指标','Metric')}">${buttons}</div><svg class="call-trend" viewBox="0 0 760 140" role="img" aria-label="${esc(metric === 'ttft_ms' ? 'TTFT' : metric === 'tokens_per_second' ? 'TPS' : tr('调用量','Calls'))}"><line x1="20" y1="110" x2="740" y2="110" stroke="var(--border)"/>${segments.map(segment => '<polyline points="'+segment.map(row => (20+(row.start-start)/span*720).toFixed(1)+','+(110-row[metric]/maximum*90).toFixed(1)).join(' ')+'" fill="none" stroke="var(--accent)" stroke-width="2"/>').join('')}${points.map(p => `<circle cx="${p.x.toFixed(1)}" cy="${p.y.toFixed(1)}" r="4" fill="var(--accent)"><title>${esc(time(p.row.start))} · ${esc(format(p.row[metric]))} · ${tr('样本','Samples')} ${number(metric === 'ttft_ms' ? p.row.ttft_samples : metric === 'tokens_per_second' ? p.row.tps_samples : p.row.calls)}</title></circle>`).join('')}<text x="20" y="12" fill="var(--muted)" font-size="11">${esc(format(maximum))}</text><text x="20" y="135" fill="var(--muted)" font-size="11">${esc(time(start))}</text><text x="740" y="135" text-anchor="end" fill="var(--muted)" font-size="11">${esc(time(report.end))}</text></svg>`;
  }
  function summaryHtml(report) {
    const summary = report.summary || {};
    return `<div class="call-facts call-overview">${field(tr('调用数','Calls'),number(summary.calls))}${field(tr('成功率','Success rate'),summary.calls ? (summary.completed / summary.calls * 100).toFixed(1)+'%' : '—')}${field(tr('平均耗时','Mean duration'),seconds(summary.duration_ms))}${field('TTFT',seconds(summary.ttft_ms))}${field('TPS',tps(summary.tokens_per_second))}${field(tr('输入缓存率','Input cache rate'),rate(summary.cached_input_tokens,summary.cache_input_tokens))}</div><p class="muted">${tr('有效样本','Valid samples')}: TTFT ${number(summary.ttft_samples)} · TPS ${number(summary.tps_samples)}</p>${trendHtml(report)}`;
  }
  function modelHtml(report) {
    const rows = (report.models || []).map(row => `<tr><td><code>${esc(row.model_id)}</code><small>${esc(row.account_id || row.provider_id)} · ${esc(row.upstream_model)} · ${esc(row.speed_mode)} · ${esc(row.requested_effort)}</small></td><td>${number(row.calls)}</td><td>${seconds(row.duration_ms)}</td><td>${seconds(row.ttft_ms)}</td><td>${tps(row.tokens_per_second)}</td></tr>`).join('');
    return `<div class="usage-table"><table><thead><tr>${[['model',tr('模型 / 服务','Model / service')],['calls',tr('调用','Calls')],['duration',tr('平均耗时','Mean duration')],['ttft','TTFT'],['tps','TPS']].map(([key,label]) => '<th><button type="button" class="secondary" data-call-sort="'+key+'">'+label+(current.modelsSort === key ? (key === 'model' ? ' ↑' : ' ↓') : '')+'</button></th>').join('')}</tr></thead><tbody>${rows}</tbody></table></div><div class="call-pages"><button type="button" class="secondary" data-call-action="models-previous" ${report.models_offset ? '' : 'disabled'}>‹</button><span>${number(report.models_total)} ${tr('组','groups')}</span><button type="button" class="secondary" data-call-action="models-next" ${report.models_offset + report.models_limit < report.models_total ? '' : 'disabled'}>›</button></div>`;
  }
  function matches(row, filters) {
    return (!filters.category || row.usage_category === filters.category) && (!filters.provider || row.provider_id === filters.provider) && (!filters.account || row.account_id === filters.account) && (!filters.model || row.model_id === filters.model) && (!filters.models?.length || filters.models.includes(row.model_id)) && (!filters.session || (row.thread_id || row.session_id) === filters.session) && (!filters.state || row.state === filters.state);
  }
  function render(target, report) {
    const focused = target.ownerDocument?.activeElement;
    const focus = focused && target.contains(focused) && ['callMetric','callSort'].find(key => focused.dataset[key]);
    const open = new Set([...target.querySelectorAll('details[data-call-id][open]')].map(node => node.dataset.callId));
    const active = current.offset === 0 ? (getActivity()?.requests || []).filter(row => row.state === 'active' && matches(row,current.filters) && row.started_at >= report.start && row.started_at <= report.end) : [];
    const ids = new Set(active.map(row => row.request_id));
    const rows = [...active,...(report.records || []).filter(row => !ids.has(row.request_id))];
    const pages = `<div class="call-pages"><button type="button" class="secondary" data-call-action="previous" ${report.offset ? '' : 'disabled'}>‹</button><span>${number(report.total)} ${tr('条记录','records')}</span><button type="button" class="secondary" data-call-action="next" ${report.offset + report.limit < report.total ? '' : 'disabled'}>›</button></div>`;
    $(current.id).querySelectorAll('[data-call-view]').forEach(button => {
      const active = button.dataset.callView === current.view;
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    });
    const content = current.view === 'models' ? modelHtml(report) : current.view === 'overview' ? summaryHtml(report) : rows.map(recordHtml).join('') + pages;
    target.innerHTML = (rows.length || current.view !== 'calls' ? content : `<p class="muted">${tr('暂无调用记录','No calls recorded')}</p>${pages}`);
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
  function stop() { current?.query.cancel(); current = null; if (timer !== null) clearTimeout(timer); timer = null; }
  function mount(id, filters = {}, performance = false) {
    stop();
    const root = $(id); if (!root) return;
    current = {id,filters:{...filters},offset:0,modelsOffset:0,modelsSort:'calls',view:performance ? 'overview' : 'calls',performance};
    root.innerHTML = toolbar();
    const selected = current;
    current.query = createReportQuery({api,
      onState:(loading,error) => setReportState(root,loading,error,tr),
      onResult:report => { selected.report = report; render(root.querySelector('[data-call-content]'),report); },
    });
    for (const field of ['model','state','session','category']) { if (filters[field]) root.querySelector(`[data-call-filter="${field}"]`).value = filters[field]; }
    root.querySelector('[data-call-filter="service"]').value = filters.provider ? 'provider:'+filters.provider : filters.account ? 'account:'+filters.account : '';
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
    const range = filters.start !== undefined || filters.end !== undefined
      ? {start:filters.start ?? 0,end:filters.end ?? Date.now()/1000} : null;
    registerPeriodPicker('calls','1d', period => {
      current.filters.start = period.start; current.filters.end = period.end;
      current.offset = 0; current.modelsOffset = 0;
      return load();
    },range);
  }
  function open(filters = {}) { openModal(tr('调用详情','Call details'),'<div id="call_report"></div>','',null); mount('call_report',filters); }
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
  return {open,openActivity,normalize,mount,refresh,live,stop,recordHtml,number,seconds,tps,rate};
}
