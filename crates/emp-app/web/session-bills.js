// Bills reuse reconciled usage. Detail requests belong to the currently open row.
function createSessionBills({api,tr,esc,number,getLanguage}) {
  let current=null, searchTimer=null;
  const time=value=>new Date(value*1000).toLocaleString(getLanguage()==='en'?'en-US':'zh-CN',{hour12:false});
  const count=(row,field,coverage)=>{
    if (!row[coverage]) return '—';
    const value=number(row[field]);
    if (row[coverage]>=row.requests) return value;
    const help=coverage.startsWith('cache') ? tr('reports.bill_partial_cache') : tr('reports.bill_missing_usage',{count:number(row.requests-row[coverage])});
    return `<span title="${esc(help)}">${value} *</span>`;
  };
  const tokens=row=>row.input_reports || row.output_reports ? number(row.input_tokens+row.output_tokens)+(row.reported_requests<row.requests?' *':'') : '—';
  const usd=row=>row.requests>row.unpriced_requests ? 'US$'+(row.cost_nanos/1e9).toFixed(4)+(row.unpriced_requests?' *':'') : '—';
  function notices(row) {
    return [row.unpriced_requests ? tr('reports.bill_unpriced',{count:number(row.unpriced_requests)}) : '',
      row.reported_requests<row.requests ? tr('reports.bill_missing_usage',{count:number(row.requests-row.reported_requests)}) : '']
      .filter(Boolean).map(text=>`<p class="usage-notice">${esc(text)}</p>`).join('');
  }
  function detailHtml(report,id) {
    const groups=new Map(), fields=['input_tokens','output_tokens','cached_input_tokens','cache_write_tokens','cost_nanos','requests','reported_requests','priced_requests','unpriced_requests','input_reports','output_reports','cache_reports','cache_write_reports'];
    for (const row of report.groups) {
      if (!groups.has(row.model)) groups.set(row.model,Object.fromEntries(fields.map(field=>[field,0])));
      const group=groups.get(row.model);
      for (const field of fields) group[field]+=row[field] || 0;
    }
    const rows=[...groups].sort((a,b)=>(b[1].input_tokens+b[1].output_tokens)-(a[1].input_tokens+a[1].output_tokens)).map(([model,row])=>
      `<tr><td><strong>${esc(model)}</strong></td><td>${count(row,'input_tokens','input_reports')}</td><td>${count(row,'output_tokens','output_reports')}</td><td>${count(row,'cached_input_tokens','cache_reports')}</td><td>${count(row,'cache_write_tokens','cache_write_reports')}</td><td>${usd(row)}</td></tr>`).join('');
    return `<p class="session-bill-id"><span>${tr('reports.session_id')}</span> <code>${esc(id)}</code></p><div class="usage-table"><table><thead><tr>${['reports.model','reports.input_tokens','reports.output_tokens','reports.cache_read','reports.cache_write','reports.api_estimate_usd'].map(key=>`<th>${tr(key)}</th>`).join('')}</tr></thead><tbody>${rows || `<tr><td colspan="6">${tr('reports.no_usage_in_this_period')}</td></tr>`}</tbody></table></div>${notices(report.totals)}`;
  }
  function cancelDetails() {
    for (const query of current?.details.values() || []) query.cancel();
    current?.details.clear();
  }
  function toggleDetail(selected,node) {
    if (current!==selected || !selected.root.contains(node)) return;
    const id=node.dataset.sessionBill;
    if (!node.open) {selected.details.get(id)?.cancel();selected.details.delete(id);return;}
    if (selected.details.has(id)) return;
    const query=createReportQuery({api,onState:(loading,error)=>setReportState(node,loading,error,tr),
      onResult:report=>{node.querySelector('[data-bill-detail]').innerHTML=detailHtml(report,id);}});
    selected.details.set(id,query);
    void query.run('/api/usage?'+new URLSearchParams({...selected.filters,series:true,session:id}));
  }
  function render(report) {
    if (!current) return;
    if (report.offset && report.offset>=report.total) {current.offset=Math.floor(Math.max(0,report.total-1)/report.limit)*report.limit;void load();return;}
    const selected=current, root=selected.root, open=new Set([...root.querySelectorAll('[data-session-bill][open]')].map(node=>node.dataset.sessionBill));
    cancelDetails();
    root.querySelector('[data-bill-indexing]').hidden=!(report.history?.running || report.history?.queued);
    const totals=report.totals, unknown=report.unassigned;
    root.querySelector('[data-bill-totals]').innerHTML=`<div class="call-facts call-overview session-bill-totals"><div class="call-fact"><span>${tr('reports.sessions')}</span><strong>${number(report.total)}</strong></div><div class="call-fact"><span>${tr('reports.total_tokens')}</span><strong>${tokens(totals)}</strong></div><div class="call-fact"><span>${tr('reports.api_estimate_usd')}</span><strong>${usd(totals)}</strong></div></div>${notices(totals)}${unknown?.requests ? `<p class="usage-notice">${tr('reports.bill_unassigned',{count:number(unknown.requests),tokens:tokens(unknown)})}</p>` : ''}`;
    root.querySelector('[data-bill-list]').innerHTML=report.records.map(row=>`<details class="session-bill" data-session-bill="${esc(row.session_id)}"><summary><span><strong>${esc(row.session_name || tr('reports.unnamed_session'))}</strong><small>${esc(time(row.last_at))} · ${tr('reports.bill_records',{count:number(row.requests)})}</small></span><span><small>${tr('reports.total_tokens')}</small><strong>${tokens(row)}</strong></span><span><small>${tr('reports.cache_read')}</small><strong>${count(row,'cached_input_tokens','cache_reports')}</strong></span><span><small>${tr('reports.api_estimate_usd')}</small><strong>${usd(row)}</strong></span></summary><div class="session-bill-detail"><p class="report-status" data-report-status role="status" aria-live="polite"></p><div data-bill-detail></div></div></details>`).join('') || `<p class="muted">${tr('reports.no_session_bills')}</p>`;
    root.querySelector('[data-bill-pages]').innerHTML=`<button type="button" class="secondary" data-bill-page="previous" aria-label="${tr('reports.previous_page')}" ${report.offset ? '' : 'disabled'}>‹</button><span>${tr('reports.bill_page',{first:number(report.total?report.offset+1:0),last:number(Math.min(report.offset+report.records.length,report.total)),total:number(report.total)})}</span><button type="button" class="secondary" data-bill-page="next" aria-label="${tr('reports.next_page')}" ${report.offset+report.limit<report.total ? '' : 'disabled'}>›</button>`;
    for (const node of root.querySelectorAll('[data-session-bill]')) {
      node.addEventListener('toggle',()=>toggleDetail(selected,node));
      if (open.has(node.dataset.sessionBill)) node.open=true;
    }
  }
  function load() {
    if (!current) return;
    clearTimeout(searchTimer);searchTimer=null;
    cancelDetails();
    return current.query.run('/api/usage?'+new URLSearchParams({...current.filters,sessions:true,q:current.search,sort:current.sort,offset:current.offset,limit:20}));
  }
  function stop() {
    clearTimeout(searchTimer);searchTimer=null;
    current?.query.cancel();cancelDetails();
    if (current) {current.root.oninput=null;current.root.onchange=null;current.root.onclick=null;}
    current=null;
  }
  function mount(root,filters,refresh=false) {
    const key=JSON.stringify(filters);
    if (current?.root===root) {
      if (current.key!==key) {current.filters={...filters};current.key=key;current.offset=0;refresh=true;}
      if (refresh) return load();
      return;
    }
    stop();
    current={root,filters:{...filters},key,search:'',sort:'recent',offset:0,details:new Map()};
    root.innerHTML=`<div class="session-bill-tools"><input type="search" data-bill-search maxlength="128" placeholder="${tr('reports.session_search')}" aria-label="${tr('reports.session_search')}"><select data-bill-sort aria-label="${tr('reports.session_sort')}">${[['recent','reports.recent_activity'],['tokens','reports.total_tokens'],['cost','reports.api_estimate_usd']].map(([value,label])=>`<option value="${value}">${tr(label)}</option>`).join('')}</select></div><p class="usage-notice">${tr('reports.bill_scope')}</p><p class="report-status" data-report-status role="status" aria-live="polite"></p><p class="report-status" data-bill-indexing hidden role="status" aria-live="polite">${tr('reports.bill_indexing')}</p><div data-bill-totals></div><div data-bill-list></div><div class="call-pages" data-bill-pages></div>`;
    current.query=createReportQuery({api,onState:(loading,error)=>setReportState(root,loading,error,tr),onResult:render});
    const selected=current;
    root.oninput=event=>{
      if (!event.target.hasAttribute('data-bill-search')) return;
      clearTimeout(searchTimer);
      selected.search=event.target.value;selected.offset=0;
      // Cancel before the debounce so a stale result cannot win while typing.
      selected.query.cancel();cancelDetails();
      searchTimer=setTimeout(()=>{searchTimer=null;if(current===selected) void load();},250);
    };
    root.onchange=event=>{if (event.target.hasAttribute('data-bill-sort')) {selected.sort=event.target.value;selected.offset=0;void load();}};
    root.onclick=event=>{const button=event.target.closest('[data-bill-page]');if (!button || button.disabled) return;selected.offset=Math.max(0,selected.offset+(button.dataset.billPage==='next'?20:-20));void load();};
    return load();
  }
  return {mount,stop};
}
