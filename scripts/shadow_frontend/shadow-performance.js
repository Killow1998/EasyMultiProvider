// Preview-only presentation. Existing report queries and metric calculations stay intact.
function createShadowPerformanceView({esc, tr, getLanguage, number, seconds, tps, rate, outcomes, bindOutcomes, definition}) {
  const chart = createShadowPerformanceChart({esc,tr,getLanguage,number,seconds,tps});
  let dispose = () => {};
  function stat(title,value,hint,spark = '',tone = '', outcome = false) {
    return `<div class="shadow-stat ${tone}"${outcome ? ' data-call-outcomes tabindex="0"' : ''}><span class="shadow-stat-label" title="${esc(definition(title))}">${esc(title)}</span><strong>${esc(value)}</strong><small>${esc(hint)}</small>${spark}</div>`;
  }
  function facts(report) {
    const s = report.summary || {};
    const result = outcomes(s);
    const samples = count => `${number(count)} ${tr('个样本','samples')}`;
    const stats = [
      stat(tr('调用数','Calls'),number(s.calls),`${number(s.completed)} ${tr('次完成','completed')}`,chart.spark(report,'calls')),
      stat(result.label,result.value,result.hint,'',result.failed > 0 ? 'shadow-stat-warning' : '',true),
      stat(tr('平均耗时','Mean duration'),seconds(s.duration_ms),tr('每次调用','Per call')),
      stat('TTFT',seconds(s.ttft_ms),samples(s.ttft_samples),chart.spark(report,'ttft_ms')),
      stat('TPS',tps(s.tokens_per_second),samples(s.tps_samples),chart.spark(report,'tokens_per_second')),
      stat(tr('输入缓存率','Input cache rate'),rate(s.cached_input_tokens,s.cache_input_tokens),`${number(s.cached_input_tokens)} / ${number(s.cache_input_tokens)} token`),
    ].join('');
    return stats;
  }
  function html(report, metric) { return `<div class="shadow-stat-grid">${facts(report)}</div>${chart.html(report,metric)}`; }
  function bind(root, report, metric) { stop(); const chartDispose=chart.bind(root,report,metric), outcomeDispose=bindOutcomes(root,report.summary || {},report); dispose=()=>{chartDispose();outcomeDispose();}; }
  function stop() { dispose(); dispose = () => {}; }
  return {html,facts,stat,bind,stop};
}
