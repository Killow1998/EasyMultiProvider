// Preview-only command guards and service demos; visuals are shared with EMP.
let shadowErrorDemoActive = new URLSearchParams(location.search).get('demo') === 'quota-error';
const shadowLiveServiceIcon=serviceActivityIcon;
serviceActivityIcon=function(kind,id,label,brand,cpa=false,error=null){
  if(shadowServicesDemoActive) error=shadowServiceDemoMode==='service-errors' ? {title:tr('服务连接失败 · 演示','Service connection failed · Demo'),message:tr('连接超时，请重试。此信息为演示数据。','Connection timed out. Try again. This is sample data.')} : null;
  if(shadowErrorDemoActive && kind==='account' && id==='@native') error={title:tr('额度刷新失败 · 演示','Quota refresh failed · Demo'),message:tr('连接超时，请点击刷新重试。','Connection timed out. Click Refresh to try again.')};
  return shadowLiveServiceIcon(kind,id,label,brand,cpa,error);
};
const shadowFill = fill;
fill = function() {
  shadowFill(); presentationSyncMotion();
  mountShadowServiceDemo();
  $('emp_version').textContent += tr(' · 影子',' · Shadow');
  $('emp_version').title = tr('修改只保存在影子页面','Changes are saved only in this shadow page');
  document.querySelector('[data-i18n="quit_emp"]').hidden = true;
  $('integration_toggle').disabled = true;
  $('integration_toggle').onclick = null;
  if (shadowErrorDemoActive) {
    let demo = document.getElementById('shadow_error_demo');
    if (!demo) {
      demo = document.createElement('div'); demo.id = 'shadow_error_demo'; demo.className = 'presentation-error-demo';
      document.body.append(demo);
    }
    demo.innerHTML = `<span>${tr('正在演示错误提示','Error preview active')}</span><button type="button" class="secondary">${tr('结束演示','End demo')}</button>`;
    demo.querySelector('button').onclick = () => {
      shadowErrorDemoActive = false; demo.remove();
      const url = new URL(location.href); url.searchParams.delete('demo'); history.replaceState(null,'',url);
      renderServices();
    };
    document.querySelector('.presentation-service-icon[data-activity-id="@native"]')?.focus({preventScroll:true});
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

installShadowServiceDemo();

// Event-driven integration snapshots must preserve the preview's disabled action.
const shadowRenderIntegration=renderIntegration;
renderIntegration=function(...args){
  const result=shadowRenderIntegration(...args);
  $('integration_toggle').disabled=true;
  return result;
};
