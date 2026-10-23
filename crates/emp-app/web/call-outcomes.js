// Request outcome formatting and dialog lifecycle shared by report views.
function createCallOutcomes({esc,tr,number,time}) {
  const n=value=>typeof value === 'number' && Number.isFinite(value) && value>=0 ? value : null;
  function outcomes(summary) {
    const known = Object.prototype.hasOwnProperty.call(summary,'success_samples');
    const samples = known ? (summary.success_samples || 0) : summary.calls;
    const label = known ? tr("reports.request_success_rate") : tr("reports.completion_ratio");
    const value = samples > 0 && n(summary.completed) !== null ? (summary.completed/samples*100).toFixed(1)+'%' : '—';
    const details = known ? [
      `${number(summary.completed)} ${tr("reports.completed")}`, `${number(summary.failed ?? 0)} ${tr("reports.failed")}`,
      `${number(summary.interrupted ?? 0)} ${tr("reports.interrupted")}`, `${number(summary.cancelled ?? 0)} ${tr("reports.disconnected")}`,
      `${number(summary.recovery_required ?? 0)} ${tr("reports.history_resends")}`, `${number(summary.unknown ?? 0)} ${tr("reports.unrecorded_outcomes")}`,
    ].join(' · ') : `${number(summary.calls-summary.completed)} ${tr("reports.not_completed")}`;
    const hint = known ? `${number(summary.completed)} ${tr("reports.completed")} · ${number(summary.failed ?? 0)} ${tr("reports.failed")}` : details;
    const failures = summary.failure_breakdown || [];
    const reasons = failures.map(row => `<tr><td>${esc(errorSource(row.origin))}</td><td>${esc(errorReason(row.code))}${row.status ? ' · HTTP '+esc(row.status) : ''}</td><td>${number(row.count)}</td></tr>`).join('');
    const remaining = (summary.failed || 0)-failures.reduce((total,row)=>total+row.count,0);
    const detailsHtml = `<strong>${esc(label)} · ${esc(value)}</strong><p>${known ? tr("reports.completed_completed_failed_interrupts_disconnects_and_history_resends_are") : tr("reports.completed_records_all_requests_reported_by_the_current_backend")}</p><p>${esc(details)}</p>${reasons ? `<table><thead><tr><th>${tr("reports.error_source")}</th><th>${tr("reports.reason")}</th><th>${tr("reports.count")}</th></tr></thead><tbody>${reasons}</tbody></table>` : ''}${remaining > 0 ? '<p>'+tr("reports.other_failures")+' · '+number(remaining)+'</p>' : ''}`;
    return {label,value,hint,detailsHtml,failed:known ? summary.failed : summary.calls-summary.completed};
  }
  const errorSource = value => ({emp:'EMP',upstream:tr("reports.upstream_service"),transport:tr("reports.connection"),client:tr("reports.codex_client"),claude_cli:'Claude Code'})[value] || tr("reports.source_not_recorded");
  const errorReason = code => ({upstream_error:tr("reports.response_transport_failed"),quota_exceeded:tr("reports.quota_exhausted"),rate_limit_exceeded:tr("reports.rate_limited"),rate_limit_error:tr("reports.rate_limited"),authentication_error:tr("reports.authentication_failed"),output_budget_exhausted:tr("reports.output_budget_exhausted"),claude_cli_output_budget_exhausted:tr("reports.output_budget_exhausted"),output_limit:tr("reports.output_limit_reached"),timeout:tr("reports.request_timed_out"),stream_error:tr("reports.response_stream_failed"),unknown:tr("reports.reason_not_recorded")})[code] || code;
  function bindOutcomes(root, summary, report = {}) {
    const anchor = root.querySelector('[data-call-outcomes]');
    if (!anchor) return () => {};
    const doc = root.ownerDocument;
    let dialog = null;
    anchor.setAttribute('role','button'); anchor.setAttribute('aria-haspopup','dialog');
    anchor.setAttribute('aria-label',outcomes(summary).label+' · '+tr("reports.view_call_outcomes"));
    function close() { if (dialog) { dialog.close(); dialog.remove(); dialog=null; } }
    function show() {
      if (dialog) return;
      dialog=doc.createElement('dialog'); dialog.className='call-outcome-dialog';
      const titleId='call-outcome-'+Math.random().toString(36).slice(2);
      dialog.setAttribute('aria-labelledby',titleId);
      const range = n(report.start) !== null && n(report.end) !== null ? `<p class="muted">${esc(time(report.start))} — ${esc(time(report.end))}</p>` : '';
      dialog.innerHTML=`<div class="modal-header"><h2 id="${titleId}">${tr("reports.call_outcomes")}</h2><div class="modal-navigation"><button type="button" class="secondary modal-back" data-icon="restore" aria-label="${tr("reports.back_to_previous_dialog")}"></button></div></div><div class="modal-body">${range}${outcomes(summary).detailsHtml}</div>`;
      doc.body.appendChild(dialog);
      dialog.querySelector('.modal-back').onclick=close;
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
