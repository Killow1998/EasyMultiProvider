// Developer preview entry; production owns the routing dialog and all labels.
window.EMP_ROUTING_DEMO = async () => {
  if (shadowServicesDemoActive) { location.assign('/?routing=1'); return; }
  if (!(state.accounts || []).some(account => account.id === 'routing-demo')) {
    await api('/api/accounts/import', {method:'POST', body:JSON.stringify({
      id:'routing-demo', prefix:'routing-demo', name:tr('图片示例账号','Image sample account'),
      auth_json:{tokens:{account_id:'routing-demo-identity',access_token:'synthetic-routing-preview-only'}}
    })});
    state = await api('/api/config'); fill();
  }
  routingSettings.open({back:openSettings});
};
const routingButton = document.createElement('button');
routingButton.type = 'button'; routingButton.id = 'shadow_routing_button'; routingButton.className = 'secondary';
routingButton.onclick = () => window.EMP_ROUTING_DEMO().catch(error => notice(error.message,true));
demoBanner.after(routingButton);
const routingFill = fill;
fill = function(...args) {
  const result = routingFill(...args);
  routingButton.textContent = tr('路由设置 · 本次新增','Routing settings · New');
  return result;
};
routingButton.textContent = tr('路由设置 · 本次新增','Routing settings · New');
const routingReviewChoices = window.EMP_REVIEW_CHOICES;
window.EMP_REVIEW_CHOICES = () => reviewChoice('collaboration','跨服务子代理','Cross-service subagents',
  '关闭、按需恢复、打开；问号内查看模式比较',
  'Off, on-demand recovery and On; mode comparison under the question mark') + reviewChoice('routing','路由设置','Routing settings',
  '自动审查与图片路由；跟随对话、指定账号、自动选择',
  'Auto Review and image routing; follow, specific account, automatic') + routingReviewChoices();
const routingReviewAction = window.EMP_REVIEW_ACTION;
window.EMP_REVIEW_ACTION = action => {
  if (action === 'collaboration') { openSettings(); return true; }
  if (action !== 'routing') return routingReviewAction(action);
  routingButton.click(); return true;
};
if (new URLSearchParams(location.search).has('routing')) {
  const ready = setInterval(() => {
    if (state?.native_account) { clearInterval(ready); routingButton.click(); }
  },100);
  setTimeout(() => clearInterval(ready),10000);
}
if (new URLSearchParams(location.search).has('collaboration')) {
  const ready = setInterval(() => {
    if (state?.native_account) { clearInterval(ready); openSettings(); }
  },100);
  setTimeout(() => clearInterval(ready),10000);
}
