// Service-owned lifetime summaries and service-type/model usage breakdowns.
function createServiceUsage({esc, tr, number, money, dateTime}) {
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
    const {models, totals} = aggregate(data.groups);
    const rows = models.sort((a, b) => a.model.localeCompare(b.model)).map(row =>
      `<tr><td>${esc(row.model)}</td><td>${row.input_reports ? number(row.input_tokens) : '—'}</td><td>${row.output_reports ? number(row.output_tokens) : '—'}</td><td>${cost(row)}</td></tr>`).join('');
    return `<h3>${tr('累计用量','Lifetime usage')}</h3>
      <div class="usage-totals"><div class="usage-total"><span>Tokens</span><strong>${totals.input_reports || totals.output_reports ? number(totals.input_tokens + totals.output_tokens) : '—'}</strong></div><div class="usage-total"><span>${tr('API 等价估算 · USD','API equivalent · USD')}</span><strong>${cost(totals)}</strong></div></div>
      <p class="muted">${number(totals.priced_requests)} / ${number(totals.requests)} ${tr('次请求已计价','requests priced')}${data.first_record_at ? ' · ' + tr('开始记录 ','Recorded since ') + esc(dateTime(data.first_record_at)) : ''}</p>
      ${totals.priced_requests < totals.requests ? `<p class="muted">${tr('部分模型未定价，估算仅包含已知价格的用量。','Some model prices are unavailable; the estimate includes known prices only.')}</p>` : ''}
      <div class="usage-table"><table><thead><tr><th>${tr('模型','Model')}</th><th>${tr('输入','Input')}</th><th>${tr('输出','Output')}</th><th>USD</th></tr></thead><tbody>${rows || `<tr><td colspan="4">${tr('暂无用量记录','No usage recorded')}</td></tr>`}</tbody></table></div>`;
  }
  function renderGroups(data, providers, describe) {
    const services = new Map((providers || []).map(provider => [provider.id, provider]));
    const types = [
      {id:'codex', label:tr('Codex 订阅','Codex subscriptions'), rows:[]},
      {id:'claude', label:tr('Claude Code / CPA','Claude Code / CPA'), rows:[]},
      {id:'api', label:'API', rows:[]},
      {id:'other', label:tr('其他记录','Other records'), rows:[]},
    ];
    for (const row of data.groups || []) {
      const provider = services.get(row.owner);
      const type = ['native','subscription'].includes(row.category) ? 'codex'
        : row.category === 'external' && provider ? (provider.execution_backend === 'claude_cli' ? 'claude' : 'api') : 'other';
      types.find(group => group.id === type).rows.push(row);
    }
    const input = row => `${row.input_reports ? number(row.input_tokens) : '—'}<small>${tr('缓存 ','Cached ')}${row.cache_reports ? number(row.cached_input_tokens) : '—'}${row.cache_write_tokens ? ' · '+tr('写入 ','Write ')+number(row.cache_write_tokens) : ''}</small>`;
    const output = row => `${row.output_reports ? number(row.output_tokens) : '—'}<small>${tr('推理 ','Reasoning ')}${row.reasoning_reports ? number(row.reasoning_tokens) : '—'}</small>`;
    return types.filter(group => group.rows.length).map(group => {
      const {models} = aggregate(group.rows);
      const rows = models.sort((a,b) => (b.cost_nanos-a.cost_nanos)||(b.input_tokens-a.input_tokens)||a.model.localeCompare(b.model)).map(model => {
        const details = model.details.slice().sort((a,b) => (b.cost_nanos-a.cost_nanos)||(b.input_tokens-a.input_tokens)).map(row => `<tr><td>${esc(describe(row))}<small>${row.service_tier === 'default' ? tr('普通','Standard') : esc(row.service_tier || '')}</small></td><td>${input(row)}</td><td>${output(row)}</td><td>${cost(row)}</td><td>${number(row.requests || 0)}</td></tr>`).join('');
        return `<details class="usage-model"><summary class="usage-model-columns"><span class="usage-model-name">${esc(model.model)}</span><span>${input(model)}</span><span>${output(model)}</span><span>${cost(model)}</span><span>${number(model.requests)}</span></summary><div class="usage-table"><table><thead><tr><th>${tr('服务 / 账号','Service / account')}</th><th>${tr('输入','Input')}</th><th>${tr('输出','Output')}</th><th>USD</th><th>${tr('请求','Calls')}</th></tr></thead><tbody>${details}</tbody></table></div></details>`;
      }).join('');
      return `<section class="usage-service-group" data-usage-service-type="${group.id}"><h3>${group.label}</h3><div class="usage-table"><div class="usage-models"><div class="usage-model-columns usage-model-heading"><span>${tr('模型','Model')}</span><span>${tr('输入','Input')}</span><span>${tr('输出','Output')}</span><span>USD</span><span>${tr('请求','Calls')}</span></div>${rows}</div></div></section>`;
    }).join('');
  }
  return {render, renderGroups};
}
