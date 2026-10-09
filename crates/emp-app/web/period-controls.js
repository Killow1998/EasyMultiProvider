// Shared period controls and time axis, so every time-based panel reads the same way.
function periodLocalInput(date) { return new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0,16); }
const PERIOD_SECONDS = {'1h':3600, '1d':86400, '7d':7*86400, '30d':30*86400};
const periodPickers = {};
function periodPresetLabel(preset) { return ({'1h':tr('近 1 小时','Last hour'),'1d':tr('近 1 天','Last day'),'7d':tr('近 7 天','Last 7 days'),'30d':tr('近 30 天','Last 30 days'),all:tr('全部历史','All history')})[preset] || preset; }
function periodPickerHtml(id, presets) {
  const presetButtons = presets.map(preset => `<button type="button" class="secondary" data-ui-action="period-preset" data-id="${esc(id)}" data-preset="${esc(preset)}">${esc(periodPresetLabel(preset))}</button>`).join('');
  return `<div class="period-picker"><div class="period-presets" role="group" aria-label="${esc(tr('时间段','Period'))}">${presetButtons}</div><div class="period-custom"><label>${tr('开始','From')}<input type="datetime-local" id="${esc(id)}_start" data-ui-action="period-edit" data-id="${esc(id)}"></label><span class="period-sep">—</span><label>${tr('结束','Until')}<input type="datetime-local" id="${esc(id)}_end" data-ui-action="period-edit" data-id="${esc(id)}"></label><button type="button" class="secondary" data-ui-action="period-apply" data-id="${esc(id)}">${tr('查询','Apply')}</button><button type="button" class="secondary" data-report-action="refresh" data-icon="refresh" data-ui-action="period-refresh" data-id="${esc(id)}">${t('refresh')}</button></div></div>`;
}
function periodPresetRange(preset) {
  const end = Math.ceil(Date.now()/60000)*60;
  return {start:preset === 'all' ? 0 : end - PERIOD_SECONDS[preset], end};
}
function markPeriodPreset(id) {
  const picker = periodPickers[id];
  document.querySelectorAll('[data-ui-action="period-preset"]').forEach(button => { if (button.dataset.id !== id) return; const active = Boolean(picker) && button.dataset.preset === picker.preset; button.classList.toggle('active', active); button.setAttribute('aria-pressed', String(active)); });
}
function showPeriod(id) {
  const picker = periodPickers[id]; if (!picker) return;
  const start = $(`${id}_start`), end = $(`${id}_end`);
  if (start) start.value = picker.start > 0 ? periodLocalInput(new Date(picker.start*1000)) : '';
  if (end) end.value = periodLocalInput(new Date(picker.end*1000));
  markPeriodPreset(id);
}
function setPeriod(id, period, options = {}) {
  const picker = periodPickers[id]; if (!picker) return;
  Object.assign(picker, period); showPeriod(id); return picker.onChange(picker, options);
}
function registerPeriodPicker(id, preset, onChange, range = null) {
  const period = range ? {preset:'',...range} : {preset,...periodPresetRange(preset)};
  periodPickers[id] = {onChange,...period}; showPeriod(id);
  return onChange(periodPickers[id], {user:true});
}
function selectPeriodPreset(id, preset) { return setPeriod(id, {preset, ...periodPresetRange(preset)}, {user:true}); }
function applyCustomPeriod(id) {
  const start = new Date($(`${id}_start`)?.value).getTime()/1000, end = new Date($(`${id}_end`)?.value).getTime()/1000;
  if (!Number.isFinite(start) || !Number.isFinite(end) || start >= end) return notice(tr('请选择有效的时间段。','Choose a valid period.'), true);
  return setPeriod(id, {preset:'', start, end}, {user:true});
}
function editPeriod(id) { const picker = periodPickers[id]; if (picker) { picker.preset = ''; markPeriodPreset(id); } }
// A preset period slides forward to now on refresh; a custom period stays fixed.
function refreshPeriod(id, user = true) {
  const picker = periodPickers[id]; if (!picker) return;
  if (picker.preset) Object.assign(picker, periodPresetRange(picker.preset));
  showPeriod(id); return picker.onChange(picker, {user});
}
const AXIS_STEPS = [60, 300, 900, 1800, 3600, 3*3600, 6*3600, 12*3600, 86400, 2*86400, 7*86400];
function timeAxisTicks(start, end, maxTicks = 8) {
  const step = AXIS_STEPS.find(value => (end-start)/value <= maxTicks) || AXIS_STEPS[AXIS_STEPS.length-1];
  const offset = new Date(start*1000).getTimezoneOffset()*60;
  const ticks = []; let previousDay = '';
  for (let at = Math.ceil((start-offset)/step)*step+offset; at <= end; at += step) {
    const date = new Date(at*1000), day = date.toLocaleDateString([], {month:'numeric',day:'numeric'});
    const time = step >= 86400 ? day : date.toLocaleTimeString([], {hour:'2-digit',minute:'2-digit',hour12:false});
    ticks.push({at, time, day:step < 86400 && day !== previousDay ? day : ''}); previousDay = day;
  }
  return ticks;
}
// Round local-time ticks under a chart; `x` maps a Unix time to the chart's x coordinate.
function timeAxisSvg(start, end, x, y, left, right) {
  return timeAxisTicks(start, end).map(tick => {
    const position = x(tick.at), anchor = position-left < 24 ? 'start' : right-position < 24 ? 'end' : 'middle';
    return `<line x1="${position.toFixed(1)}" y1="${y-14}" x2="${position.toFixed(1)}" y2="${y-10}" stroke="var(--border-strong)"/><text x="${position.toFixed(1)}" y="${y}" text-anchor="${anchor}" fill="var(--muted)" font-size="10">${esc(tick.time)}${tick.day ? `<tspan x="${position.toFixed(1)}" dy="13">${esc(tick.day)}</tspan>` : ''}</text>`;
  }).join('');
}
