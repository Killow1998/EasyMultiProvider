// Request outcome formatting and dialog lifecycle shared by report views.
function createCallOutcomes({esc,tr,number,time}) {
  const n=value=>typeof value === 'number' && Number.isFinite(value) && value>=0 ? value : null;
  function outcomes(summary) {
    const known = Object.prototype.hasOwnProperty.call(summary,'success_samples');
    const samples = known ? (summary.success_samples || 0) : summary.calls;
    const label = known ? tr('请求成功率','Request success rate') : tr('完成比例','Completion ratio');
    const value = samples > 0 && n(summary.completed) !== null ? (summary.completed/samples*100).toFixed(1)+'%' : '—';
    const details = known ? [
      `${number(summary.completed)} ${tr('完成','completed')}`, `${number(summary.failed ?? 0)} ${tr('失败','failed')}`,
      `${number(summary.interrupted ?? 0)} ${tr('中断','interrupted')}`, `${number(summary.cancelled ?? 0)} ${tr('断开','disconnected')}`,
      `${number(summary.recovery_required ?? 0)} ${tr('重发历史','history resends')}`, `${number(summary.unknown ?? 0)} ${tr('未记录结果','unrecorded outcomes')}`,
    ].join(' · ') : `${number(summary.calls-summary.completed)} ${tr('未完成','not completed')}`;
    const hint = known ? `${number(summary.completed)} ${tr('完成','completed')} · ${number(summary.failed ?? 0)} ${tr('失败','failed')}` : details;
    const failures = summary.failure_breakdown || [];
    const reasons = failures.map(row => `<tr><td>${esc(errorSource(row.origin))}</td><td>${esc(errorReason(row.code))}${row.status ? ' · HTTP '+esc(row.status) : ''}</td><td>${number(row.count)}</td></tr>`).join('');
    const remaining = (summary.failed || 0)-failures.reduce((total,row)=>total+row.count,0);
    const detailsHtml = `<strong>${esc(label)} · ${esc(value)}</strong><p>${known ? tr('完成 ÷（完成＋失败）。中断、断开和历史重发不计入失败。','Completed ÷ (completed + failed). Interrupts, disconnects and history resends are excluded.') : tr('当前后端记录的完成数 ÷ 全部请求数。','Completed records ÷ all requests reported by the current backend.')}</p><p>${esc(details)}</p>${reasons ? `<table><thead><tr><th>${tr('错误来源','Error source')}</th><th>${tr('原因','Reason')}</th><th>${tr('次数','Count')}</th></tr></thead><tbody>${reasons}</tbody></table>` : ''}${remaining > 0 ? '<p>'+tr('其他失败','Other failures')+' · '+number(remaining)+'</p>' : ''}`;
    return {label,value,hint,detailsHtml,failed:known ? summary.failed : summary.calls-summary.completed};
  }
  const errorSource = value => ({emp:'EMP',upstream:tr('服务端','Upstream service'),transport:tr('连接','Connection'),client:tr('Codex 客户端','Codex client'),claude_cli:'Claude Code'})[value] || tr('来源未记录','Source not recorded');
  const errorReason = code => ({upstream_error:tr('响应传输失败','Response transport failed'),quota_exceeded:tr('额度不足','Quota exhausted'),rate_limit_exceeded:tr('请求限流','Rate limited'),rate_limit_error:tr('请求限流','Rate limited'),authentication_error:tr('认证失败','Authentication failed'),output_budget_exhausted:tr('输出预算耗尽','Output budget exhausted'),claude_cli_output_budget_exhausted:tr('输出预算耗尽','Output budget exhausted'),output_limit:tr('达到输出上限','Output limit reached'),timeout:tr('请求超时','Request timed out'),stream_error:tr('响应流失败','Response stream failed'),unknown:tr('原因未记录','Reason not recorded')})[code] || code;
  function bindOutcomes(root, summary, report = {}) {
    const anchor = root.querySelector('[data-call-outcomes]');
    if (!anchor) return () => {};
    const doc = root.ownerDocument;
    let dialog = null;
    anchor.setAttribute('role','button'); anchor.setAttribute('aria-haspopup','dialog');
    anchor.setAttribute('aria-label',outcomes(summary).label+' · '+tr('查看调用结果','View call outcomes'));
    function close() { if (dialog) { dialog.close(); dialog.remove(); dialog=null; } }
    function show() {
      if (dialog) return;
      dialog=doc.createElement('dialog'); dialog.className='call-outcome-dialog';
      const titleId='call-outcome-'+Math.random().toString(36).slice(2);
      dialog.setAttribute('aria-labelledby',titleId);
      const range = n(report.start) !== null && n(report.end) !== null ? `<p class="muted">${esc(time(report.start))} — ${esc(time(report.end))}</p>` : '';
      dialog.innerHTML=`<div class="modal-header"><h2 id="${titleId}">${tr('调用结果','Call outcomes')}</h2><button type="button" class="modal-close" aria-label="${tr('关闭','Close')}">×</button></div><div>${range}${outcomes(summary).detailsHtml}</div>`;
      doc.body.appendChild(dialog); dialog.querySelector('button').onclick=close;
      dialog.addEventListener('cancel',event=>{event.preventDefault();close();});
      dialog.addEventListener('keydown',event=>{if(event.key==='Escape')event.stopPropagation();});
      dialog.showModal();
    }
    function key(event) { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); show(); } }
    anchor.addEventListener('click',show); anchor.addEventListener('keydown',key);
    return () => { close(); anchor.removeEventListener('click',show); anchor.removeEventListener('keydown',key); };
  }
  return {outcomes,bindOutcomes,errorSource};
}
