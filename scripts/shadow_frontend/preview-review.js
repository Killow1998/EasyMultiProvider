// Keep all unreleased changes since this baseline; do not reset the list per task.
// Production controllers own their dialogs and text.
const reviewBaseline='v0.13.20';
const reviewRows=['onboarding','quota_history','statistics','settings','gateway','capabilities','codex_maintenance','provider_balances','appearance'];
const realShadow=window.EMP_REAL_SHADOW===true;
const syntheticScenarios=['provider_balances','gateway','onboarding','lan','migration','connection','collaboration','metrics','recovery','lifecycle','removal','historical'];
const previewScenarios={
  appearance:{title:['应用图标','Application icon'],detail:['😎 为默认图标；可选服务商图标、彩色／单色，或导入正方形 SVG。EMP 单色版沿用彩色版的轮廓和比例。','Use the default sunglasses icon, service icons in color or monochrome, or a square SVG. Monochrome EMP retains the color logo’s shape and proportions.'],open:()=>{settings.open('general');document.querySelector('[data-app-icon-open]').click();}},
  statistics:{title:['统计显示','Statistics display'],detail:['Token 与费用使用 k/m/b 缩写，悬停查看完整数字。1h 分六段、1d 分二十四段、7d 分七段、30d 分三十段；查询／刷新右侧显示处理百分比和进度条。','Tokens and costs use k/m/b with exact values on hover. Charts use six buckets for 1h, 24 for 1d, seven for 7d, and 30 for 30d. Processing percentages and a progress bar appear beside Query/Refresh.'],open:()=>statistics.open()},
  provider_balances:{title:['服务商余额','Provider balances'],detail:['合成数据展示余额、Key 预算和官方账单入口；Mercury 补充 Inception 图标。','Synthetic data shows balances, key budgets and official billing links. Mercury now uses the Inception icon.'],open:()=>openBalancesDemo()},
  gateway:{title:['决策模型设置','Decision model settings'],detail:['从外部服务设置选择服务、接口和 Jev 模型；使用隔离测试实例。','Choose a service, API and Jev model from external service settings. Uses an isolated test instance.'],open:()=>{settings.open('external');document.querySelector('[data-decision-open]').click();}},
  capabilities:{title:['模型能力资料','Model capability data'],detail:['收录 123 条模型声明，补齐图像能力并修正旧缓存；修复工具图片转换与原始思考外显。','123 model records, image capabilities and older-cache updates; fixes for tool images and raw reasoning output.'],open:()=>openModal(tr(...previewScenarios.capabilities.title),`<p>${tr('已核对 123 条模型资料，覆盖 OpenAI、Claude、Gemini／Gemma、Grok、Kimi、DeepSeek、GLM、Meta 和 Mercury。','123 model records cover OpenAI, Claude, Gemini/Gemma, Grok, Kimi, DeepSeek, GLM, Meta and Mercury.')}</p><p>${tr('导入与旧缓存按确切模型 ID 补齐缺失能力；用户已设置的上下文、输出限制和推理强度继续保留。','Imports and older caches fill missing capabilities by exact model ID, retaining saved context, output and reasoning settings.')}</p>`,'',null,null,{back:openReview})},
  codex_maintenance:{title:['Codex 0.166 维护','Codex 0.166 maintenance'],detail:['维护范围 0.162–0.166，更新动态工具历史与记忆档位验收。','Maintenance covers 0.162–0.166, dynamic-tool history and memory service tiers.'],open:()=>openModal(tr(...previewScenarios.codex_maintenance.title),`<p>${tr('维护范围推进至 0.162–0.166。历史恢复保留动态工具更新记录与推理用量字段。','Maintenance advances to 0.162–0.166. History restoration preserves dynamic-tool updates and reasoning-usage fields.')}</p><p>${tr('已验证记忆整理的 Standard／Flex／Fast 档位，以及 App Server 时间戳、费用字段和 WebSocket 压缩协商。','Acceptance covers Standard/Flex/Fast memory consolidation and App Server timestamps, cost fields and WebSocket compression negotiation.')}</p>`,'',null,null,{back:openReview})},
  review:{title:['本次改动','This update'],detail:['查看当前待检阅的修改','Review the current changes'],open:openReview},
  tutorials:{title:['使用教程','Tutorials'],detail:['更新教学动图与弹窗布局，隧道迁移同时展示发送端和接收端。','Updated tutorial animations and layout, including a side-by-side tunnel transfer.'],open:()=>tutorials.open({back:openReview})},
  native_local_key:{title:['Native 切换为 EMP Key','Switch Native to EMP key'],detail:['保留为 Other 或移除当前 Native 登录，再切换为 EMP 本地 Key。','Keep the subscription as Other or remove the current Native sign-in, then switch to the EMP local key.'],open:()=>nativeLogin.openLocalKey({back:openReview})},
  notifications:{title:['通知与信箱','Notifications and inbox'],detail:['顶部浮层与倒计时条；右下角信箱保留此浏览器最近 50 条通知，刷新后仍可查看。','Top notification with a countdown bar; the bottom-right inbox keeps the last 50 messages in this browser across reloads.'],open:()=>{closeModal();$('notification_mailbox').click();}},
  onboarding:{title:['服务信息与登录方式','Service details and sign-in methods'],detail:['名称打开信息，齿轮打开模型设置。信息页移除顶部切换和登录来源，重置、移除放在账号行右侧。登录切换仍在刷新左侧；使用独立合成账号检阅。','Names open details; gears open model settings. Details omit the top tabs and sign-in origin; reset and remove sit beside the account. Sign-in switching stays before Refresh. Uses isolated synthetic accounts.'],open:()=>nativeLogin.manage()},
  quota_history:{title:['额度趋势','Quota trends'],detail:['查看真实账号趋势；增加近 15 天，额度窗口与日期范围并排显示。','Review real account trends, with Last 15 days and quota windows beside the date ranges.'],open:()=>{const account=[state.native_account,...state.accounts].find(item=>item.native?item.auth_mode==='subscription':item.enabled!==false&&item.credential_set&&item.credential_status!=='invalid');if(account)quotaHistory.open(account.id);else notice(tr('services.not_signed_in'));}},
  lan:{title:['隧道迁移修复','Tunnel migration fix'],detail:['修复两端时钟不同导致的解密失败，迁移代码仍固定五分钟有效。','Fixes decryption failures caused by clock differences. Migration codes still expire after five minutes.'],open:()=>migration.open()},
  settings:{title:['设置分类','Settings categories'],detail:['通用、外部服务、原生服务三张入口卡片；返回时保留所属分类。','Three entry cards: General, External services and Native services. Nested dialogs return to their category.'],open:()=>openSettings()},
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
  openModal(tr(...previewScenarios.review.title)+' · '+tr('相较 {version}','Since {version}').replace('{version}',reviewBaseline),`<div class="choice-grid">${reviewRows.map(reviewChoice).join('')}</div>`,'',null);
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
