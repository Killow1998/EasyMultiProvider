// Replace this list for each review round; shipped demos belong in Demos below.
// Production controllers own their dialogs and text.
const reviewRows=['lifecycle'];
const realShadow=window.EMP_REAL_SHADOW===true;
const syntheticScenarios=['migration','connection','collaboration','metrics','recovery','lifecycle','removal','historical'];
const previewScenarios={
  review:{title:['本次改动','This update'],detail:['查看当前待检阅的修改','Review the current changes'],open:openReview},
  settings:{title:['设置分组','Settings groups'],detail:['显示、通用服务、外部服务与原生服务分组；后台压缩和路由集中在原生服务中。','Grouped display, general, external and native service settings, with background compaction and routing under native services.'],open:()=>openSettings()},
  bills:{title:['会话账单归属','Conversation billing'],detail:['合成示例：自动审查、子代理与 Sidechat 合并到主会话；展开可查看模型及任务用量，归属不明的用量保留在总计中。','Synthetic examples: Auto Review, subagents and Sidechat belong to the main conversation. Expand model and task usage; unassigned usage remains in totals.'],open:()=>{statistics.open();document.querySelector('[data-stats-view="sessions"]').click();}},
  migration:{title:['迁移导入预览','Migration import preview'],detail:['清晰的等待提示；预览超时后保留选择，可直接重新预览。','Clear progress messages; keep selections and retry after a preview timeout.'],open:()=>window.EMP_IMPORT_DEMO()},
  connection:{title:['服务接入：Mercury 3','Service setup: Mercury 3'],detail:['新增 Inception / Mercury 预设；填写 Key，连接参数按需展开，保存只影响演示数据。','New Inception / Mercury preset: enter a key and expand connection details when needed. Saves affect demo data only.'],open:()=>chooseOfficialProvider('inception',null,{back:openReview})},
  collaboration:{title:['子代理与订阅性能对比','Subagent and subscription comparisons'],detail:['模式建议、速度与耗时对比，以及匿名的导入订阅／原生直连实测。','Mode recommendations, speed/time comparisons and anonymous imported/native direct measurements.'],open:()=>openCollaborationDemo()},
  metrics:{title:['调用结果与性能统计','Call outcomes and performance'],detail:['合成示例：重试完成、上游等待建议和输出中断；保留速度与工具耗时示例。','Synthetic examples: successful retries, upstream wait advice and interrupted output, alongside speed and tool timing.'],open:()=>statistics.open()},
  context:{title:['上下文与压缩说明','Context and compaction'],detail:['在现有模型设置的问号中查看有效窗口、自动压缩阈值和调整方式。','Use the existing model settings help to inspect usable context, compaction triggers and adjustments.'],open:()=>editAccount(state.native_account.id,{back:openReview})},
  routing:{title:['路由设置','Routing settings'],detail:['自动审查、图片和记忆整理；记忆只选 GPT 模型，账号跟随会话。','Auto Review, images and memories; select a GPT memory model while its account follows the conversation.'],open:()=>realShadow?routingSettings.open({back:openSettings}):openRoutingDemo()},
  tray:{title:['macOS 菜单栏图标','macOS menu-bar icon'],detail:['纯色透明图标，预览浅色和深色背景；实际菜单交互仍由系统提供。','A transparent monochrome icon on light and dark backgrounds; the OS provides the actual menu interaction.'],open:openTrayPreview},
  recovery:{title:['key 异常恢复','Key recovery'],detail:['实际恢复页，模拟清除重建和退出','Actual recovery page with simulated reset and exit'],open:()=>location.assign('/recovery')},
  lifecycle:{title:['恢复原生 / 退出','Restore Native / Quit'],detail:['合成示例：区分已确认待转换与未检查的历史。恢复时共用目录索引，系统菜单显示操作进度；恢复与退出仍为两个操作。','Synthetic example: distinguish confirmed pending conversions from unchecked history. Restore shares one directory inventory; the tray shows action progress. Restore and Quit remain separate.'],open:()=>openLifecycleDemo()},
  removal:{title:['移除账号','Remove account'],detail:['可重复创建和移除示例账号','Create and remove a disposable sample account'],open:()=>openRemovalDemo()},
  historical:{title:['已有设置冲突','Existing preference conflicts'],detail:['模型设置中的历史重复账号选择','Resolve existing duplicate preferences in model settings'],open:()=>openHistoricalDemo()}
};
if(realShadow)previewScenarios.bills.detail=['候选后端计算真实账单快照；保留原始名称、缺失字段和会话关系，检查归属及用量。','The candidate backend calculates a real accounting snapshot, retaining original names, missing fields and conversation relationships.'];
function reviewChoice(id){
  const {title,detail}=previewScenarios[id];
  return `<button type="button" class="choice" data-shadow-demo="${id}"><strong>${tr(...title)}</strong><small>${tr(...detail)}</small></button>`;
}
window.EMP_REVIEW_CHOICES=()=> (window.EMP_REAL_SHADOW_URL?`<button type="button" class="choice" data-shadow-demo="real_shadow"><strong>${tr('返回真实资料 Shadow','Return to real-data Shadow')}</strong></button>`:'')+(realShadow?syntheticScenarios:['review','recovery','lifecycle','removal','historical']).map(reviewChoice).join('');
function openTrayPreview(){
  const icon='__TRAY_TEMPLATE_ICON__';
  const label=tr('模板图像预览；实际 macOS 图标由系统着色。','Template image preview; macOS supplies the actual icon color.');
  openModal(tr(...previewScenarios.tray.title),`<p>${label}</p><div style="display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:16px">${[['#f4f4f4','#202020'],['#252525','#fff']].map(([background,color])=>`<div style="height:96px;background:${background};color:${color};display:flex;align-items:center;justify-content:center;border-radius:6px"><span role="img" aria-label="EMP" style="width:32px;height:32px;background:currentColor;mask:url(${icon}) center/contain no-repeat"></span></div>`).join('')}</div>`,'',null,null,{back:openReview});
}
function openReview(){
  openModal(tr(...previewScenarios.review.title),`<div class="choice-grid">${reviewRows.map(reviewChoice).join('')}</div>`,'',null);
}
window.EMP_REVIEW_ACTION=action=>{
  if(action==='background')action='settings';
  if(action==='real_shadow'&&window.EMP_REAL_SHADOW_URL){location.assign(window.EMP_REAL_SHADOW_URL);return true;}
  if(realShadow && syntheticScenarios.includes(action)){
    void window.EMP_OPEN_SYNTHETIC(action);return true;
  }
  if(!Object.hasOwn(previewScenarios,action))return false;
  if(shadowServicesDemoActive&&!['review','recovery'].includes(action)){
    location.assign('/?reviewAction='+encodeURIComponent(action));return true;
  }
  void Promise.resolve().then(previewScenarios[action].open).catch(error=>notice(error.message,true));
  return true;
};
const reviewButton=document.createElement('button');reviewButton.type='button';reviewButton.id='shadow_review_button';reviewButton.className='secondary';reviewButton.textContent=tr('本次改动','This update');reviewButton.onclick=openReview;demoBanner.after(reviewButton);
const reviewParameters=new URLSearchParams(location.search);
let previewStartupAction=['import','settings','routing','collaboration','context','background','tray','review'].find(action=>reviewParameters.has(action))||reviewParameters.get('reviewAction');
function syncPreviewReview(){
  reviewButton.textContent=tr(...previewScenarios.review.title);
  if(!state?.native_account||!previewStartupAction)return;
  const action=previewStartupAction;previewStartupAction=null;
  if(action==='import'){
    const url=new URL(location.href);url.searchParams.delete('import');history.replaceState(null,'',url);
    if(realShadow)void window.EMP_OPEN_SYNTHETIC('import');else window.EMP_IMPORT_DEMO();
  }else window.EMP_REVIEW_ACTION(action);
}
