// 2026-10-17, Codex 0.163.0, ten interleaved rounds. See docs/external-collaboration.md.
// Each cell stores paired TPS/time ranges and its valid pair count.
const collaborationMeasurements = [
  {model:'GPT-5.6 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.96,1.12],time:[0.91,1.95],samples:10}, on:{tps:[0.36,0.42],time:[1.88,2.19],samples:10}},
  {model:'GPT-5.6 Terra', off:{tps:[1,1],time:[1,1],samples:3}, on_demand:{tps:[0.46,0.46],time:[1.57,1.57],samples:1}, on:{tps:[0.22,0.22],time:[2.90,2.90],samples:1}},
  {model:'GPT-5.6 Luna', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.98,1.08],time:[0.91,1.09],samples:9}, on:{tps:[0.27,0.30],time:[2.26,2.52],samples:10}},
  {model:'GPT-6 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.88,1.09],time:[0.83,2.07],samples:10}, on:{tps:[0.35,0.40],time:[1.81,2.26],samples:10}},
  {model:'GPT-6 Luna', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.92,1.01],time:[0.92,1.17],samples:10}, on:{tps:[0.28,0.32],time:[2.23,2.51],samples:9}},
  {model:'GPT-6 Astra', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.91,1.12],time:[0.89,1.12],samples:10}, on:{tps:[0.92,1.33],time:[0.81,1.09],samples:10}},
  {model:'GPT-6.1 Sol', off:{tps:[1,1],time:[1,1],samples:10}, on_demand:{tps:[0.92,1.28],time:[0.83,1.05],samples:10}, on:{tps:[0.51,0.98],time:[1.05,1.73],samples:10}},
  {model:'GLM-5.3-Flash', external:true, on_demand:{tps:[0.82,1.12],time:[0.87,1.57],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}},
  {model:'Opus 5.5', external:true, on_demand:{tps:[0.96,1.04],time:[0.86,1.11],samples:7}, on:{tps:[1,1],time:[1,1],samples:9}},
  {model:'Sonnet 5.5', external:true, on_demand:{tps:[0.88,1.14],time:[0.94,1.09],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}},
  {model:'Haiku 5.5', external:true, on_demand:{tps:[0.95,1.07],time:[0.88,1.18],samples:10}, on:{tps:[1,1],time:[1,1],samples:10}}
];

function collaborationComparison() {
  const modes = ['off','on_demand','on'];
  const ratio = values => {
    const [min, max] = values.map(value => value.toFixed(2));
    return esc(tr('models.collaboration_ratio', {ratio:min === max ? min : `${min}–${max}`}));
  };
  const descriptions = modes.map(mode => `<dt>${tr('models.collaboration_' + mode)}</dt><dd>${tr('models.collaboration_' + mode + '_help')}</dd>`).join('');
  const rows = collaborationMeasurements.map(row => `<tr><th scope="row">${esc(row.model)}</th>${modes.map(mode => {
    const value = row[mode];
    if (value) return `<td title="${esc(tr('models.collaboration_samples', {count:value.samples}))}"><div>${ratio(value.tps)}</div><div>${ratio(value.time)}</div></td>`;
    const label = tr(row.external && mode === 'off' ? 'models.collaboration_disallowed' : 'models.collaboration_unmeasured');
    return `<td>${esc(label)}</td>`;
  }).join('')}</tr>`).join('');
  return `<button type="button" class="shadow-help" data-shadow-help="" data-shadow-help-template="collaboration-comparison" aria-label="${tr('models.collaboration_help_title')}">?</button><template id="collaboration-comparison"><div class="collaboration-comparison"><strong>${tr('models.collaboration_help_title')}</strong><dl class="collaboration-modes">${descriptions}</dl><strong>${tr('models.collaboration_comparison')}</strong><p>${tr('models.collaboration_comparison_context', {date:'2026-10-17',version:'0.163.0',count:10})}</p><p>${tr('models.collaboration_comparison_metric')}</p><div class="collaboration-table"><table><thead><tr><th scope="col">${tr('reports.model')}</th>${modes.map(mode => `<th scope="col">${tr('models.collaboration_' + mode)}</th>`).join('')}</tr></thead><tbody>${rows}</tbody></table></div><p>${tr('models.collaboration_comparison_baseline')}</p><p>${tr('models.collaboration_comparison_sparse')}</p><p>${tr('models.collaboration_comparison_variation')}</p></div></template>`;
}
