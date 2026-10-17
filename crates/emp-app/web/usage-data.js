// All chart groupings project the same reconciled rows; grouping cannot alter totals.
function createUsageData({tr,esc,number}) {
  const palette = ['#537de0','#279e9b','#c98730','#a371ce','#d2637d','#559746','#77838e','#547db1'];
  const key = row => JSON.stringify([row.category,row.owner]);
  const service = row => row.owner_name || (String(row.owner).startsWith('history:') ? tr("reports.unlinked_historical_usage")+' · '+row.owner.slice(8) : /^(account|credential):/.test(row.owner) ? tr("reports.unlinked_account") : row.category === 'native' ? 'Native' : row.owner || tr("reports.unlinked"));
  const category = value => ({native:'Codex · Native',subscription:tr("reports.codex_subscription"),external:tr("reports.external_providers"),unknown:tr("reports.unlinked")})[value] || value;
  const usd = row => row.priced_requests ? 'US$'+(row.cost_nanos/1e9).toFixed(4) : '—';
  const color = value => palette[[...value].reduce((n,c) => (n*31+c.codePointAt(0))>>>0,0)%palette.length];
  function chart(report,metric,grouping) {
    const count = Math.ceil((report.end-report.start)/report.bucket), groups = new Map();
    for (const row of report.series) {
      const id = grouping === 'services' ? key(row) : grouping === 'models' ? row.model : 'all';
      if (!groups.has(id)) groups.set(id,{label:grouping === 'services' ? service(row) : grouping === 'models' ? row.model : tr("reports.total_usage"),color:grouping === 'none' ? 'var(--accent)' : color(id),values:Array(count).fill(0)});
      const index = Math.round((row.start-report.start)/report.bucket);
      groups.get(id).values[index] += metric === 'tokens' ? row.input_tokens+row.output_tokens : row.cost_nanos;
    }
    const series = [...groups.values()].sort((a,b) => a.label.localeCompare(b.label));
    const periods = Array.from({length:count},(_,i) => ({start:report.start+i*report.bucket,[metric]:series.reduce((n,item) => n+item.values[i],0),requests:report.periods.find(row => Math.round((row.start-report.start)/report.bucket) === i)?.requests || 0}));
    return {...report,periods,chartSeries:series};
  }
  function table(report,mode) {
    const groups = new Map();
    for (const row of report.groups) {
      const id = JSON.stringify([row.category,row.owner,...(mode === 'models' ? [row.model] : [])]);
      if (!groups.has(id)) groups.set(id,{...row,input_tokens:0,output_tokens:0,cached_input_tokens:0,cost_nanos:0,requests:0,priced_requests:0});
      for (const field of ['input_tokens','output_tokens','cached_input_tokens','cost_nanos','requests','priced_requests']) groups.get(id)[field]+=row[field];
    }
    const rows=[...groups.values()].sort((a,b) => a.category.localeCompare(b.category)||service(a).localeCompare(service(b))||a.model.localeCompare(b.model));
    let previous='';
    const html=rows.map(row => {
      const heading=row.category !== previous ? `<tr class="presentation-usage-category"><th colspan="6">${esc(category(row.category))}</th></tr>` : ''; previous=row.category;
      return `${heading}<tr><td><strong>${esc(mode === 'models' ? row.model : service(row))}</strong>${mode === 'models' ? `<small>${esc(service(row))}</small>` : row.owner_email ? `<small>${esc(row.owner_email)}</small>` : ''}</td><td>${number(row.requests)}</td><td>${number(row.input_tokens)}</td><td>${number(row.output_tokens)}</td><td>${number(row.cached_input_tokens)}</td><td>${usd(row)}</td></tr>`;
    }).join('');
    return `<div class="usage-table"><table><thead><tr>${[mode === 'models' ? tr("reports.model_service") : tr("reports.service"),tr("reports.records_2"),tr("services.input"),tr("services.output"),tr("reports.cache_read"),'API · USD'].map(title=>`<th>${esc(title)}</th>`).join('')}</tr></thead><tbody>${html || `<tr><td colspan="6">${tr("reports.no_usage_in_this_period")}</td></tr>`}</tbody></table></div>`;
  }
  return {chart,table,usd};
}
