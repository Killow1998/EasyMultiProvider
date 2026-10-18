// Replace this list for each review round; shipped demos belong in Demos below.
// Production controllers own their dialogs and text.
const reviewRows=['collaboration','metrics'];
const previewScenarios={
  review:{title:['本次改动','This update'],detail:['查看当前待检阅的修改','Review the current changes'],open:openReview},
  collaboration:{title:['子代理与订阅性能对比','Subagent and subscription comparisons'],detail:['模式建议、速度与耗时对比，以及匿名的导入订阅／原生直连实测。','Mode recommendations, speed/time comparisons and anonymous imported/native direct measurements.'],open:openCollaborationDemo},
  metrics:{title:['统计速度与服务配色','Statistics speed and service colors'],detail:['合成示例：生成 80 TPS、全程 20 TPS；按服务配色可区分三个示例服务。','Synthetic example: 80 generation TPS, 20 end-to-end TPS; service colors distinguish three sample services.'],open:()=>statistics.open()},
  routing:{title:['路由设置','Routing settings'],detail:['自动审查与图片路由；跟随对话、指定账号、自动选择','Auto Review and image routing; follow, specific account, automatic'],open:openRoutingDemo},
  recovery:{title:['key 异常恢复','Key recovery'],detail:['实际恢复页，模拟清除重建和退出','Actual recovery page with simulated reset and exit'],open:()=>location.assign('/recovery')},
  lifecycle:{title:['恢复原生 / 退出','Restore Native / Quit'],detail:['查看两个独立操作','Review the two separate actions'],open:openLifecycleDemo},
  removal:{title:['移除账号','Remove account'],detail:['可重复创建和移除示例账号','Create and remove a disposable sample account'],open:openRemovalDemo},
  historical:{title:['已有设置冲突','Existing preference conflicts'],detail:['模型设置中的历史重复账号选择','Resolve existing duplicate preferences in model settings'],open:openHistoricalDemo}
};
function reviewChoice(id){
  const {title,detail}=previewScenarios[id];
  return `<button type="button" class="choice" data-shadow-demo="${id}"><strong>${tr(...title)}</strong><small>${tr(...detail)}</small></button>`;
}
window.EMP_REVIEW_CHOICES=()=>['review','recovery','lifecycle','removal','historical','routing'].map(reviewChoice).join('');
function openReview(){
  openModal(tr(...previewScenarios.review.title),`<div class="choice-grid">${reviewRows.map(reviewChoice).join('')}</div>`,'',null);
}
window.EMP_REVIEW_ACTION=action=>{
  if(!Object.hasOwn(previewScenarios,action))return false;
  if(shadowServicesDemoActive&&!['review','recovery'].includes(action)){
    location.assign('/?reviewAction='+encodeURIComponent(action));return true;
  }
  void Promise.resolve().then(previewScenarios[action].open).catch(error=>notice(error.message,true));
  return true;
};
const reviewButton=document.createElement('button');reviewButton.type='button';reviewButton.id='shadow_review_button';reviewButton.className='secondary';reviewButton.textContent=tr('本次改动','This update');reviewButton.onclick=openReview;demoBanner.after(reviewButton);
const reviewParameters=new URLSearchParams(location.search);
let previewStartupAction=['import','routing','collaboration','review'].find(action=>reviewParameters.has(action))||reviewParameters.get('reviewAction');
function syncPreviewReview(){
  reviewButton.textContent=tr(...previewScenarios.review.title);
  if(!state?.native_account||!previewStartupAction)return;
  const action=previewStartupAction;previewStartupAction=null;
  if(action==='import'){
    const url=new URL(location.href);url.searchParams.delete('import');history.replaceState(null,'',url);
    window.EMP_IMPORT_DEMO();
  }else window.EMP_REVIEW_ACTION(action);
}
