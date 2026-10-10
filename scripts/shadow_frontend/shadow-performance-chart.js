// Native SVG time chart; missing latency samples stay missing, rather than zero.
function createShadowPerformanceChart({esc, tr, getLanguage, number, seconds, tps}) {
  const valid = value => typeof value === 'number' && Number.isFinite(value) && value >= 0;
  const label = metric => metric === 'tokens' ? 'Token' : metric === 'cost_nanos' ? tr('API 估算 · USD','API estimate · USD') : metric === 'calls' ? tr('调用量','Calls') : metric === 'ttft_ms' ? 'TTFT' : 'TPS';
  const format = (value, metric) => !valid(value) ? '—' : metric === 'cost_nanos' ? 'US$'+(value/1e9).toFixed(4) : metric === 'tokens' ? number(value) : metric === 'calls' ? number(value) : metric === 'ttft_ms' ? seconds(value) : tps(value);
  const date = value => new Date(value*1000).toLocaleString(getLanguage() === 'en' ? 'en-US' : 'zh-CN', {month:'short',day:'numeric',hour:'2-digit',minute:'2-digit',hour12:false});
  function buckets(report) {
    const span = Math.max(1,report.end-report.start), step = Math.max(60,span/48);
    const count = Math.max(Math.ceil(span/step), ...(report.periods || []).map(row => Math.round((row.start-report.start)/step)+1));
    const rows = Array.from({length:count}, (_,i) => ({start:report.start+i*step,calls:0,tokens:0,cost_nanos:0,ttft_ms:null,tokens_per_second:null,ttft_samples:0,tps_samples:0}));
    for (const row of report.periods || []) {
      const i = Math.round((row.start-report.start)/step);
      if (i >= 0 && i < count) Object.assign(rows[i],row);
    }
    return {rows,step};
  }
  function segments(rows, metric) {
    const result = []; let segment = null;
    rows.forEach((row,i) => {
      if (!valid(row[metric])) { segment = null; return; }
      if (!segment) { segment = []; result.push(segment); }
      segment.push({value:row[metric],index:i});
    });
    return result;
  }
  function scale(maximum, integers) {
    const raw = Math.max(maximum,integers ? 4 : 1)/4;
    const power = 10**Math.floor(Math.log10(raw));
    const step = [1,2,2.5,5,10].find(n => n*power >= raw)*power;
    return (integers ? Math.ceil(step) : step)*4;
  }
  function spark(report, metric) {
    const {rows} = buckets(report), parts = segments(rows,metric);
    if (parts.reduce((n,part) => n+part.length,0) < 2) return '';
    const max = Math.max(1,...rows.map(row => valid(row[metric]) ? row[metric] : 0));
    const point = p => `${(p.index/(rows.length-1)*140).toFixed(1)},${(25-p.value/max*22).toFixed(1)}`;
    return `<svg class="shadow-stat-spark" viewBox="0 0 140 28" preserveAspectRatio="none" aria-hidden="true">${parts.map(part => `<polyline points="${part.map(point).join(' ')}"/>`).join('')}</svg>`;
  }
  function html(report, metric) {
    const {step} = buckets(report);
    const interval = step >= 3600 ? `${+(step/3600).toFixed(1)} ${tr('小时','h')}` : `${+(step/60).toFixed(1)} ${tr('分钟','min')}`;
    const buttons = [['calls',tr('调用量','Calls')],['tokens_per_second','TPS'],['ttft_ms','TTFT']].map(([key,title]) => `<button type="button" class="secondary${metric === key ? ' active' : ''}" aria-pressed="${metric === key}" data-call-metric="${key}">${title}</button>`).join('');
    return `<div class="shadow-performance-chart"><div class="shadow-chart-heading"><div><h3>${tr('调用趋势','Call activity')}</h3><p>${tr('每','Per ')} ${esc(interval)} · ${esc(date(report.start))} — ${esc(date(report.end))}</p></div><div class="report-tabs" role="group" aria-label="${tr('指标','Metric')}">${buttons}</div></div><div class="shadow-chart-canvas" tabindex="0" role="group" aria-label="${esc(label(metric))} · ${tr('左右方向键查看数据','Use left and right arrows to inspect data')}"><svg class="shadow-time-chart" role="img" aria-label="${esc(label(metric))}"></svg><div class="shadow-chart-tooltip" hidden role="status" aria-live="polite"></div></div></div>`;
  }
  function bind(root, report, metric) {
    const canvas = root.querySelector('.shadow-chart-canvas'); if (!canvas) return () => {};
    const svg = canvas.querySelector('svg'), tip = canvas.querySelector('.shadow-chart-tooltip');
    const {rows,step} = buckets(report), parts = segments(rows,metric);
    const bar = ['calls','tokens','cost_nanos'].includes(metric);
    const maximum = Math.max(0,...rows.map(row => valid(row[metric]) ? row[metric] : 0));
    const ceiling = scale(maximum,metric !== 'cost_nanos' && bar);
    let width = 0, index = -1, disposed = false;
    const left = 50, top = 18, bottom = 180;
    const plotWidth = () => width-left-14;
    const x = i => left+(i+.5)/rows.length*plotWidth();
    const y = value => bottom-value/ceiling*(bottom-top);
    const point = p => `${x(p.index).toFixed(1)},${y(p.value).toFixed(1)}`;
    function hide() { index = -1; tip.hidden = true; svg.querySelector('[data-chart-cursor]')?.setAttribute('visibility','hidden'); }
    function show(i) {
      index = Math.max(0,Math.min(rows.length-1,i)); const row = rows[index];
      const cursor = svg.querySelector('[data-chart-cursor]');
      cursor.setAttribute('x',(left+index/rows.length*plotWidth()).toFixed(1)); cursor.setAttribute('visibility','visible');
      const samples = bar ? row.requests ?? row.calls : metric === 'ttft_ms' ? row.ttft_samples : row.tps_samples;
      tip.replaceChildren();
      const heading = document.createElement('span'), value = document.createElement('strong'), foot = document.createElement('small');
      heading.textContent = `${date(row.start)} – ${new Date(Math.min(report.end,row.start+step)*1000).toLocaleTimeString(getLanguage() === 'en' ? 'en-US' : 'zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false})}`;
      value.textContent = `${label(metric)}  ${format(row[metric],metric)}`;
      foot.textContent = bar ? `${number(samples)} ${tr('条记录','records')}` : `${number(samples)} ${tr('个样本','samples')}`;
      tip.append(heading,value,foot);
      for (const series of report.chartSeries || []) { const amount = series.values[index] || 0; if (!amount) continue; const item = document.createElement('small'); item.style.color=series.color; item.textContent=series.label+'  '+format(amount,metric); tip.append(item); }
      tip.hidden = false;
      tip.style.left = Math.max(4,Math.min(width-tip.offsetWidth-4,x(index)-tip.offsetWidth/2))+'px';
    }
    function draw() {
      if (disposed || !canvas.isConnected) return;
      const nextWidth = Math.max(180,Math.floor(canvas.clientWidth)); if (nextWidth === width) return;
      width = nextWidth; svg.setAttribute('viewBox',`0 0 ${width} 224`);
      const axis = Array.from({length:5}, (_,i) => {
        const value = ceiling*i/4, position = y(value);
        const text = metric === 'cost_nanos' ? '$'+(value/1e9).toLocaleString(undefined,{maximumFractionDigits:3}) : metric === 'ttft_ms' ? `${+(value/1000).toFixed(2)}s` : new Intl.NumberFormat(getLanguage() === 'en' ? 'en-US' : 'zh-CN',{notation:'compact',maximumFractionDigits:1}).format(value);
        return `<line class="shadow-chart-grid" x1="${left}" x2="${width-14}" y1="${position}" y2="${position}"/><text x="${left-8}" y="${position+4}" text-anchor="end">${esc(text)}</text>`;
      }).join('');
      const ticks = timeAxisTicks(report.start,report.end,Math.max(2,Math.floor(plotWidth()/105))).map(tick => {
        const position = left+(tick.at-report.start)/(report.end-report.start || 1)*plotWidth();
        const anchor = position-left < 25 ? 'start' : width-14-position < 25 ? 'end' : 'middle';
        return `<text x="${position.toFixed(1)}" y="201" text-anchor="${anchor}">${esc(tick.time)}${tick.day ? `<tspan x="${position.toFixed(1)}" dy="14">${esc(tick.day)}</tspan>` : ''}</text>`;
      }).join('');
      const series = report.chartSeries || [{values:rows.map(row => row[metric]),color:'var(--accent)'}];
      const marks = bar ? rows.map((row,i) => {
        let accumulated=0; const barWidth=Math.min(40,plotWidth()/rows.length*.72);
        return series.map(item => { const amount=item.values[i] || 0, base=accumulated; accumulated+=amount;
          if (amount <= 0) return '';
          return `<rect data-stack-value="${amount}" x="${(x(i)-barWidth/2).toFixed(1)}" y="${y(accumulated).toFixed(1)}" width="${barWidth.toFixed(1)}" height="${(y(base)-y(accumulated)).toFixed(1)}" fill="${esc(item.color)}"/>`;
        }).join('');
      }).join('') : parts.map(part => {
        const first = part[0], last = part.at(-1), points = part.map(point).join(' ');
        return `${part.length > 1 ? `<polygon class="shadow-chart-area" points="${x(first.index)},${bottom} ${points} ${x(last.index)},${bottom}"/><polyline class="shadow-chart-line" points="${points}"/>` : ''}${part.map(p => `<circle class="shadow-chart-point" cx="${x(p.index).toFixed(1)}" cy="${y(p.value).toFixed(1)}" r="3"/>`).join('')}`;
      }).join('');
      const empty = !parts.length || (bar && maximum === 0) ? `<text class="shadow-chart-empty" x="${left+plotWidth()/2}" y="96" text-anchor="middle">${tr('此时间段暂无数据','No data in this period')}</text>` : '';
      svg.innerHTML = `${axis}${ticks}<rect data-chart-cursor visibility="hidden" y="${top}" width="${plotWidth()/rows.length}" height="${bottom-top}"/>${marks}${empty}`;
      if (index >= 0) show(index);
    }
    canvas.onpointermove = event => {
      const position = event.clientX-canvas.getBoundingClientRect().left;
      if (position < left || position > width-14) return hide();
      show(Math.floor((position-left)/plotWidth()*rows.length));
    };
    canvas.onpointerleave = hide; canvas.onblur = hide;
    canvas.onkeydown = event => {
      if (!['ArrowLeft','ArrowRight','Home','End','Escape'].includes(event.key)) return;
      if (event.key === 'Escape' && index < 0) return;
      event.preventDefault();
      if (event.key === 'Escape') { event.stopPropagation(); return hide(); }
      const last = rows.findLastIndex(row => valid(row[metric]) && (!bar || row[metric] > 0));
      show(event.key === 'Home' ? 0 : event.key === 'End' ? rows.length-1 : index < 0 ? Math.max(0,last) : index+(event.key === 'ArrowLeft' ? -1 : 1));
    };
    draw(); const observer = new ResizeObserver(draw); observer.observe(canvas);
    return () => { disposed = true; observer.disconnect(); canvas.onpointermove = canvas.onpointerleave = canvas.onblur = canvas.onkeydown = null; };
  }
  return {html,bind,spark};
}
