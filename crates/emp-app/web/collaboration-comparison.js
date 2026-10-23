// 2026-10-18, Codex 0.163.0, ten paired rounds plus gap-filling controls. See docs/external-collaboration.md.
// Each cell stores paired TPS/time ranges and its valid pair count.
const collaborationMeasurements = [
  {model:'GPT-5.6 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.96,1.12],time:[0.91,1.95],samples:10}, on:{tps:[0.36,0.42],time:[1.88,2.19],samples:10}},
  {model:'GPT-5.6 Luna', off:{tps:[1,1],time:[1,1],samples:11}, on_demand:{tps:[0.98,1.08],time:[0.91,1.09],samples:10}, on:{tps:[0.27,0.30],time:[2.26,2.52],samples:10}},
  {model:'GPT-6 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.88,1.09],time:[0.83,2.07],samples:10}, on:{tps:[0.35,0.40],time:[1.81,2.26],samples:10}},
  {model:'GPT-6 Luna', off:{tps:[1,1],time:[1,1],samples:11}, on_demand:{tps:[0.92,1.01],time:[0.92,1.17],samples:10}, on:{tps:[0.28,0.32],time:[2.22,2.51],samples:10}},
  {model:'GPT-6 Astra', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.91,1.12],time:[0.89,1.12],samples:10}, on:{tps:[0.92,1.33],time:[0.81,1.09],samples:10}},
  {model:'GPT-6.1 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.92,1.28],time:[0.83,1.05],samples:10}, on:{tps:[0.51,0.98],time:[1.05,1.73],samples:10}},
  {model:'GLM-5.3-Flash', external:true, on_demand:{tps:[0.82,1.12],time:[0.87,1.57],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}},
  {model:'Opus 5.5', external:true, on_demand:{tps:[0.94,1.04],time:[0.86,1.11],samples:10}, on:{tps:[1,1],time:[1,1],samples:12}},
  {model:'Sonnet 5.5', external:true, on_demand:{tps:[0.88,1.14],time:[0.94,1.09],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}},
  {model:'Haiku 5.5', external:true, on_demand:{tps:[0.95,1.07],time:[0.88,1.18],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}}
];

// Same-subscription controls: medians of 20 matched pairs per effort.
const subscriptionMeasurements = [
  {effort:'low', tps:0.9709,ttft:0.9689,time:1.0286,samples:20},
  {effort:'medium', tps:1.0053,ttft:0.9567,time:0.9796,samples:20},
  {effort:'high', tps:0.9487,ttft:0.9880,time:1.0589,samples:20},
  {effort:'xhigh', tps:0.9731,ttft:1.0073,time:1.0236,samples:20},
  {effort:'max', tps:0.9626,ttft:0.9562,time:1.0074,samples:20}
];

function subscriptionComparison() {
  const columns = ['tps','ttft','time'];
  const rows = subscriptionMeasurements.map(row => `<tr><th scope="row">${esc(row.effort)}</th>${columns.map(metric => `<td title="${esc(tr('models.collaboration_samples',{count:row.samples}))}">${esc(tr('models.collaboration_ratio',{ratio:row[metric].toFixed(2)}))}</td>`).join('')}</tr>`).join('');
  return `<h3 id="subscription-comparison-title">${tr('models.subscription_comparison_title')}</h3><p class="collaboration-takeaway">${tr('models.subscription_comparison_takeaway')}</p><p>${tr('models.subscription_comparison_baseline')}</p><p>${tr('models.subscription_comparison_guide')}</p><div class="collaboration-table" tabindex="0" role="region" aria-labelledby="subscription-comparison-title"><table aria-labelledby="subscription-comparison-title"><thead><tr><th scope="col">${tr('models.subscription_comparison_effort')}</th>${columns.map(metric => `<th scope="col">${tr('models.subscription_comparison_'+metric)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table></div><p>${tr('models.subscription_comparison_context',{date:'2026-10-18',version:'0.163.0',count:20})}</p>`;
}

// 2026-10-23 UTC, standalone 0.164.0 Code Mode, GPT-6.1 Sol xhigh.
// Three samples per arm; these are ratios of medians, not paired-ratio medians.
function codeModeComparison() {
  const baseline = {tps:54.12654564662915, ttft:8.05089005397167, time:47.58282882999629};
  const measured = [
    {mode:'off', tps:56.38226790722568, ttft:7.338474391959608, time:43.89440695394296},
    {mode:'on_demand', tps:57.517867000559285, ttft:7.774420959991403, time:43.91945511905942},
    {mode:'on', tps:52.55539971973242, ttft:7.259282188955694, time:48.18346161104273}
  ];
  const metrics = ['tps','ttft','time'];
  const rows = measured.map(row => `<tr><th scope="row">${tr('models.collaboration_'+row.mode)}</th>${metrics.map(metric => `<td>${tr('models.collaboration_ratio',{ratio:(row[metric]/baseline[metric]).toFixed(2)})}</td>`).join('')}</tr>`).join('');
  return `<details><summary>${tr('models.collaboration_current_title')}</summary><p>${tr('models.collaboration_current_context')}</p><p>${tr('models.subscription_comparison_guide')}</p><div class="collaboration-table" tabindex="0" role="region" aria-label="${esc(tr('models.collaboration_current_title'))}"><table><thead><tr><th scope="col">${tr('models.collaboration_mode')}</th>${metrics.map(metric => `<th scope="col">${tr('models.subscription_comparison_'+metric)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table></div><p>${tr('models.collaboration_current_scope')}</p></details>`;
}

function collaborationComparison() {
  const modes = ['off','on_demand','on'];
  const ratio = values => {
    const [min, max] = values.map(value => value.toFixed(2));
    return esc(tr('models.collaboration_ratio', {ratio:min === max ? min : `${min}–${max}`}));
  };
  const descriptions = modes.map(mode => `<dt>${tr('models.collaboration_' + mode)}</dt><dd>${tr('models.collaboration_' + mode + '_help')}</dd>`).join('');
  const tables = ['tps','time'].map(metric => {
    const rows = collaborationMeasurements.map(row => `<tr><th scope="row">${esc(row.model)}</th>${modes.map(mode => {
      const value = row[mode];
      if (value) {
        const baseline = mode === (row.external ? 'on' : 'off');
        return `<td title="${esc(tr('models.collaboration_samples', {count:value.samples}))}">${baseline ? tr('models.collaboration_baseline_value') : ratio(value[metric])}</td>`;
      }
      const label = tr(row.external && mode === 'off' ? 'models.collaboration_disallowed' : 'models.collaboration_unmeasured');
      return `<td>${esc(label)}</td>`;
    }).join('')}</tr>`).join('');
    return `<h3 id="collaboration-${metric}-title">${tr('models.collaboration_' + metric + '_title')}</h3><p>${tr('models.collaboration_' + metric + '_guide')}</p><div class="collaboration-table" tabindex="0" role="region" aria-labelledby="collaboration-${metric}-title"><table aria-labelledby="collaboration-${metric}-title"><thead><tr><th scope="col">${tr('reports.model')}</th>${modes.map(mode => `<th scope="col">${tr('models.collaboration_' + mode)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table></div>`;
  }).join('');
  return `<button type="button" class="shadow-help" data-shadow-help="" data-shadow-help-template="collaboration-comparison" aria-label="${tr('models.collaboration_help_title')}" aria-haspopup="dialog" aria-expanded="false" aria-controls="shadow_feedback">?</button><template id="collaboration-comparison"><div class="collaboration-comparison"><strong>${tr('models.collaboration_help_title')}</strong><p class="collaboration-takeaway">${tr('models.collaboration_takeaway')}</p><dl class="collaboration-modes">${descriptions}</dl>${codeModeComparison()}<details><summary>${tr('models.collaboration_comparison')}</summary><p>${tr('models.collaboration_comparison_context', {date:'2026-10-18',version:'0.163.0',count:10})}</p><p>${tr('models.collaboration_comparison_baseline')}</p>${tables}<p>${tr('models.collaboration_comparison_metric')}</p><p>${tr('models.collaboration_comparison_variation')}</p>${subscriptionComparison()}</details></div></template>`;
}
