// Replace this list for each review round; shipped demos belong in Demos below.
// Production controllers own their dialogs and text.
const reviewRows=['tutorials','native_local_key'];
const realShadow=window.EMP_REAL_SHADOW===true;
const syntheticScenarios=['onboarding','lan','migration','connection','collaboration','metrics','recovery','lifecycle','removal','historical'];
const previewScenarios={
  review:{title:['本次改动','This update'],detail:['查看当前待检阅的修改','Review the current changes'],open:openReview},
  tutorials:{title:['使用教程','Tutorials'],detail:['更新教学动图与弹窗布局，隧道迁移同时展示发送端和接收端。','Updated tutorial animations and layout, including a side-by-side tunnel transfer.'],open:()=>tutorials.open({back:openReview})},
  native_local_key:{title:['Native 切换为 EMP Key','Switch Native to EMP key'],detail:['在账号信息中查看切换入口及两种账号处理方式。','Review the account-details entry and both ways to handle the subscription.'],open:()=>openAccountDetails(state.native_account.id,{back:openReview})},
  notifications:{title:['通知与信箱','Notifications and inbox'],detail:['顶部浮层与倒计时条；右下角信箱保留此浏览器最近 50 条通知，刷新后仍可查看。','Top notification with a countdown bar; the bottom-right inbox keeps the last 50 messages in this browser across reloads.'],open:()=>{closeModal();$('notification_mailbox').click();}},
  onboarding:{title:['Codex 登录与导入','Codex sign-in and import'],detail:['添加订阅只有装置码登录与 auth.json 两个入口；本地 Key 与原生登录仍在服务列表中。使用独立合成账号验收。','Add subscriptions through device-code sign-in or auth.json. Local key and native sign-in remain in the service list. Uses separate synthetic accounts.'],open:()=>openServiceChooser()},
  lan:{title:['数据迁移','Data migration'],detail:['导入、导出、隧道三个入口；窗口随内容展开，服务列表不再限制为三行，返回保留已填内容。','Three entries: Import, Export and Tunnel. Content determines dialog height; service lists expand fully and Back keeps your entries.'],open:()=>migration.open()},
  settings:{title:['说明与弹窗操作','Help and dialog controls'],detail:['问号说明直接交代条件和操作；滚动内容时，标题、返回、关闭与保存按钮保持可见。','Help states conditions and actions directly. Titles, Back, Close and Save stay visible while the content scrolls.'],open:()=>openSettings()},
  ultra:{title:['Ultra 实际推理强度','Ultra actual reasoning effort'],detail:['模型按上游原名显示；Native 与导入订阅共用推理强度，保存后重启 Codex Runtime。','Models use their upstream names. Native and imported subscriptions share the effort setting; restart Codex Runtime after saving.'],open:()=>ultraSettings.open({back:openReview})},
  bills:{title:['会话账单归属','Conversation billing'],detail:['合成示例：自动审查、子代理与 Sidechat 合并到主会话；展开可查看模型及任务用量，归属不明的用量保留在总计中。','Synthetic examples: Auto Review, subagents and Sidechat belong to the main conversation. Expand model and task usage; unassigned usage remains in totals.'],open:()=>{statistics.open();document.querySelector('[data-stats-view="sessions"]').click();}},
  migration:{title:['迁移导入预览','Migration import preview'],detail:['清晰的等待提示；预览超时后保留选择，可直接重新预览。','Clear progress messages; keep selections and retry after a preview timeout.'],open:()=>window.EMP_IMPORT_DEMO()},
  connection:{title:['服务接入：Mercury 3','Service setup: Mercury 3'],detail:['新增 Inception / Mercury 预设；填写 Key，连接参数按需展开，保存只影响演示数据。','New Inception / Mercury preset: enter a key and expand connection details when needed. Saves affect demo data only.'],open:()=>chooseOfficialProvider('inception',null,{back:openReview})},
  collaboration:{title:['子代理与订阅性能对比','Subagent and subscription comparisons'],detail:['模式建议、速度与耗时对比，以及匿名的导入订阅／原生直连实测。','Mode recommendations, speed/time comparisons and anonymous imported/native direct measurements.'],open:()=>openCollaborationDemo()},
  metrics:{title:['调用结果与性能统计','Call outcomes and performance'],detail:['合成示例：重试完成、上游等待建议和输出中断；保留速度与工具耗时示例。','Synthetic examples: successful retries, upstream wait advice and interrupted output, alongside speed and tool timing.'],open:()=>statistics.open()},
  context:{title:['上下文与压缩说明','Context and compaction'],detail:['在现有模型设置的问号中查看有效窗口、自动压缩阈值和调整方式。','Use the existing model settings help to inspect usable context, compaction triggers and adjustments.'],open:()=>editAccount(state.native_account.id,{back:openReview})},
  routing:{title:['路由设置','Routing settings'],detail:['自动审查、图片及记忆路由；记忆提取、记忆整理可分别选择 GPT 模型。','Auto Review, image and memory routing; choose GPT models independently for memory extraction and consolidation.'],open:()=>realShadow?routingSettings.open({back:openSettings}):openRoutingDemo()},
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
let previewStartupAction=['import','settings','routing','ultra','collaboration','context','background','tray','review'].find(action=>reviewParameters.has(action))||reviewParameters.get('reviewAction');
function syncPreviewReview(){
  reviewButton.textContent=tr(...previewScenarios.review.title);
  if(!state?.native_account||!previewStartupAction)return;
  const action=previewStartupAction;previewStartupAction=null;
  if(action==='import'){
    const url=new URL(location.href);url.searchParams.delete('import');history.replaceState(null,'',url);
    if(realShadow)void window.EMP_OPEN_SYNTHETIC('import');else window.EMP_IMPORT_DEMO();
  }else window.EMP_REVIEW_ACTION(action);
}
