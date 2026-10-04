// Feature state is private; the page supplies current data and UI operations.
function createDiagnostics({api, getLanguage, openModal, $, esc, t, tr, localTimeText, notice}) {
  let diagnosticsTimer = null;
  let diagnosticsRequest = 0;
  let supportReportRequest = 0;
  function diagnosticsTransportLabel(value) { const labels = getLanguage() === 'en' ? {http:'HTTP',sse:'SSE',websocket:'WebSocket'} : {http:'普通请求',sse:'流式请求',websocket:'WebSocket'}; return labels[value] || tr('请求','request'); }
  function diagnosticsProtocolLabel(value) { return ({responses:'Responses',chat_completions:'Chat Completions',anthropic_messages:'Anthropic Messages',unknown:tr('未知协议','Unknown protocol')}[value] || tr('未知协议','Unknown protocol')); }
  function diagnosticsContextLabel(record) { const labels = getLanguage() === 'en' ? {allowed:'Context OK',warned:'Context warning',blocked:'Blocked: context too large',unknown:'Context unknown'} : {allowed:'上下文正常',warned:'上下文需关注',blocked:'已拦截：上下文过大',unknown:'上下文未知'}; const decision = labels[record.context_decision] || labels.unknown; const estimate = record.estimated_tokens == null ? tr('估算未知','Estimate unknown') : `${tr('估算','Estimate')} ${Number(record.estimated_tokens).toLocaleString()} tokens`; const limit = record.safe_input_limit == null ? tr('安全上限未知','Safe limit unknown') : `${tr('安全上限','Safe limit')} ${Number(record.safe_input_limit).toLocaleString()}`; return `${decision} · ${estimate} · ${limit}`; }
  function diagnosticsErrorLabel(value) {
    const labels = {
      auth:['登录或密钥失效','Sign-in or key rejected'], payment_required:['余额或计费受限','Billing unavailable'],
      rate_limit:['上游限流','Rate limited'], upstream_5xx:['上游 5xx','Upstream 5xx'], upstream_504:['上游超时','Upstream timeout'],
      proxy_unavailable:['代理不可用','Proxy unavailable'], proxy_reset:['代理连接中断','Proxy connection reset'],
      dns_failure:['域名解析失败','DNS failure'], tls_failure:['安全连接失败','TLS failure'], network:['网络不可用','Network unavailable'],
      stream_error:['流异常','Stream error'], stream_incomplete:['输出未正常完成','Incomplete stream'], malformed_terminal:['终止事件异常','Invalid terminal event'],
      upstream_close_pre_output:['输出前断线','Disconnected before output'], upstream_close_after_output:['输出后断线','Disconnected after output'], upstream_close_after_tool:['工具调用后断线','Disconnected after tool call'],
      connect_timeout:['连接超时','Connection timeout'], first_event_timeout:['等待上游响应超时','First response timeout'], first_output_timeout:['首个输出超时','First output timeout'],
      idle_after_output:['输出中断后超时','Output stalled'], local_deadline:['请求等待超时','Request deadline reached'], timeout:['超时','Timeout'],
      upstream_capacity:['本地排队超限','Local queue limit'], protocol_rejection:['上游不接受请求格式','Request format rejected'],
      context_length_exceeded:['上下文过长','Context too long'], history_reconstruction_failed:['历史恢复失败','History recovery failed'], external_compaction_failed:['上下文压缩失败','Context compaction failed'],
      output_limit:['达到输出上限','Output limit reached'], content_filter:['内容被上游拦截','Content blocked upstream'], router_error:['转发失败','Routing failed'],
    };
    return (labels[value] || ['其他错误','Other error'])[getLanguage() === 'en' ? 1 : 0];
  }
  function performanceNumber(value) { if (value === null || value === undefined || value === '') return null; const number = Number(value); return Number.isFinite(number) && number >= 0 ? number : null; }
  function performanceMode(value) { return value === 'fast' ? 'Fast' : (value === 'standard' ? tr('普通','Standard') : '—'); }
  function performanceRate(value) { const number = performanceNumber(value); return number == null ? '—' : number.toFixed(1) + '%'; }
  function performanceTrend(value, kind) { const change = performanceNumber(Math.abs(Number(value))); if (value === null || value === undefined || change == null || Number(value) === 0) return ''; const improved = Number(value) > 0; const arrow = kind === 'ttft' ? (improved ? '↓' : '↑') : (improved ? '↑' : '↓'); const title = kind === 'ttft' ? tr('与此前样本相比的首字延迟变化','First-token latency change from the previous window') : tr('与此前样本相比的生成速度变化','Generation speed change from the previous window'); return `<span class="metric-trend ${improved ? 'good' : 'bad'}" title="${esc(title)}">${arrow}${change.toFixed(1)}%</span>`; }
  function renderDiagnostics(payload) {
    const records = Array.isArray(payload.records) ? payload.records : [];
    const models = Array.isArray(payload.models) ? payload.models : [];
    const health = payload.health && typeof payload.health === 'object' ? payload.health : {};
    const healthSamples = performanceNumber(health.sample_count) || 0;
    $('diagnostics_summary').textContent = healthSamples ? tr(`最近 ${healthSamples} 次请求的运行情况`,`Health across the latest ${healthSamples} requests`) : tr('完成几次模型调用后，这里会显示汇总结果。','Aggregates appear here after a few model calls.');
    const fallbackAttempts = performanceNumber(health.fallback_attempt_count) || 0;
    if (fallbackAttempts) $('diagnostics_summary').textContent += tr(` · 另有 ${fallbackAttempts} 次连接回退尝试，未重复计入请求`,` · ${fallbackAttempts} transport fallback attempts counted separately`);
    $('health_summary').innerHTML = healthSamples ? `<div class="health-grid"><div class="health-stat"><span>${tr('成功率','Success rate')}</span><strong>${performanceRate(health.success_rate)}</strong><small>${Number(health.success_count || 0).toLocaleString()} / ${Number(health.sample_count || 0).toLocaleString()}</small></div><div class="health-stat"><span>429</span><strong>${performanceRate(health.status_429_rate)}</strong><small>${Number(health.status_429_count || 0).toLocaleString()} ${tr('次','calls')}</small></div><div class="health-stat"><span>502</span><strong>${performanceRate(health.status_502_rate)}</strong><small>${Number(health.status_502_count || 0).toLocaleString()} ${tr('次','calls')}</small></div><div class="health-stat"><span>503</span><strong>${performanceRate(health.status_503_rate)}</strong><small>${Number(health.status_503_count || 0).toLocaleString()} ${tr('次','calls')}</small></div><div class="health-stat"><span>504</span><strong>${performanceRate(health.status_504_rate)}</strong><small>${Number(health.status_504_count || 0).toLocaleString()} ${tr('次','calls')}</small></div><div class="health-stat"><span>${tr('本地排队超限','Local queue limit')}</span><strong>${performanceRate(health.local_capacity_rate)}</strong><small>${Number(health.local_capacity_count || 0).toLocaleString()} ${tr('次','calls')}</small></div></div>` : '';
    const showSpeedMode = models.some(model => model.speed_mode === 'fast');
    const windowCalls = Number(payload.performance_window?.calls) || 20;
    $('performance_records').innerHTML = models.length ? `<div class="performance-table"><table><tr><th>${tr('模型','Model')}</th>${showSpeedMode ? `<th>${tr('模式','Mode')}</th>` : ''}<th>${tr('调用','Calls')}</th><th>TTFT</th><th>TPS</th></tr>${models.map(model => { const ttft = performanceNumber(model.ttft_ms); const tps = performanceNumber(model.tokens_per_second); return `<tr><td><code>${esc(model.model_id)}</code></td>${showSpeedMode ? `<td>${esc(performanceMode(model.speed_mode))}</td>` : ''}<td>${Number(model.call_count || 0).toLocaleString()}</td><td>${ttft == null ? '—' : (ttft / 1000).toFixed(2) + ' s'} <span class="muted">(${Number(model.ttft_samples || 0)})</span>${performanceTrend(model.ttft_change_percent,'ttft')}</td><td>${tps == null ? '—' : tps.toFixed(1) + ' token/s'} <span class="muted">(${Number(model.tps_samples || 0)})</span>${performanceTrend(model.tps_change_percent,'tps')}</td></tr>`; }).join('')}</table><div class="muted">${tr(`最近 ${windowCalls} 次有效调用的中位数；箭头与此前 ${windowCalls} 次比较。`,`Median of the latest ${windowCalls} valid calls; arrows compare with the preceding ${windowCalls}.`)}</div></div>` : '';
    const failures = records.slice().reverse().filter(record => !['client_disconnect','client_cancelled','client_websocket_close'].includes(record.error_class) && (Number(record.status) >= 400 || (record.error_class && record.error_class !== 'none'))).slice(0, 20);
    $('diagnostics_records').innerHTML = failures.length ? failures.map(diagnosticFailureHtml).join('') : `<p class="muted">${tr('近期没有失败记录。','No recent failures.')}</p>`;
    renderCacheUsage(payload);
  }
  function renderCacheUsage(payload) {
    const target = $('cache_records'); if (!target) return;
    const opened = new Set([...target.querySelectorAll('details[open]')].map(item => item.dataset.cacheKey));
    const models = Array.isArray(payload.cache?.models) ? payload.cache.models : [];
    const rate = value => value === null || value === undefined ? tr('未提供','Not reported') : performanceRate(value);
    const coverage = item => `${Number(item.sample_count || 0)} / ${Number(item.call_count || 0)}`;
    const hits = item => item.sample_count ? `${Number(item.hit_count || 0)} / ${Number(item.sample_count)}` : '—';
    const timeText = seconds => new Date(seconds * 1000).toLocaleTimeString(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {hour:'2-digit',minute:'2-digit',hour12:false});
    const rows = models.map(model => {
      const key = JSON.stringify([model.model_id, model.provider_id, model.speed_mode, model.endpoint_fingerprint]);
      const mode = model.speed_mode === 'fast' ? ' · Fast' : model.speed_mode === 'standard' ? tr(' · 普通',' · Standard') : '';
      const source = model.provider_id || tr('当前 Codex 登录','Current Codex login');
      const periods = (Array.isArray(model.periods) ? model.periods : []).map(period => {
        const day = new Date(period.start * 1000).toLocaleDateString(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {month:'numeric',day:'numeric'});
        const ongoing = period.complete ? '' : `<span class="muted">${tr('统计中','In progress')}</span>`;
        return `<tr><td>${esc(day)} ${esc(timeText(period.start))}–${esc(timeText(period.end))} ${ongoing}</td>
          <td>${rate(period.rate)}</td><td>${hits(period)}</td><td>${coverage(period)}</td></tr>`;
      }).join('');
      const tokens = model.sample_count ? `${Number(model.cached_input_tokens).toLocaleString()} / ${Number(model.input_tokens).toLocaleString()} token` : '—';
      return `<details class="cache-model" data-cache-key="${esc(key)}"${opened.has(key) ? ' open' : ''}>
        <summary><span>${esc(model.model_id)} · ${esc(source)}${esc(mode)}</span><strong>${rate(model.rate)}</strong></summary>
        <p class="muted">${tr('有效记录','Reported usage')} ${coverage(model)} · ${tr('命中请求','Requests with cache hits')} ${hits(model)} · ${tr('缓存 / 输入','Cached / input')} ${tokens}</p>
        <div class="cache-periods"><table><thead><tr><th>${tr('时段','Period')}</th><th>${tr('缓存 token 比例','Cached token rate')}</th><th>${tr('命中请求','Cache-hit requests')}</th><th>${tr('有效记录','Reported usage')}</th></tr></thead><tbody>${periods}</tbody></table></div>
      </details>`;
    }).join('');
    target.innerHTML = `<section class="cache-performance"><h3>${tr('缓存命中率','Prompt cache hit rate')}</h3>
      <p class="muted">${tr('缓存 token 总数 ÷ 输入 token 总数；每 10 分钟汇总，空闲时段不显示。','Cached input tokens ÷ total input tokens; grouped by 10 minutes, with idle periods omitted.')}</p>
      ${rows || `<p class="muted">${tr('还没有模型调用记录。','No model calls recorded yet.')}</p>`}
      <p class="muted">${tr(`最近 7 天，最多 ${Number(payload.capacity) || 512} 次已保留请求。缺少缓存用量的请求不计入比例。`,`Retained requests from the last 7 days, up to ${Number(payload.capacity) || 512} calls. Requests without cache usage are excluded from rates.`)} ${tr('仅反映上游报告的命中情况，不代表与原生调用的差异。','Reports upstream cache usage, not a comparison with direct calls.')}</p></section>`;
  }
  function diagnosticFailureHtml(record) {
    const status = Number(record.status);
    const attempt = record.recovery_mode === 'native_http_fallback';
    const result = `${attempt ? tr('回退前尝试 · ','Pre-fallback attempt · ') : ''}${status >= 400 && status <= 599 ? status + ' · ' : ''}${diagnosticsErrorLabel(record.error_class)}`;
    const source = record.provider_id || (record.dialect === 'codex_native' ? tr('当前 Codex 登录','Current Codex login') : tr('未记录','Not recorded'));
    const fields = [
      [tr('账号 / 服务','Account / provider'), source],
      [tr('调用模型','Requested model'), record.model_id || tr('未记录','Not recorded')],
      [tr('连接方式','Connection'), `${diagnosticsProtocolLabel(record.protocol)} · ${diagnosticsTransportLabel(record.transport)}`],
      [tr('耗时','Duration'), `${Number(record.duration_ms || 0).toLocaleString()} ms`],
      [tr('输出进度','Output progress'), record.tool_activity ? tr('已收到工具调用','Tool call received') : record.output_emitted ? tr('已收到部分输出','Partial output received') : tr('未记录到输出','No output recorded')],
      [tr('自动恢复','Automatic recovery'), record.recovery_succeeded ? tr('恢复成功','Recovered') : record.fallback ? tr('已尝试切换通道','Another transport was tried') : tr('未记录到成功恢复','No successful recovery recorded')],
      [tr('诊断编号','Diagnostic ID'), record.request_id || record.observation_id || tr('未记录','Not recorded')],
    ];
    if (record.context_decision === 'blocked' || record.context_decision === 'warned') fields.push([tr('上下文','Context'), diagnosticsContextLabel(record)]);
    const advice = diagnosticFailureAdvice(record);
    return `<details class="request-failure"><summary><span>${esc(record.model_id || tr('未知模型','Unknown model'))}</span><span>${esc(result)}</span><time class="muted">${esc(record.observed_at ? localTimeText(Date.parse(record.observed_at)) : '')}</time></summary><dl>${fields.map(([label,value]) => `<dt>${esc(label)}</dt><dd>${esc(value)}</dd>`).join('')}</dl><p class="muted">${esc(advice)}</p></details>`;
  }
  function diagnosticFailureAdvice(record) {
    if (record.tool_activity || record.output_emitted) return tr('中断前已有输出，请先检查任务进度，再决定是否重试。','Output arrived before interruption. Check the task before retrying.');
    if (['auth','payment_required'].includes(record.error_class)) return tr('检查所选账号的登录状态或服务密钥、余额。','Check the selected account sign-in, provider key or balance.');
    if (record.error_class === 'rate_limit') return tr('上游正在限流，请查看账号余量和重置倒计时。','The upstream is rate limiting requests. Check quota and the reset countdown.');
    if (['proxy_unavailable','proxy_reset','network','dns_failure','tls_failure','connect_timeout'].includes(record.error_class)) return tr('检查系统代理、网络和证书设置；这类错误不代表账号额度不足。','Check proxy, network and certificate settings. This error does not indicate exhausted quota.');
    if (['context_length_exceeded','history_reconstruction_failed','external_compaction_failed'].includes(record.error_class)) return tr('请求的上下文或历史未能处理，请保留诊断编号以便排查。','The context or history could not be processed. Keep the diagnostic ID for investigation.');
    return tr('可用诊断编号定位本地日志。没有收到输出，也不能确认上游未执行。','Use the diagnostic ID to locate local logs. Missing output does not prove the upstream did not execute.');
  }
  function stopDiagnostics() { diagnosticsRequest++; supportReportRequest++; if (diagnosticsTimer !== null) clearInterval(diagnosticsTimer); diagnosticsTimer = null; }
  function supportStatusLabel(value) {
    const labels = getLanguage() === 'en' ? {
      auth_required:'Sign-in required', transport_error:'Network or proxy error', rate_limited:'Rate limited',
      unclassified_error:'Other error', credential_missing:'No credential', not_checked:'Not checked', success:'Last check succeeded'
    } : {
      auth_required:'需要重新登录', transport_error:'网络或代理故障', rate_limited:'查询受限',
      unclassified_error:'其他错误', credential_missing:'没有凭据', not_checked:'尚未检查', success:'上次检查成功'
    };
    return labels[value] || tr('未知','Unknown');
  }
  function supportDetailLabel(value) {
    const labels = getLanguage() === 'en' ? {
      allowed:'Likely writable', denied:'Write permission denied', unknown:'Unknown',
      environment:'Environment proxy', system:'System proxy', direct:'Direct', proxy:'Through proxy',
      catalog_loaded:'EMP model catalog observed', emp_catalog_absent:'EMP model IDs absent', nvm:'Codex CLI managed by nvm', path_cli:'Codex CLI on PATH', codex_app:'ChatGPT App (Codex)'
    } : {
      allowed:'可能可写', denied:'没有写入权限', unknown:'未知',
      environment:'环境变量代理', system:'系统代理', direct:'直连', proxy:'经过代理',
      catalog_loaded:'已观察 EMP 模型目录', emp_catalog_absent:'EMP 模型 ID 已不可见', nvm:'由 nvm 管理的 Codex CLI', path_cli:'PATH 中的 Codex CLI', codex_app:'ChatGPT App（Codex）'
    };
    return labels[value] || esc(value || 'unknown');
  }
  function runtimeLabel(value) {
    return {
      catalog_loaded:tr('模型目录符合目标；请求路线未确认','model catalog matches target; request route unverified'),
      emp_catalog_absent:tr('此前的 EMP 模型 ID 已不可见；恢复未确认','previous EMP model IDs are absent; restoration unverified'),
      stopped_waiting_for_start:tr('控制接口不可用；目录尚未验证','control interface unavailable; catalog unverified'),
      reload_required:tr('共享模型目录仍旧；当前对话结束后再请后端所有者正常重启','shared catalog is stale; ask its owner to restart normally after active chats finish'),
      catalog_unverified:tr('共享模型目录尚未验证','shared model catalog not yet verified'),
      verification_failed:tr('无法读取共享模型目录','shared model catalog could not be read'),
      not_checked:tr('尚未观察共享模型目录','shared model catalog has not been observed')
    }[value] || tr('共享模型目录状态未知','shared model catalog state unknown');
  }
  function renderSupportReport(report) {
    const box = $('support_report'); if (!box) return;
    const config = report.configuration || {}, codex = report.codex || {}, network = report.network || {}, accounts = report.accounts || {};
    const imported = Array.isArray(accounts.imported) ? accounts.imported : [];
    const accountLines = imported.map(account => `${tr('导入账号','Imported account')} ${Number(account.index) || 0}: ${supportStatusLabel(account.quota_status)}`);
    const configNotes = [!config.exists && tr('文件不存在','file missing'), config.write_access_hint === 'denied' && tr('没有写入权限','not writable')].filter(Boolean);
    const compatibilityNote = {
      available:tr('可用', 'available'),
      unknown:tr('无法确认引擎版本', 'engine version could not be observed')
    }[codex.compatibility];
    const versionFallback = { unavailable:tr('未找到可用引擎','no engine available') }[codex.compatibility];
    const versionLabel = codex.version || versionFallback || tr('版本未知','version unknown');
    const codexNotes = [codex.host_version && `${tr('应用版本','App version')} ${codex.host_version}`, compatibilityNote, codex.source && supportDetailLabel(codex.source), codex.target && runtimeLabel(codex.state,codex.target)].filter(Boolean);
    box.innerHTML = `<p><strong>${tr('配置','Config')}</strong> ${esc(config.path || '—')}${configNotes.map(note => ' · ' + esc(note)).join('')}</p>
      <p><strong>Codex</strong> ${esc(versionLabel)}${codexNotes.map(note => ' · ' + esc(note)).join('')}</p>
      <p><strong>${tr('代理','Proxy')}</strong> ${supportDetailLabel(network.source_at_startup)} · ${supportDetailLabel(network.chatgpt_route)}${network.proxy_scheme ? ' (' + esc(network.proxy_scheme) + ')' : ''}</p>
      <p><strong>${tr('账号','Accounts')}</strong> Native: ${supportStatusLabel(accounts.native?.quota_status)}${accountLines.length ? ' · ' + accountLines.map(esc).join(' · ') : ''}</p>`;
  }
  async function loadSupportReport() {
    const request = ++supportReportRequest;
    try {
      const report = await api('/api/support-report');
      if (request === supportReportRequest) renderSupportReport(report);
    } catch (_) {
      if (request === supportReportRequest && $('support_report')) $('support_report').textContent = tr('诊断报告暂时不可用。','Support report is unavailable.');
    }
  }
  async function downloadSupportReport() {
    try {
      const report = await api('/api/support-report');
      const blob = new Blob([JSON.stringify(report, null, 2) + '\n'], {type:'application/json'});
      const url = URL.createObjectURL(blob), link = document.createElement('a');
      link.href = url; link.download = 'EMP-support-report.json'; document.body.appendChild(link); link.click(); link.remove(); URL.revokeObjectURL(url);
      notice(tr('诊断报告已下载','Support report downloaded'));
    } catch (_) { notice(tr('诊断报告下载失败','Could not download support report'), true); }
  }
  async function loadDiagnostics() {
    const request = ++diagnosticsRequest;
    try {
      const payload = await api('/api/diagnostics');
      if (request === diagnosticsRequest && $('diagnostics_summary')) renderDiagnostics(payload);
      return true;
    } catch (error) {
      if (request === diagnosticsRequest && $('diagnostics_summary')) $('diagnostics_summary').textContent = tr('运行状态暂时不可用。','Diagnostics are unavailable.');
      return false;
    }
  }
  function openDiagnostics() { openModal(tr('性能与健康','Performance and health'), `<div class="toolbar"><button type="button" class="secondary" data-icon="refresh" onclick="loadDiagnostics(); loadSupportReport()">${esc(t('refresh'))}</button><button type="button" class="secondary" data-icon="download" onclick="downloadSupportReport()">${tr('下载脱敏报告','Download redacted report')}</button></div><details class="diagnostics-details" open><summary>${tr('系统诊断','System diagnostics')}</summary><div id="support_report" class="performance-help">${tr('正在读取诊断报告…','Loading support report…')}</div></details><div class="performance-help"><span><strong>TTFT</strong> · ${tr('请求进入 EMP 后，到收到首段正文或工具参数的时间。','Time from EMP receiving the request to the first text or tool-argument output.')}</span><span><strong>TPS</strong> · ${tr('上游回报的全部输出 token 除以完整请求耗时，包含首 token 等待与隐藏推理时间。','All upstream-reported output tokens divided by the complete request duration, including TTFT and hidden reasoning time.')}</span></div><div id="diagnostics_summary" class="muted"></div><div id="health_summary"></div><div id="performance_records"></div><div id="cache_records"></div><details class="failure-list"><summary>${tr('近期失败详情','Recent failure details')}</summary><div id="diagnostics_records"></div></details>`, '', null); loadDiagnostics(); loadSupportReport(); diagnosticsTimer = setInterval(() => { if (document.visibilityState !== 'hidden') { loadDiagnostics(); loadSupportReport(); } }, 10 * 60 * 1000); }

  return {open:openDiagnostics, stop:stopDiagnostics, load:loadDiagnostics, loadSupport:loadSupportReport, downloadSupport:downloadSupportReport};
}
