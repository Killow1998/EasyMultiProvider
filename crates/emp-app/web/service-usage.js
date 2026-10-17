// Service-owned lifetime summaries and service-type/model usage breakdowns.
function createServiceUsage({esc, tr, number, money, dateTime, renderLifetime}) {
  const fields = ['requests', 'priced_requests', 'input_reports', 'output_reports',
    'input_tokens', 'output_tokens', 'cost_nanos', 'cache_reports', 'cached_input_tokens',
    'cache_write_tokens', 'reasoning_reports', 'reasoning_tokens'];
  const cost = row => row.priced_requests ? money(row.cost_nanos) : '—';
  function aggregate(groups) {
    const models = new Map(), totals = Object.fromEntries(fields.map(key => [key, 0]));
    for (const row of groups || []) {
      const name = String(row.model || '');
      if (!models.has(name)) models.set(name, {model:name, details:[], ...Object.fromEntries(fields.map(key => [key, 0]))});
      const model = models.get(name); model.details.push(row);
      for (const key of fields) {
        const value = Number(row[key]);
        if (Number.isFinite(value) && value >= 0) { model[key] += value; totals[key] += value; }
      }
    }
    return {models:[...models.values()], totals};
  }
  function render(data) {
    if (renderLifetime) return renderLifetime(data,{aggregate,number,money,dateTime});
    const {models, totals} = aggregate(data.groups);
    const rows = models.sort((a, b) => a.model.localeCompare(b.model)).map(row =>
      `<tr><td>${esc(row.model)}</td><td>${row.input_reports ? number(row.input_tokens) : '—'}</td><td>${row.output_reports ? number(row.output_tokens) : '—'}</td><td>${cost(row)}</td></tr>`).join('');
    return `<h3>${tr("services.lifetime_usage")}</h3>
      <div class="usage-totals"><div class="usage-total"><span>${tr("common.tokens")}</span><strong>${totals.input_reports || totals.output_reports ? number(totals.input_tokens + totals.output_tokens) : '—'}</strong></div><div class="usage-total"><span>${tr("services.api_equivalent_usd")}</span><strong>${cost(totals)}</strong></div></div>
      <p class="muted">${number(totals.priced_requests)} / ${number(totals.requests)} ${tr("services.requests_priced")}${data.first_record_at ? ' · ' + tr("services.recorded_since") + esc(dateTime(data.first_record_at)) : ''}</p>
      ${totals.priced_requests < totals.requests ? `<p class="muted">${tr("services.some_model_prices_are_unavailable_the_estimate_includes_known")}</p>` : ''}
      <div class="usage-table"><table><thead><tr><th>${tr("reports.model")}</th><th>${tr("services.input")}</th><th>${tr("services.output")}</th><th>${tr("common.usd")}</th></tr></thead><tbody>${rows || `<tr><td colspan="4">${tr("services.no_usage_recorded")}</td></tr>`}</tbody></table></div>`;
  }
  function renderGroups(data, providers, describe) {
    const services = new Map((providers || []).map(provider => [provider.id, provider]));
    const types = [
      {id:'codex', label:tr("services.codex_subscriptions"), rows:[]},
      {id:'claude', label:tr("services.claude_code_cpa"), rows:[]},
      {id:'api', label:'API', rows:[]},
      {id:'other', label:tr("services.other_records"), rows:[]},
    ];
    for (const row of data.groups || []) {
      const provider = services.get(row.owner);
      const type = ['native','subscription'].includes(row.category) ? 'codex'
        : row.category === 'external' && provider ? (provider.execution_backend === 'claude_cli' ? 'claude' : 'api') : 'other';
      types.find(group => group.id === type).rows.push(row);
    }
    const input = row => `${row.input_reports ? number(row.input_tokens) : '—'}<small>${tr("services.cached")}${row.cache_reports ? number(row.cached_input_tokens) : '—'}${row.cache_write_tokens ? ' · '+tr("services.write")+number(row.cache_write_tokens) : ''}</small>`;
    const output = row => `${row.output_reports ? number(row.output_tokens) : '—'}<small>${tr("services.reasoning")}${row.reasoning_reports ? number(row.reasoning_tokens) : '—'}</small>`;
    return types.filter(group => group.rows.length).map(group => {
      const {models} = aggregate(group.rows);
      const rows = models.sort((a,b) => (b.cost_nanos-a.cost_nanos)||(b.input_tokens-a.input_tokens)||a.model.localeCompare(b.model)).map(model => {
        const details = model.details.slice().sort((a,b) => (b.cost_nanos-a.cost_nanos)||(b.input_tokens-a.input_tokens)).map(row => `<tr><td>${esc(describe(row))}<small>${row.service_tier === 'default' ? tr("services.standard") : esc(row.service_tier || '')}</small></td><td>${input(row)}</td><td>${output(row)}</td><td>${cost(row)}</td><td>${number(row.requests || 0)}</td></tr>`).join('');
        return `<details class="usage-model"><summary class="usage-model-columns"><span class="usage-model-name">${esc(model.model)}</span><span>${input(model)}</span><span>${output(model)}</span><span>${cost(model)}</span><span>${number(model.requests)}</span></summary><div class="usage-table"><table><thead><tr><th>${tr("services.service_account")}</th><th>${tr("services.input")}</th><th>${tr("services.output")}</th><th>${tr("common.usd")}</th><th>${tr("services.calls")}</th></tr></thead><tbody>${details}</tbody></table></div></details>`;
      }).join('');
      return `<section class="usage-service-group" data-usage-service-type="${group.id}"><h3>${group.label}</h3><div class="usage-table"><div class="usage-models"><div class="usage-model-columns usage-model-heading"><span>${tr("reports.model")}</span><span>${tr("services.input")}</span><span>${tr("services.output")}</span><span>${tr("common.usd")}</span><span>${tr("services.calls")}</span></div>${rows}</div></div></section>`;
    }).join('');
  }
  return {render, renderGroups};
}
