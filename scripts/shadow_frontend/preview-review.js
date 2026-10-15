// Review entry points only. Production controllers own their dialogs and text.
const reviewRows=[
  ['迁移导入导航','Migration navigation','选择文件 → 打开迁移包 → 预览；返回保留文件与密码。','Choose file → open package → preview; Back retains the file and password.','import'],
  ['同一账号合并展示','One service per account','确认是同一账号后显示为一个服务，名称相同不代表同一账号。','Verified identical accounts share one service; matching names alone do not merge.','import'],
  ['显示与上下文冲突选择','Visibility and context choices','在导入预览中选择保留本地或采用导入值，一次提交。','Choose local or incoming preferences before one import commit.','import'],
  ['移除进度与防重复点击','Removal progress','移除时立即显示进度，完成后更新列表。','Show progress immediately, block duplicate clicks, then update the list.','removal'],
  ['恢复原生与退出分开','Restore Native and Quit','恢复后继续运行，可再次启用；退出另有确认。','Restore keeps EMP running and allows re-enable; Quit has its own confirmation.','lifecycle'],
  ['新增 key 异常恢复页','New key recovery page','一键清除并重新配置，备份可选；中断也自动处理。','Clear and set up again, with optional backup and automatic interrupted-reset handling.','recovery']
];
const reviewChoice=(id,zh,en,detailZh,detailEn)=>`<button type="button" class="choice" data-shadow-demo="${id}"><strong>${tr(zh,en)}</strong><small>${tr(detailZh,detailEn)}</small></button>`;
window.EMP_REVIEW_CHOICES=()=>reviewChoice('review','本次改动 · 10 组','This update · 10 groups','6 组前端操作，4 组后台调整','6 frontend flows and 4 backend changes')+reviewChoice('recovery','key 异常恢复','Key recovery','实际恢复页，模拟清除重建和退出','Actual recovery page with simulated reset and exit')+reviewChoice('lifecycle','恢复原生 / 退出','Restore Native / Quit','查看两个独立操作','Review the two separate actions')+reviewChoice('removal','移除账号','Remove account','可重复创建和移除示例账号','Create and remove a disposable sample account')+reviewChoice('historical','已有设置冲突','Existing preference conflicts','模型设置中的历史重复账号选择','Resolve existing duplicate preferences in model settings');
function openReview(){
  openModal(tr('本次改动 · 相对 v0.13.1','Changes since v0.13.1'),`<p>${tr('10 组功能调整，其中 6 组涉及前端：5 组已有操作调整，1 个新增恢复页面。agy 不计入本候选版。','10 feature groups, including 6 frontend flows: 5 revised flows and 1 new recovery page. agy is separate.')}</p><div class="choice-grid">${reviewRows.map(([zh,en,dzh,den,action],i)=>reviewChoice(action,`${i+1}. ${zh}`,`${i+1}. ${en}`,dzh,den)).join('')}</div><h3>${tr('其余 4 组后台调整','4 other backend changes')}</h3><ul>${[
    tr('Native 额度读取跟随当前登录身份','Native quota follows the current signed-in identity'),
    tr('启动错误分类与诊断记录','Startup error classification and diagnostics'),
    tr('模型目录刷新与缓存归属修复','Model catalog refresh and cache ownership'),
    tr('Windows 本机代理兼容与地址迁移','Windows loopback proxy compatibility and endpoint migration')
  ].map(x=>`<li>${x}</li>`).join('')}</ul><p class="muted">${tr('后台效果及 Windows 退出恢复需实际验收，Shadow 不冒充验证结果。下方已有服务动画与错误画廊继续保留，不计作本次新增。','Backend behavior and Windows exit restoration need runtime acceptance. Shadow is not evidence of that acceptance. Existing activity/error galleries remain available and are not new features.')}</p>`,'',null);
}
let lifecycleDemo=false;
let lifecycleState='emp_applied';
const lifecycleInfo=()=>({configuration:{state:lifecycleState},runtime:{state:'not_checked'}});
const guardedPreviewFetch=window.fetch.bind(window);
window.fetch=async(path,options={})=>{
  const route=String(path).split('?')[0];
  // These actions never reach even the disposable backend during UI review.
  if(route==='/api/quit'||route.startsWith('/api/integration')){
    if(route.endsWith('/restore'))lifecycleState='native';
    if(route.endsWith('/enable'))lifecycleState='emp_applied';
    return new Response(JSON.stringify(route==='/api/quit'?{ok:true}:lifecycleDemo?lifecycleInfo():{configuration:{state:'native'},runtime:{state:'not_checked'}}),{headers:{'Content-Type':'application/json'}});
  }
  if(route.startsWith('/api/accounts/')&&options.method==='DELETE')await new Promise(resolve=>setTimeout(resolve,700));
  return guardedPreviewFetch(path,options);
};
const reviewRenderIntegration=renderIntegration;
renderIntegration=function(info){reviewRenderIntegration(lifecycleDemo?lifecycleInfo():info);if(lifecycleDemo){$('integration_toggle').disabled=false;document.querySelector('[data-i18n="quit_emp"]').hidden=false;}};
window.EMP_REVIEW_ACTION=action=>{
  if(!['review','recovery','lifecycle','removal','historical'].includes(action))return false;
  if(action==='recovery'){location.assign('/recovery');return true;}
  if(action==='review'){openReview();return true;}
  if(shadowServicesDemoActive){location.assign('/?reviewAction='+action);return true;}
  if(action==='lifecycle'){
    lifecycleDemo=true;lifecycleState='emp_applied';closeModal();renderIntegration(lifecycleInfo());
    demoBanner.textContent=tr('生命周期演示 · 恢复、启用和退出均为模拟，不会停止 EMP。','Lifecycle demo · Restore, Enable and Quit are simulated; EMP will not stop.');
    confirmIntegrationAction('restore');return true;
  }
  closeModal();
  void (async()=>{
    if(action==='historical'){
      await api('/api/accounts/import',{method:'POST',body:JSON.stringify({id:'review-history',prefix:'review-history',name:'Earlier imported settings',auth_json:{tokens:{account_id:'demo-fixture-account',access_token:'synthetic-history-preview-only'}}})});
      const candidate=await api('/api/config');
      candidate.native_hidden_models=[];candidate.native_model_context_windows={'demo-model':400000};
      for(const account of candidate.accounts.filter(a=>a.id==='review-history')){
        account.hidden_models=['demo-model'];account.model_context_windows={'demo-model':467368};
      }
      await api('/api/config',{method:'POST',body:JSON.stringify(candidate)});
      state=await api('/api/config');fill();editAccount('@native');return;
    }
    await api('/api/accounts/import',{method:'POST',body:JSON.stringify({id:'review-removal',prefix:'review-removal',name:'Removal sample',auth_json:{tokens:{account_id:'review-removal-identity',access_token:'synthetic-preview-only'}}})});
    state=await api('/api/config');fill();await openAccountDetails('review-removal');
    notice(tr('示例账号可移除；演示增加 0.7 秒等待以查看进度。','Remove this sample account. A 0.7 second demo delay makes progress visible.'));
  })().catch(error=>notice(error.message,true));
  return true;
};
const reviewButton=document.createElement('button');reviewButton.type='button';reviewButton.id='shadow_review_button';reviewButton.className='secondary';reviewButton.textContent='本次改动 / Changes';reviewButton.onclick=openReview;demoBanner.after(reviewButton);
const reviewParameters=new URLSearchParams(location.search);
if(reviewParameters.has('review'))openReview();
if(reviewParameters.has('reviewAction')){
  const requested=reviewParameters.get('reviewAction');
  const ready=setInterval(()=>{if(state?.native_account){clearInterval(ready);window.EMP_REVIEW_ACTION(requested);}},100);
  setTimeout(()=>clearInterval(ready),10000);
}
