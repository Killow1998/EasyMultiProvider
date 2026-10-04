// Feature state is private; the page supplies current data and UI operations.
function createRequestDetails({getState, getActivity, getLanguage, openModal, $, esc, tr, utcOffsetLabel, protocolLabel}) {
  // Request details consume the existing management activity events.
  let activityDetailsSelection = null;

  function requestMatchesSelection(request, selection) {
    const {kind, id, modelIds} = selection;
    if (kind === 'provider') return request.provider_id === id;
    if (kind === 'account') return request.account_id === id;
    if (kind === 'models') return modelIds.includes(request.model_id);
    return request.model_id === id;
  }
  function requestReasonLabel(reason) {
    const labels = {
      network:tr('连接中断','Connection interrupted'), timeout:tr('请求超时','Request timed out'),
      connect_timeout:tr('连接超时','Connection timed out'), rate_limit:tr('请求限流','Rate limited'),
      rate_limited:tr('请求限流','Rate limited'), account_refresh:tr('刷新账号登录','Account sign-in refreshed'),
      reasoning_fallback:tr('调整思考强度','Reasoning effort adjusted'), protocol_rejection:tr('切换接口协议','Protocol changed'),
      context_length:tr('超出上下文','Context limit exceeded'), context_length_exceeded:tr('超出上下文','Context limit exceeded'),
      stream_incomplete:tr('回复中断','Reply interrupted'), stream_error:tr('回复失败','Reply failed'),
      output_limit:tr('达到输出上限','Output limit reached'), auth:tr('登录验证失败','Authentication failed'),
      client_disconnect:tr('客户端已断开','Client disconnected'), local_deadline:tr('请求超时','Request timed out'),
      content_filter:tr('内容过滤','Content filtered'), quota_exhausted:tr('额度用尽','Quota exhausted'),
      auth_rejected:tr('登录验证失败','Authentication failed'), upstream_http:tr('上游请求失败','Upstream request failed'),
    };
    return labels[reason] || tr('请求失败','Request failed');
  }
  function requestTargetName(record) {
    if (record.account_id === '@native') return 'Native';
    if (record.account_id) return (getState()?.accounts || []).find(account => account.id === record.account_id)?.name || record.account_id;
    return (getState()?.providers || []).find(provider => provider.id === record.provider_id)?.name || record.provider_id || '';
  }
  function requestTimeHtml(seconds, full = false) {
    if (!Number.isFinite(seconds)) return '';
    const date = new Date(seconds * 1000);
    const time = date.toLocaleTimeString(getLanguage() === 'en' ? 'en-US' : 'zh-CN',{hour12:false});
    const pad = value => String(value).padStart(2,'0');
    const text = full ? `${date.getFullYear()}-${pad(date.getMonth()+1)}-${pad(date.getDate())} ${time} (${utcOffsetLabel(date)})` : time;
    return `<time datetime="${esc(date.toISOString())}">${esc(text)}</time>`;
  }
  function requestReceiptHtml(record) {
    const labels = {active:tr('正在响应','Responding'),completed:tr('已完成','Completed'),failed:tr('失败','Failed'),cancelled:tr('已取消','Cancelled'),interrupted:tr('已中断','Interrupted'),disconnected:tr('连接已断开','Connection closed')};
    const status = labels[record.state] || labels.interrupted;
    const selected = record.client_model || record.model_id;
    const selectedName = (getState()?.models || []).find(model => model.id === record.model_id)?.display_name || selected;
    const attempts = Number.isSafeInteger(record.attempts) ? record.attempts : 0;
    const duration = Number.isFinite(record.duration_ms) ? `<span>${tr('耗时','Duration')} <strong>${(record.duration_ms / 1000).toFixed(1)}s</strong></span>` : '';
    const returned = record.response_model && record.response_model !== record.upstream_model
      ? `<div class="request-returned">${tr('返回模型','Returned model')} <code>${esc(record.response_model)}</code></div>` : '';
    const error = record.state === 'failed' ? `<p class="request-error">${esc(requestReasonLabel(record.failure_reason || record.error_class))}${record.http_status ? ` · ${esc(record.http_status)}` : ''}</p>` : '';
    const retries = record.retries || [];
    const retrySteps = retries.map((retry,index) => {
      const stage = attempts > index + 1 ? tr(`重试 ${index + 1}`,`Retry ${index + 1}`) : tr('准备重试','Retry scheduled');
      return `<li><span>${stage} · ${esc(requestReasonLabel(retry.reason))}${retry.status ? ` · ${esc(retry.status)}` : ''}</span>${requestTimeHtml(retry.at)}</li>`;
    }).join('');
    const timeline = retries.length ? `<section class="request-retries"><h3>${tr('重试过程','Retry history')}</h3><ol class="request-timeline"><li><span>${tr('发起请求','Request started')}</span>${requestTimeHtml(record.started_at)}</li>${retrySteps}<li><span>${esc(status)}</span>${requestTimeHtml(record.finished_at)}</li></ol></section>` : '';
    return `<article class="request-receipt"><div class="request-receipt-head">${requestTimeHtml(record.started_at, true)}<span class="request-state" data-state="${esc(record.state)}">${esc(status)}</span></div><div class="request-route"><div class="request-route-node"><small>${tr('所选模型','Selected model')}</small><strong>${esc(selectedName)}</strong>${selectedName !== selected ? `<code>${esc(selected)}</code>` : ''}</div><span class="request-route-arrow" aria-hidden="true">→</span><div class="request-route-node"><small>${tr('实际目标','Target')}</small><strong>${esc(requestTargetName(record))}</strong><code>${esc(record.upstream_model || '')}</code></div></div>${returned}<div class="request-summary">${duration}<span><strong>${attempts}</strong> ${tr('次发送','attempts')}</span><span>${esc(protocolLabel(record.resolved_protocol))}</span><span>${esc(String(record.transport || '').toUpperCase())}</span></div>${error}${timeline}</article>`;
  }
  function activityDetailsHtml(selection) {
    const records = (getActivity()?.requests || []).filter(record => requestMatchesSelection(record, selection));
    return records.length ? records.map(requestReceiptHtml).join('') : `<p class="muted">${tr('暂无请求记录','No requests recorded')}</p>`;
  }

  function openActivityDetails(element) {
    let modelIds = [];
    try { modelIds = JSON.parse(element.dataset.activityModels || '[]'); } catch (_) {}
    const selection = {kind:element.dataset.activityKind,id:element.dataset.activityId || '',modelIds,label:element.dataset.activityLabel || ''};
    openModal(tr('调用详情','Request details'), `<div id="request_details">${activityDetailsHtml(selection)}</div>`, '', null);
    activityDetailsSelection = selection;
  }
  function refreshActivityDetails() {
    const container = $('request_details');
    if (container && activityDetailsSelection) container.innerHTML = activityDetailsHtml(activityDetailsSelection);
  }
  function normalizeRequestReceipts(value) {
    if (!Array.isArray(value)) return [];
    return value.slice(0,128).filter(record => record && typeof record.model_id === 'string' && typeof record.request_id === 'string')
      .map(record => ({...record,retries:Array.isArray(record.retries) ? record.retries.slice(0,16) : []}));
  }

  return {open:openActivityDetails, refresh:refreshActivityDetails, reset:() => { activityDetailsSelection = null; }, normalize:normalizeRequestReceipts};
}
