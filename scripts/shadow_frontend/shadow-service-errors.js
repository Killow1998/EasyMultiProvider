// Service artwork and error bubble lifecycle.
let shadowErrorDemoActive = new URLSearchParams(location.search).get('demo') === 'quota-error';
let shadowIconSequence = 0;
function shadowServiceIcon(kind, id, label, brand, cpa = false, error = null) {
  const template = document.createElement('template');
  template.innerHTML = activityIndicatorHtml(kind,id,label);
  const button = template.content.firstElementChild;
  button.className = 'service-brand shadow-service-icon';
  button.dataset.brand = brand;
  button.innerHTML = SERVICE_BRAND_ICONS[brand] || SERVICE_BRAND_ICONS.custom;
  button.firstElementChild.classList.add('shadow-icon-mono');
  if (SHADOW_BRAND_COLOR_ICONS[brand]) {
    template.innerHTML = SHADOW_BRAND_COLOR_ICONS[brand];
    const colored = template.content.firstElementChild;
    if (brand === 'codex') {
      colored.querySelector('path[fill="#fff"][d^="M19.503 0H4.496"]')?.remove();
      colored.setAttribute('viewBox','3 3 18 18');
    }
    const prefix = `shadow-brand-${++shadowIconSequence}-`;
    const ids = new Map([...colored.querySelectorAll('[id]')].map(node => [node.id,prefix+node.id]));
    for (const node of [colored,...colored.querySelectorAll('*')]) {
      for (const attr of [...node.attributes]) {
        if (attr.name === 'id') node.id = ids.get(attr.value);
        else node.setAttribute(attr.name,attr.value.replace(/url\(#([^)]*)\)/g,(_,id) => `url(#${ids.get(id) || id})`));
      }
    }
    colored.classList.add('shadow-icon-color');
    button.append(colored);
  }
  const halo=document.createElement('span'); halo.className='shadow-activity-halo'; halo.dataset.shadowMotion='halo'; halo.setAttribute('aria-hidden','true'); button.append(halo);
  if (cpa) button.innerHTML += '<span class="service-cpa-badge">CPA</span>';
  if (shadowServicesDemoActive) error = shadowServiceDemoMode === 'service-errors' ? {title:tr('服务连接失败 · 演示','Service connection failed · Demo'),message:tr('连接超时，请重试。此信息为演示数据。','Connection timed out. Try again. This is sample data.')} : null;
  if (shadowErrorDemoActive && kind === 'account' && id === '@native') error = {title:tr('额度刷新失败 · 演示','Quota refresh failed · Demo'),message:tr('连接超时，请点击刷新重试。','Connection timed out. Click Refresh to try again.')};
  if (error) {
    button.dataset.errorTitle = error.title;
    button.dataset.errorMessage = String(error.message);
    const badge = document.createElement('span');
    badge.className = 'shadow-service-error-badge'; badge.setAttribute('aria-hidden','true'); badge.textContent = '!';
    button.append(badge);
  }
  return button.outerHTML;
}
function shadowUpdateServiceErrorLabels() {
  for (const button of document.querySelectorAll('.shadow-service-icon[data-error-message]')) {
    button.removeAttribute('title');
    button.setAttribute('aria-label',activityDescription(button.dataset.activityLabel,activitySummaryForIndicator(button))+'; '+button.dataset.errorTitle+': '+button.dataset.errorMessage);
  }
}
// One body-level bubble avoids clipping in the horizontally scrollable service list.
function installShadowServiceErrors() {
  const bubble = document.createElement('div');
  bubble.id = 'shadow-service-error'; bubble.className = 'shadow-service-error';
  bubble.setAttribute('role','tooltip'); bubble.hidden = true;
  const heading = document.createElement('strong'), message = document.createElement('span');
  bubble.append(heading,message); document.body.append(bubble);
  let target = null, dismiss = null;
  const selector = '.shadow-service-icon[data-error-message]';
  function hide() {
    clearTimeout(dismiss); target?.removeAttribute('aria-describedby'); target = null; bubble.hidden = true;
  }
  function place() {
    if (!target?.isConnected) return hide();
    const rect = target.getBoundingClientRect();
    if (rect.bottom < 0 || rect.top > innerHeight || rect.right < 0 || rect.left > innerWidth) return hide();
    const box = bubble.getBoundingClientRect();
    bubble.style.left = Math.max(8,Math.min(rect.left,innerWidth-box.width-8))+'px';
    bubble.style.top = Math.max(8,Math.min(rect.bottom+10,innerHeight-box.height-8))+'px';
  }
  function show(button) {
    clearTimeout(dismiss);
    if (!button) return;
    target?.removeAttribute('aria-describedby'); target = button;
    heading.textContent = button.dataset.errorTitle; message.textContent = button.dataset.errorMessage;
    button.setAttribute('aria-describedby',bubble.id); bubble.hidden = false; place();
  }
  function leave() {
    dismiss = setTimeout(() => {
      if (target?.matches(':hover,:focus-visible') || bubble.matches(':hover')) return;
      hide();
    },100);
  }
  document.addEventListener('pointerover',event => { const button=event.target.closest(selector); if(button) show(button); else if(bubble.contains(event.target)) clearTimeout(dismiss); });
  document.addEventListener('pointerout',event => { if(event.target.closest(selector) || bubble.contains(event.target)) leave(); });
  document.addEventListener('focusin',event => { const button=event.target.closest(selector); if(button) show(button); else hide(); });
  document.addEventListener('focusout',leave);
  document.addEventListener('keydown',event => { if(event.key==='Escape') hide(); });
  document.addEventListener('click',event => { if(event.target.closest(selector)) hide(); });
  window.addEventListener('resize',place); window.addEventListener('scroll',place,true);
  window.addEventListener('blur',hide);
  document.addEventListener('visibilitychange',() => {if(document.hidden) hide();});
  new MutationObserver(() => {
    if(target && (!target.isConnected || !target.hasAttribute('data-error-message')))hide();
    else if(target)show(target);
  }).observe($('services'),{childList:true,subtree:true,attributes:true,attributeFilter:['data-error-message','data-error-title']});
  const update = updateActivityDots;
  updateActivityDots = function() {
    update();
    shadowUpdateServiceErrorLabels();
  };
}
