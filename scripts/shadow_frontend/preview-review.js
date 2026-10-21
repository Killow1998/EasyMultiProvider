// Replace this list for each review round; shipped demos belong in Demos below.
// Production controllers own their dialogs and text.
const reviewRows=['bills','connection','metrics','migration','lifecycle'];
const previewScenarios={
  review:{title:['本次改动','This update'],detail:['查看当前待检阅的修改','Review the current changes'],open:openReview},
  bills:{title:['会话账单','Session bills'],detail:['合成示例：按名称搜索会话，展开各模型用量、缓存与 API 等价估算；未计价和缺失用量分别标示。','Synthetic examples: search sessions and expand model usage, cache and API-equivalent estimates, with unpriced and missing usage marked.'],open:()=>{statistics.open();document.querySelector('[data-stats-view="sessions"]').click();}},
  migration:{title:['迁移导入预览','Migration import preview'],detail:['清晰的等待提示；预览超时后保留选择，可直接重新预览。','Clear progress messages; keep selections and retry after a preview timeout.'],open:()=>window.EMP_IMPORT_DEMO()},
  connection:{title:['服务接入：Mercury 3','Service setup: Mercury 3'],detail:['新增 Inception / Mercury 预设；填写 Key，连接参数按需展开，保存只影响演示数据。','New Inception / Mercury preset: enter a key and expand connection details when needed. Saves affect demo data only.'],open:()=>chooseOfficialProvider('inception',null,{back:openReview})},
  collaboration:{title:['子代理与订阅性能对比','Subagent and subscription comparisons'],detail:['模式建议、速度与耗时对比，以及匿名的导入订阅／原生直连实测。','Mode recommendations, speed/time comparisons and anonymous imported/native direct measurements.'],open:openCollaborationDemo},
  metrics:{title:['调用结果与性能统计','Call outcomes and performance'],detail:['合成示例：重试完成、上游等待建议和输出中断；保留速度与工具耗时示例。','Synthetic examples: successful retries, upstream wait advice and interrupted output, alongside speed and tool timing.'],open:()=>statistics.open()},
  routing:{title:['路由设置','Routing settings'],detail:['自动审查与图片路由；跟随对话、指定账号、自动选择','Auto Review and image routing; follow, specific account, automatic'],open:openRoutingDemo},
  recovery:{title:['key 异常恢复','Key recovery'],detail:['实际恢复页，模拟清除重建和退出','Actual recovery page with simulated reset and exit'],open:()=>location.assign('/recovery')},
  lifecycle:{title:['恢复原生 / 退出','Restore Native / Quit'],detail:['合成示例：闲置历史转换，使用中的历史保留，并可在结束会话后继续转换。恢复与退出仍为两个操作。','Synthetic example: convert idle history, preserve busy sessions, and continue conversion once they close. Restore and Quit remain separate.'],open:openLifecycleDemo},
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
