// Shared report presentation. Existing report queries and metric calculations stay intact.
function createPerformanceView({esc, tr, getLanguage, number, seconds, tps, rate, outcomes, bindOutcomes, definition}) {
  const chart = createPerformanceChart({esc,tr,getLanguage,number,seconds,tps});
  let dispose = () => {};
  function stat(title,value,hint,{spark='',tone='',outcome=false,exact=''} = {}) {
    return `<div class="presentation-stat ${tone}"${outcome ? ' data-call-outcomes tabindex="0"' : ''}><span class="presentation-stat-label" title="${esc(definition(title))}">${esc(title)}</span><strong${exact ? ` title="${esc(exact)}"` : ''}>${esc(value)}</strong><small>${esc(hint)}</small>${spark}</div>`;
  }
  function facts(report) {
    const s = report.summary || {};
    const result = outcomes(s);
    const samples = count => `${number(count)} ${tr("reports.samples")}`;
    const stats = [
      stat(tr("reports.calls_4"),number(s.calls),`${number(s.completed)} ${tr("reports.completed_3")}`,{spark:chart.spark(report,'calls')}),
      stat(result.label,result.value,result.hint,{tone:result.failed > 0 ? 'presentation-stat-warning' : '',outcome:true}),
      stat(tr("reports.mean_duration"),seconds(s.duration_ms),tr("reports.per_call")),
      stat('TTFT',seconds(s.ttft_ms),samples(s.ttft_samples),{spark:chart.spark(report,'ttft_ms')}),
      stat('TPS',tps(s.tokens_per_second),samples(s.tps_samples),{spark:chart.spark(report,'tokens_per_second')}),
      stat(tr("reports.input_cache_rate"),rate(s.cached_input_tokens,s.cache_input_tokens),`${compactUsage(s.cached_input_tokens).text} / ${compactUsage(s.cache_input_tokens).text} token`,{exact:`${number(s.cached_input_tokens)} / ${number(s.cache_input_tokens)} token`}),
    ].join('');
    return stats;
  }
  function html(report, metric) { return `<div class="presentation-stat-grid">${facts(report)}</div>${chart.html(report,metric)}`; }
  function bind(root, report, metric) { stop(); const chartDispose=chart.bind(root,report,metric), outcomeDispose=bindOutcomes(root,report.summary || {},report); dispose=()=>{chartDispose();outcomeDispose();}; }
  function stop() { dispose(); dispose = () => {}; }
  return {html,facts,stat,bind,stop};
}
