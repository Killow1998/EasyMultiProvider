// Presentation layout and settings; data writes and error presentation have their own owners.
// Bar and ring views use the same quota buckets and remaining/used preference.
function presentationQuotaMeters(account, busy = false, animate = false) {
  if (document.body.dataset.quotaStyle === 'bar') return quotaMetersHtml(account,busy,animate);
  let buckets = quotaDisplayBuckets(account);
  if (!buckets.length && account.quota_pending) buckets = [300,10080].map(windowMinutes => ({windowMinutes,pending:true}));
  if (!buckets.length) return `<span class="muted">${tr('未刷新','Not refreshed')}</span>`;
  const arc = 'M 76.309 67.223 A 37 37 0 1 1 76.309 24.777';
  return `<div class="presentation-quota-pair">${buckets.map((item,index) => {
    const pending = item.pending || (account.quota_pending && item.unreported);
    const unlimited = item.unreported && !pending && item.windowMinutes === 300;
    const value = unlimited ? 233 : item.unreported || pending ? null : quotaDisplay.value(item.remaining);
    const color = item.remaining <= 20 ? '#be4949' : item.remaining <= 50 ? '#ab711d' : '#2f884d';
    const duration = compactWindowText(item.windowMinutes);
    const label = item.limitId && !['codex','claude'].includes(item.limitId) ? quotaLimitLabel(item.limitId)+' · '+duration : duration;
    const mirror = index % 2 ? 'translate(92 0) scale(-1 1)' : '';
    const mask = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 92 92"><g transform="${mirror}"><path d="${arc}" fill="none" stroke="white" stroke-width="5" stroke-linecap="round"/></g></svg>`;
    const time = unlimited ? '1m111s' : resetCountdownText(item.resetsAt);
    return `<div class="presentation-ring-meter"><div class="presentation-ring" style="--quota-color:${color};--fill:${Math.min(100,value || 0)};--arc-mask:url('data:image/svg+xml,${encodeURIComponent(mask)}')" role="img" aria-label="${esc(label+' '+(value === null ? '—' : value+'%')+' '+time)}"><svg viewBox="0 0 92 92" aria-hidden="true"><g transform="${mirror}"><path class="rail" d="${arc}"/><path class="level" d="${arc}" pathLength="100" ${unlimited ? 'opacity="0"' : ''}/></g></svg>${unlimited ? '<div class="presentation-flow-ring"><span data-presentation-motion="ring"></span></div>' : ''}<strong class="presentation-ring-value">${value === null ? '—' : esc(value)+'%'}</strong></div><div class="presentation-ring-meta"><span class="presentation-ring-window">${esc(label)}</span><span class="presentation-ring-time">${uiIcon('refresh')}<span ${!unlimited && item.resetsAt ? `data-reset-countdown="${esc(item.resetsAt)}"` : ''}>${esc(time || '—')}</span></span></div></div>`;
  }).join('')}</div>`;
}
const presentationPreferencePrefix = window.EMP_PREVIEW ? 'emp.shadow.' : 'emp.';
document.body.dataset.quotaStyle = storedPreference(presentationPreferencePrefix+'quotaStyle',window.EMP_PREVIEW ? 'ring' : 'bar');
const presentationDotColors = [
  ['multi','多彩','Multicolor',''],['blue','蓝色','Blue','#487de8'],
  ['purple','紫色','Purple','#9473c6'],['teal','青色','Teal','#299c9f'],
  ['amber','琥珀','Amber','#bc7b25'],['rose','玫红','Rose','#d16b8c'],
];
const presentationDotPreference = presentationPreferencePrefix+'dotColor';
const presentationDotCustomPreference = presentationPreferencePrefix+'dotCustom';
let presentationDotCustom = storedPreference(presentationDotCustomPreference,'#487de8');
if (!/^#[0-9a-f]{6}$/i.test(presentationDotCustom)) presentationDotCustom = '#487de8';
function presentationSetDotColor(choice, save = false) {
  const preset = presentationDotColors.find(row => row[0] === choice);
  if (!preset && choice !== 'custom') choice = 'multi';
  document.body.dataset.dotColor = choice;
  const color = choice === 'custom' ? presentationDotCustom : preset?.[3];
  for (const name of ['--presentation-dot-color','--presentation-dot-secondary','--presentation-dot-third']) {
    if (color) document.body.style.setProperty(name,color);
    else document.body.style.removeProperty(name);
  }
  if (save) savePreference(presentationDotPreference,choice);
  document.querySelectorAll('[data-presentation-dot-color]').forEach(button => button.setAttribute('aria-pressed',String(button.dataset.presentationDotColor === choice)));
  const custom = document.querySelector('.presentation-dot-custom');
  if (custom) custom.dataset.selected = String(choice === 'custom');
  window.dispatchEvent(new Event('presentation-dot-color-change'));
}
presentationSetDotColor(storedPreference(presentationDotPreference,'multi'));
function presentationDotColorSettings() {
  return `<div class="setting-row"><div class="setting-title">${tr('点阵颜色','Dotted outline color')}</div><div class="presentation-dot-choices" role="group" aria-label="${tr('点阵颜色','Dotted outline color')}">${presentationDotColors.map(([id,zh,en,color]) => `<button type="button" class="secondary presentation-dot-choice" data-presentation-dot-color="${id}" aria-pressed="${document.body.dataset.dotColor === id}" title="${esc(tr(zh,en))}" aria-label="${esc(tr(zh,en))}"><span class="presentation-dot-swatch ${id === 'multi' ? 'is-multicolor' : ''}" ${color ? `style="--swatch:${color}"` : ''} aria-hidden="true"></span>${id === 'multi' ? tr(zh,en) : ''}</button>`).join('')}<label class="presentation-dot-custom" data-selected="${document.body.dataset.dotColor === 'custom'}"><input type="color" value="${presentationDotCustom}" data-presentation-dot-custom aria-label="${tr('自定义点阵颜色','Custom dotted outline color')}"><span>${tr('自定义','Custom')}</span></label></div></div>`;
}
function presentationSettingsHtml() {
  return `<div class="setting-row"><div class="setting-title">${tr('额度样式','Quota style')}</div><div class="quota-display-choice" role="group" aria-label="${tr('额度样式','Quota style')}">${['bar','ring'].map(style => `<button type="button" class="secondary" data-presentation-style="${style}" aria-pressed="${document.body.dataset.quotaStyle === style}">${style === 'bar' ? tr('横条','Bars') : tr('圆环','Rings')}</button>`).join('')}</div></div>`+presentationDotColorSettings();
}
document.addEventListener('click', event => {
  const color = event.target.closest('[data-presentation-dot-color]');
  if (color) { presentationSetDotColor(color.dataset.presentationDotColor,true); return; }
  const choice = event.target.closest('[data-presentation-style]');
  if (!choice) return;
  document.body.dataset.quotaStyle = choice.dataset.presentationStyle;
  savePreference(presentationPreferencePrefix+'quotaStyle',choice.dataset.presentationStyle);
  renderServices(); document.querySelectorAll('[data-presentation-style]').forEach(button=>button.setAttribute('aria-pressed',String(button.dataset.presentationStyle===choice.dataset.presentationStyle)));
});
function presentationChangeCustomColor(event) {
  if (!event.target.matches('[data-presentation-dot-custom]') || !/^#[0-9a-f]{6}$/i.test(event.target.value)) return;
  presentationDotCustom = event.target.value;
  presentationSetDotColor('custom',true);
  savePreference(presentationDotCustomPreference,presentationDotCustom);
}
document.addEventListener('input',presentationChangeCustomColor);
document.addEventListener('change',presentationChangeCustomColor);
