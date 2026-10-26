// Feature state is private; the page supplies current data and UI operations.
function createDiagnostics({callReports, api, getLanguage, openModal, $, esc, t, tr, localTimeText, notice}) {
  let diagnosticsRequest = 0;
  let supportReportRequest = 0;
  function diagnosticsTransportLabel(value) { const labels = {"http":tr("reports.http"),"sse":tr("reports.sse"),"websocket":tr("reports.websocket")}; return labels[value] || tr("reports.request"); }
  function diagnosticsProtocolLabel(value) { return ({responses:'Responses',chat_completions:'Chat Completions',anthropic_messages:'Anthropic Messages',unknown:tr("reports.unknown_protocol")}[value] || tr("reports.unknown_protocol")); }
  function diagnosticsContextLabel(record) { const labels = {"allowed":tr("reports.context_ok"),"warned":tr("reports.context_warning"),"blocked":tr("reports.blocked_context_too_large"),"unknown":tr("reports.context_unknown")}; const decision = labels[record.context_decision] || labels.unknown; const estimate = record.estimated_tokens == null ? tr("reports.estimate_unknown") : `${tr("reports.estimate")} ${Number(record.estimated_tokens).toLocaleString()} tokens`; const limit = record.safe_input_limit == null ? tr("reports.safe_limit_unknown") : `${tr("reports.safe_limit")} ${Number(record.safe_input_limit).toLocaleString()}`; return `${decision} · ${estimate} · ${limit}`; }
  function diagnosticsErrorLabel(value) {
    const labels = {
      auth:"reports.sign_in_or_key_rejected", payment_required:"reports.billing_unavailable",
      rate_limit:"reports.rate_limited_2", upstream_5xx:"reports.upstream_5xx", upstream_504:"reports.upstream_timeout",
      proxy_unavailable:"reports.proxy_unavailable", proxy_reset:"reports.proxy_connection_reset",
      dns_failure:"reports.dns_failure", tls_failure:"reports.tls_failure", network:"reports.network_unavailable",
      stream_error:"reports.stream_error", stream_incomplete:"reports.incomplete_stream", malformed_terminal:"reports.invalid_terminal_event",
      upstream_close_pre_output:"reports.disconnected_before_output", upstream_close_after_output:"reports.disconnected_after_output", upstream_close_after_tool:"reports.disconnected_after_tool_call",
      connect_timeout:"reports.connection_timeout", first_event_timeout:"reports.first_response_timeout", first_output_timeout:"reports.first_output_timeout",
      idle_after_output:"reports.output_stalled", local_deadline:"reports.request_deadline_reached", timeout:"reports.timeout",
      upstream_capacity:"reports.local_queue_limit", protocol_rejection:"reports.request_format_rejected",
      context_length_exceeded:"reports.context_too_long", history_reconstruction_failed:"reports.history_recovery_failed", external_compaction_failed:"reports.context_compaction_failed",
      output_limit:"reports.output_limit_reached", content_filter:"reports.content_blocked_upstream", router_error:"reports.routing_failed",
    };
    return tr(labels[value] || "reports.other_error");
  }
  function performanceNumber(value) { if (value === null || value === undefined || value === '') return null; const number = Number(value); return Number.isFinite(number) && number >= 0 ? number : null; }
  function performanceRate(value) { const number = performanceNumber(value); return number == null ? '—' : number.toFixed(1)+'%'; }
  function renderDiagnostics(payload) {
    const probesOpen=Boolean($('diagnostics_probes')?.open);
    const records = Array.isArray(payload.records) ? payload.records : [];
    const health = payload.health && typeof payload.health === 'object' ? payload.health : {};
    const healthSamples = performanceNumber(health.sample_count) || 0;
    $('diagnostics_summary').textContent = healthSamples ? tr("reports.health_across_the_latest_requests", {healthSamples:healthSamples}) : tr(records.length ? "reports.aggregates_appear_here_after_a_few_model_calls" : "reports.no_request_records");
    const fallbackAttempts = performanceNumber(health.fallback_attempt_count) || 0;
    if (fallbackAttempts) $('diagnostics_summary').textContent += tr("reports.transport_fallback_attempts_counted_separately", {fallbackAttempts:fallbackAttempts});
    $('health_summary').innerHTML = healthSamples ? `<div class="health-grid"><div class="health-stat"><span>${tr("reports.success_rate")}</span><strong>${performanceRate(health.success_rate)}</strong><small>${Number(health.success_count || 0).toLocaleString()} / ${Number(health.sample_count || 0).toLocaleString()}</small></div><div class="health-stat"><span>429</span><strong>${performanceRate(health.status_429_rate)}</strong><small>${Number(health.status_429_count || 0).toLocaleString()} ${tr("reports.calls_2")}</small></div><div class="health-stat"><span>502</span><strong>${performanceRate(health.status_502_rate)}</strong><small>${Number(health.status_502_count || 0).toLocaleString()} ${tr("reports.calls_2")}</small></div><div class="health-stat"><span>503</span><strong>${performanceRate(health.status_503_rate)}</strong><small>${Number(health.status_503_count || 0).toLocaleString()} ${tr("reports.calls_2")}</small></div><div class="health-stat"><span>504</span><strong>${performanceRate(health.status_504_rate)}</strong><small>${Number(health.status_504_count || 0).toLocaleString()} ${tr("reports.calls_2")}</small></div><div class="health-stat"><span>${tr("reports.local_queue_limit")}</span><strong>${performanceRate(health.local_capacity_rate)}</strong><small>${Number(health.local_capacity_count || 0).toLocaleString()} ${tr("reports.calls_2")}</small></div></div>` : '';
    const failures = records.slice().reverse().filter(record => !['client_disconnect','client_cancelled','client_websocket_close'].includes(record.error_class) && (Number(record.status) >= 400 || (record.error_class && record.error_class !== 'none'))).slice(0, 20);
    const failureHtml = failures.length ? failures.map(diagnosticFailureHtml).join('') : `<p class="muted">${tr(records.length ? "reports.no_recent_failures" : "reports.no_request_records_advice")}</p>`;
    const probes=Array.isArray(payload.probes) ? payload.probes : [];
    $('diagnostics_records').innerHTML=failureHtml+`<details class="diagnostics-details" id="diagnostics_probes" ${probesOpen?'open':''}><summary>${tr('reports.probes')} · ${probes.length}</summary><p class="muted">${tr('reports.probes_help')}</p>${probes.length ? probes.slice().reverse().map(probeHtml).join('') : `<p class="muted">${tr('reports.no_probes')}</p>`}</details>`;
  }
  function probeHtml(record) {
    const completed=record.outcome==='completed' || (!record.outcome && record.delivery==='terminal_written' && !record.error_code && (record.downstream_terminal==='completed' || Number(record.response_status)>=200 && Number(record.response_status)<300));
    const outcome=completed ? 'completed' : record.outcome==='interrupted' || record.delivery==='partial' ? 'interrupted' : 'failed';
    const stage=record.last_stage || record.last_phase || '';
    return `<details class="request-failure"><summary><span>${tr('reports.probe_'+record.operation)}</span><span>${tr('reports.probe_'+outcome)} · ${Number(record.duration_ms || 0).toLocaleString()} ms</span><time class="muted">${esc(localTimeText(Date.parse(record.observed_at)))}</time></summary><dl><dt>${tr('reports.requested_model')}</dt><dd>${esc(record.model_id || '—')}</dd><dt>${tr('reports.probe_stage')}</dt><dd>${esc(stage || '—')}</dd><dt>${tr('reports.diagnostic_id')}</dt><dd>${esc(record.request_id || record.operation_id || '—')}</dd></dl></details>`;
  }
  function diagnosticFailureHtml(record) {
    const status = Number(record.status);
    const attempt = record.recovery_mode === 'native_http_fallback';
    const result = `${attempt ? tr("reports.pre_fallback_attempt") : ''}${status >= 400 && status <= 599 ? status + ' · ' : ''}${diagnosticsErrorLabel(record.error_class)}`;
    const source = record.provider_id || (record.dialect === 'codex_native' ? tr("current_codex_login") : tr("reports.not_recorded"));
    const fields = [
      [tr("reports.account_provider"), source],
      [tr("reports.requested_model"), record.model_id || tr("reports.not_recorded")],
      [tr("reports.connection_2"), `${diagnosticsProtocolLabel(record.protocol)} · ${diagnosticsTransportLabel(record.transport)}`],
      [tr("reports.duration"), `${Number(record.duration_ms || 0).toLocaleString()} ms`],
      [tr("reports.output_progress"), record.tool_activity ? tr("reports.tool_call_received") : record.output_emitted ? tr("reports.partial_output_received") : tr("reports.no_output_recorded")],
      [tr("reports.automatic_recovery"), record.recovery_succeeded ? tr("reports.recovered") : record.fallback ? tr("reports.another_transport_was_tried") : tr("reports.no_successful_recovery_recorded")],
      [tr("reports.diagnostic_id"), record.request_id || record.observation_id || tr("reports.not_recorded")],
    ];
    if (record.context_decision === 'blocked' || record.context_decision === 'warned') fields.push([tr("reports.context"), diagnosticsContextLabel(record)]);
    const advice = diagnosticFailureAdvice(record);
    return `<details class="request-failure"><summary><span>${esc(record.model_id || tr("reports.unknown_model"))}</span><span>${esc(result)}</span><time class="muted">${esc(record.observed_at ? localTimeText(Date.parse(record.observed_at)) : '')}</time></summary><dl>${fields.map(([label,value]) => `<dt>${esc(label)}</dt><dd>${esc(value)}</dd>`).join('')}</dl><p class="muted">${esc(advice)}</p></details>`;
  }
  function diagnosticFailureAdvice(record) {
    if (record.tool_activity || record.output_emitted) return tr("reports.output_arrived_before_interruption_check_the_task_before_retrying");
    const wait = performanceNumber(record.retry_after_seconds);
    if (wait !== null) return tr("reports.upstream_retry_after",{seconds:wait.toLocaleString()});
    if (['auth','payment_required'].includes(record.error_class)) return tr("reports.check_the_selected_account_sign_in_provider_key_or");
    if (record.error_class === 'rate_limit') return tr("reports.the_upstream_is_rate_limiting_requests_check_quota_and");
    if (record.error_class === 'proxy_unavailable') return tr("reports.cannot_connect_to_the_configured_proxy_check_that_the");
    if (record.error_class === 'dns_failure') return tr("reports.cannot_resolve_the_connection_host_check_the_service_or");
    if (record.error_class === 'tls_failure') return tr("reports.secure_connection_verification_or_handshake_failed_check_the_system");
    if (record.error_class === 'connect_timeout') return tr("reports.the_upstream_connection_timed_out_check_that_the_proxy");
    if (['proxy_reset','network'].includes(record.error_class)) return tr("reports.the_network_connection_failed_check_proxy_and_network_status");
    if (['context_length_exceeded','history_reconstruction_failed','external_compaction_failed'].includes(record.error_class)) return tr("reports.the_context_or_history_could_not_be_processed_keep");
    return tr("reports.use_the_diagnostic_id_to_locate_local_logs_missing");
  }
  function stopDiagnostics() { diagnosticsRequest++; supportReportRequest++; }
  function supportStatusLabel(value) {
    const labels = {"auth_required":tr("reports.sign_in_required"),"transport_error":tr("reports.network_or_proxy_error"),"rate_limited":tr("reports.rate_limited_3"),"unclassified_error":tr("reports.other_error"),"credential_missing":tr("reports.no_credential"),"not_checked":tr("reports.not_checked"),"success":tr("reports.last_check_succeeded")};
    return labels[value] || tr("reports.unknown");
  }
  function supportDetailLabel(value) {
    const labels = {"allowed":tr("reports.likely_writable"),"denied":tr("reports.write_permission_denied"),"unknown":tr("reports.unknown"),"environment":tr("reports.environment_proxy"),"system":tr("reports.system_proxy"),"direct":tr("reports.direct"),"proxy":tr("reports.through_proxy"),"catalog_loaded":tr("reports.emp_model_catalog_observed"),"emp_catalog_absent":tr("reports.emp_model_ids_absent"),"nvm":tr("reports.codex_cli_managed_by_nvm"),"path_cli":tr("reports.codex_cli_on_path"),"codex_app":tr("reports.chatgpt_app_codex")};
    return labels[value] || esc(value || 'unknown');
  }
  function runtimeLabel(value) {
    return {
      catalog_loaded:tr("reports.model_catalog_matches_target_request_route_unverified"),
      emp_catalog_absent:tr("reports.previous_emp_model_ids_are_absent_restoration_unverified"),
      stopped_waiting_for_start:tr("reports.control_interface_unavailable_catalog_unverified"),
      reload_required:tr("reports.shared_catalog_is_stale_ask_its_owner_to_restart"),
      catalog_unverified:tr("reports.shared_model_catalog_not_yet_verified"),
      verification_failed:tr("reports.shared_model_catalog_could_not_be_read"),
      not_checked:tr("reports.shared_model_catalog_has_not_been_observed")
    }[value] || tr("reports.shared_model_catalog_state_unknown");
  }
  function renderSupportReport(report) {
    const box = $('support_report'); if (!box) return;
    const config = report.configuration || {}, codex = report.codex || {}, network = report.network || {}, accounts = report.accounts || {};
    const imported = Array.isArray(accounts.imported) ? accounts.imported : [];
    const accountLines = imported.map(account => `${tr("reports.imported_account")} ${Number(account.index) || 0}: ${supportStatusLabel(account.quota_status)}`);
    const configNotes = [!config.exists && tr("reports.file_missing"), config.write_access_hint === 'denied' && tr("reports.not_writable")].filter(Boolean);
    const compatibilityNote = {
      available:tr("reports.available"),
      unknown:tr("reports.engine_version_could_not_be_observed")
    }[codex.compatibility];
    const versionFallback = { unavailable:tr("reports.no_engine_available") }[codex.compatibility];
    const versionLabel = codex.version || versionFallback || tr("reports.version_unknown");
    const codexNotes = [codex.host_version && `${tr("reports.app_version")} ${codex.host_version}`, compatibilityNote, codex.source && supportDetailLabel(codex.source), codex.target && runtimeLabel(codex.state,codex.target)].filter(Boolean);
    box.innerHTML = `<p><strong>${tr("reports.config")}</strong> ${esc(config.path || '—')}${configNotes.map(note => ' · ' + esc(note)).join('')}</p>
      <p><strong>Codex</strong> ${esc(versionLabel)}${codexNotes.map(note => ' · ' + esc(note)).join('')}</p>
      <p><strong>${tr("reports.proxy")}</strong> ${supportDetailLabel(network.source_at_startup)} · ${supportDetailLabel(network.chatgpt_route)}${network.proxy_scheme ? ' (' + esc(network.proxy_scheme) + ')' : ''}</p>
      <p><strong>${tr("reports.accounts")}</strong> Native: ${supportStatusLabel(accounts.native?.quota_status)}${accountLines.length ? ' · ' + accountLines.map(esc).join(' · ') : ''}</p>`;
  }
  async function loadSupportReport() {
    const request = ++supportReportRequest;
    try {
      const report = await api('/api/support-report');
      if (request === supportReportRequest) renderSupportReport(report);
    } catch (_) {
      if (request === supportReportRequest && $('support_report')) $('support_report').textContent = tr("reports.support_report_is_unavailable");
    }
  }
  async function downloadSupportReport() {
    try {
      const report = await api('/api/support-report');
      const blob = new Blob([JSON.stringify(report, null, 2) + '\n'], {type:'application/json'});
      const url = URL.createObjectURL(blob), link = document.createElement('a');
      link.href = url; link.download = 'EMP-support-report.json'; document.body.appendChild(link); link.click(); link.remove(); URL.revokeObjectURL(url);
      notice(tr("reports.support_report_downloaded"));
    } catch (_) { notice(tr("reports.could_not_download_support_report"), true); }
  }
  async function loadDiagnostics() {
    const request = ++diagnosticsRequest;
    try {
      const payload = await api('/api/diagnostics');
      if (request === diagnosticsRequest && $('diagnostics_summary')) renderDiagnostics(payload);
      return true;
    } catch (error) {
      if (request === diagnosticsRequest && $('diagnostics_summary')) $('diagnostics_summary').textContent = tr("reports.diagnostics_are_unavailable");
      return false;
    }
  }
  function openDiagnostics() {
    openModal(tr("diagnostics"), `<div id="performance_call_report"></div><details class="diagnostics-details"><summary>${tr("reports.system_diagnostics")}</summary><div class="toolbar"><button type="button" class="secondary" data-icon="refresh" onclick="loadDiagnostics(); loadSupportReport()">${esc(t('refresh'))}</button><button type="button" class="secondary" data-icon="download" onclick="downloadSupportReport()">${tr("reports.download_redacted_report")}</button></div><div id="support_report"></div><div id="diagnostics_summary" class="muted"></div><div id="health_summary"></div><div id="diagnostics_records"></div></details>`, '', null);
    callReports.mount('performance_call_report',{},true);
    void loadDiagnostics(); void loadSupportReport();
  }

  return {open:openDiagnostics, stop:stopDiagnostics, load:loadDiagnostics, loadSupport:loadSupportReport, downloadSupport:downloadSupportReport};
}
