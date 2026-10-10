// Shadow layout and settings; data writes and error presentation have their own owners.
function resetCountdownText(value) {
  const timestamp = resetTimestampMillis(value);
  if (!timestamp) return '';
  const seconds = Math.max(0,Math.floor((timestamp-Date.now())/1000));
  if (seconds >= 86400) return Math.floor(seconds/86400)+'d'+Math.floor(seconds%86400/3600)+'h';
  if (seconds >= 3600) return Math.floor(seconds/3600)+'h'+Math.floor(seconds%3600/60)+'m';
  return Math.floor(seconds/60)+'m'+seconds%60+'s';
}
const shadowOriginalQuotaMeters = quotaMetersHtml;
// A named alias retains the original bar implementation without replacing its semantics.
function shadowQuotaMeters(account, busy = false, animate = false) {
  if (document.body.dataset.quotaStyle === 'bar') return shadowOriginalQuotaMeters(account,busy,animate);
  let buckets = quotaDisplayBuckets(account);
  if (!buckets.length && account.quota_pending) buckets = [300,10080].map(windowMinutes => ({windowMinutes,pending:true}));
  if (!buckets.length) return `<span class="muted">${tr('未刷新','Not refreshed')}</span>`;
  const arc = 'M 76.309 67.223 A 37 37 0 1 1 76.309 24.777';
  return `<div class="shadow-quota-pair">${buckets.map((item,index) => {
    const pending = item.pending || (account.quota_pending && item.unreported);
    const unlimited = item.unreported && !pending && item.windowMinutes === 300;
    const value = unlimited ? 233 : item.unreported || pending ? null : quotaDisplay.value(item.remaining);
    const color = item.remaining <= 20 ? '#be4949' : item.remaining <= 50 ? '#ab711d' : '#2f884d';
    const duration = compactWindowText(item.windowMinutes);
    const label = item.limitId && !['codex','claude'].includes(item.limitId) ? quotaLimitLabel(item.limitId)+' · '+duration : duration;
    const mirror = index % 2 ? 'translate(92 0) scale(-1 1)' : '';
    const mask = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 92 92"><g transform="${mirror}"><path d="${arc}" fill="none" stroke="white" stroke-width="5" stroke-linecap="round"/></g></svg>`;
    const time = unlimited ? '1m111s' : resetCountdownText(item.resetsAt);
    return `<div class="shadow-ring-meter"><div class="shadow-ring" style="--quota-color:${color};--fill:${Math.min(100,value || 0)};--arc-mask:url('data:image/svg+xml,${encodeURIComponent(mask)}')" role="img" aria-label="${esc(label+' '+(value === null ? '—' : value+'%')+' '+time)}"><svg viewBox="0 0 92 92" aria-hidden="true"><g transform="${mirror}"><path class="rail" d="${arc}"/><path class="level" d="${arc}" pathLength="100" ${unlimited ? 'opacity="0"' : ''}/></g></svg>${unlimited ? '<div class="shadow-flow-ring"><span data-shadow-motion="ring"></span></div>' : ''}<strong class="shadow-ring-value">${value === null ? '—' : esc(value)+'%'}</strong></div><div class="shadow-ring-meta"><span class="shadow-ring-window">${esc(label)}</span><span class="shadow-ring-time">${uiIcon('refresh')}<span ${!unlimited && item.resetsAt ? `data-reset-countdown="${esc(item.resetsAt)}"` : ''}>${esc(time || '—')}</span></span></div></div>`;
  }).join('')}</div>`;
}
document.body.dataset.quotaStyle = storedPreference('emp.shadow.quotaStyle','ring');
const shadowDotColors = [
  ['multi','多彩','Multicolor',''],['blue','蓝色','Blue','#487de8'],
  ['purple','紫色','Purple','#9473c6'],['teal','青色','Teal','#299c9f'],
  ['amber','琥珀','Amber','#bc7b25'],['rose','玫红','Rose','#d16b8c'],
];
const shadowDotPreference = 'emp.shadow.dotColor';
const shadowDotCustomPreference = 'emp.shadow.dotCustom';
let shadowDotCustom = storedPreference(shadowDotCustomPreference,'#487de8');
if (!/^#[0-9a-f]{6}$/i.test(shadowDotCustom)) shadowDotCustom = '#487de8';
function shadowSetDotColor(choice, save = false) {
  const preset = shadowDotColors.find(row => row[0] === choice);
  if (!preset && choice !== 'custom') choice = 'multi';
  document.body.dataset.dotColor = choice;
  const color = choice === 'custom' ? shadowDotCustom : preset?.[3];
  for (const name of ['--shadow-dot-color','--shadow-dot-secondary','--shadow-dot-third']) {
    if (color) document.body.style.setProperty(name,color);
    else document.body.style.removeProperty(name);
  }
  if (save) savePreference(shadowDotPreference,choice);
  document.querySelectorAll('[data-shadow-dot-color]').forEach(button => button.setAttribute('aria-pressed',String(button.dataset.shadowDotColor === choice)));
  const custom = document.querySelector('.shadow-dot-custom');
  if (custom) custom.dataset.selected = String(choice === 'custom');
  window.dispatchEvent(new Event('shadow-dot-color-change'));
}
shadowSetDotColor(storedPreference(shadowDotPreference,'multi'));
function shadowDotColorSettings() {
  return `<div class="setting-row"><div class="setting-title">${tr('点阵颜色','Dotted outline color')}</div><div class="shadow-dot-choices" role="group" aria-label="${tr('点阵颜色','Dotted outline color')}">${shadowDotColors.map(([id,zh,en,color]) => `<button type="button" class="secondary shadow-dot-choice" data-shadow-dot-color="${id}" aria-pressed="${document.body.dataset.dotColor === id}" title="${esc(tr(zh,en))}" aria-label="${esc(tr(zh,en))}"><span class="shadow-dot-swatch ${id === 'multi' ? 'is-multicolor' : ''}" ${color ? `style="--swatch:${color}"` : ''} aria-hidden="true"></span>${id === 'multi' ? tr(zh,en) : ''}</button>`).join('')}<label class="shadow-dot-custom" data-selected="${document.body.dataset.dotColor === 'custom'}"><input type="color" value="${shadowDotCustom}" data-shadow-dot-custom aria-label="${tr('自定义点阵颜色','Custom dotted outline color')}"><span>${tr('自定义','Custom')}</span></label></div></div>`;
}
const shadowSettingsOpen = openSettings;
openSettings = function() {
  shadowSettingsOpen();
  $('modal_body').querySelector('.settings-list').insertAdjacentHTML('afterbegin', shadowDotColorSettings());
  $('modal_body').querySelector('.settings-list').insertAdjacentHTML('afterbegin', `<div class="setting-row"><div class="setting-title">${tr('额度样式','Quota style')}</div><div class="quota-display-choice">${['bar','ring'].map(style => `<button type="button" class="secondary" data-shadow-style="${style}" aria-pressed="${document.body.dataset.quotaStyle === style}">${style === 'bar' ? tr('横条','Bars') : tr('圆环','Rings')}</button>`).join('')}</div></div>`);
};
document.addEventListener('click', event => {
  const color = event.target.closest('[data-shadow-dot-color]');
  if (color) { shadowSetDotColor(color.dataset.shadowDotColor,true); return; }
  const choice = event.target.closest('[data-shadow-style]');
  if (!choice) return;
  document.body.dataset.quotaStyle = choice.dataset.shadowStyle;
  savePreference('emp.shadow.quotaStyle',choice.dataset.shadowStyle);
  renderServices(); openSettings();
});
function shadowChangeCustomColor(event) {
  if (!event.target.matches('[data-shadow-dot-custom]') || !/^#[0-9a-f]{6}$/i.test(event.target.value)) return;
  shadowDotCustom = event.target.value;
  shadowSetDotColor('custom',true);
  savePreference(shadowDotCustomPreference,shadowDotCustom);
}
document.addEventListener('input',shadowChangeCustomColor);
document.addEventListener('change',shadowChangeCustomColor);
const shadowFill = fill;
fill = function() {
  shadowFill(); shadowSyncMotion();
  mountShadowServiceDemo();
  $('emp_version').textContent += tr(' · 影子',' · Shadow');
  $('emp_version').title = tr('修改只保存在影子页面','Changes are saved only in this shadow page');
  document.querySelector('[data-i18n="quit_emp"]').hidden = true;
  $('integration_toggle').disabled = true;
  $('integration_toggle').onclick = null;
  if (shadowErrorDemoActive) {
    let demo = document.getElementById('shadow_error_demo');
    if (!demo) {
      demo = document.createElement('div'); demo.id = 'shadow_error_demo'; demo.className = 'shadow-error-demo';
      document.body.append(demo);
    }
    demo.innerHTML = `<span>${tr('正在演示错误提示','Error preview active')}</span><button type="button" class="secondary">${tr('结束演示','End demo')}</button>`;
    demo.querySelector('button').onclick = () => {
      shadowErrorDemoActive = false; demo.remove();
      const url = new URL(location.href); url.searchParams.delete('demo'); history.replaceState(null,'',url);
      renderServices();
    };
    document.querySelector('.shadow-service-icon[data-activity-id="@native"]')?.focus({preventScroll:true});
  }
};
confirmIntegrationAction = function() {};
quitEmp = function() {};
reportWebClientPhase = function() {};
const shadowLoadIntegration = loadIntegration;
loadIntegration = async function() {
  try { return await shadowLoadIntegration(); }
  finally { $('integration_toggle').disabled = true; }
};
const shadowSavedNotice = notice;
notice = function(message, error = false) {
  shadowSavedNotice(!error && /已保存|已显示|已隐藏|已移除|已删除|saved|visible|hidden|removed|deleted/i.test(message) ? tr('影子预览：','Shadow preview: ') + message : message,error);
};
const shadowDrawLogo=drawLogo;
drawLogo=function(){shadowDrawLogo();enhanceShadowLogo($('emp_logo'));};
refreshResetCountdowns=function(){
  if(document.hidden)return;
  for(const element of document.querySelectorAll('[data-reset-countdown]')) {
    if(element.closest('.shadow-motion-paused'))continue;
    const text=resetCountdownText(element.dataset.resetCountdown);
    if(element.textContent!==text)element.textContent=text;
  }
};
const shadowSyncMotion=installShadowMotion(refreshResetCountdowns);

installShadowInteractions();
installShadowServiceErrors();
installShadowServiceDemo();
installShadowRequestErrors();

translations['zh-CN'].statistics='统计'; translations.en.statistics='Statistics';
const shadowStats=createShadowStats({api,$,tr,esc,getState:()=>state,getLanguage:()=>language,openModal,callReports,periodPickerHtml,registerPeriodPicker,periodPickers,refreshPeriod,loadDiagnostics,loadSupportReport});
const shadowUsageRefresh=usageReport.refresh;
usageReport.refresh=function(...args){if (!shadowStats.refresh()) return shadowUsageRefresh(...args);};
