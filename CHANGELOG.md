# Changelog

## Unreleased

## 0.15.3 (2026-11-21)

### 中文

问题修复

- 团队订阅同步。同一个订阅账号的本机凭据失效时，在「团队空间」点击「同步」，原有模型即可使用团队的有效凭据。模型名称、可见性和上下文设置沿用本机配置。
- 模型设置。同账号的共享副本有不同设置时，EMP 自动采用本机设置，省去重复选择。
- 从 0.15.2 升级时，只需更新桌面版 EMP Core。团队空间继续使用 0.10.1-beta。

### English

Fixes

- Team subscription sync. If the local credentials for an account expire, select Sync in Team Workspace to let its existing models use the valid shared credentials. Local names, model visibility, and context settings are preserved.
- Model settings. When a shared copy of the same account has different settings, EMP keeps the local settings automatically.
- To upgrade from 0.15.2, update desktop EMP Core. Team Workspace stays at 0.10.1-beta.

## 0.15.2 (2026-11-21)

### 中文

更新顺序

- 将桌面版 Core 升级到 0.15.2。
- 团队预算功能需要桌面版 Core 0.15.2、团队空间 0.10.1-beta 模组和 VPS 团队服务 0.10.1-beta 同时就位。
- Decision API、Device Gateway 和 Aggregate Routing 沿用原有的独立模组版本。

新增与改进

- 有兼容的独立更新时，「模组」入口和对应模组会显示红点。打开后可先查看更新说明，再选择安装。
- 安装、启用、停用和更新模组会直接热加载，无需重启 Core。已接收的请求会继续完成。更新模组后会保留上一份可用版本，以便回滚。
- 「使用教程」新增加入「团队空间」的教程。尚未安装团队空间模组时也能打开。
- 教程文字、演示 GIF、安装与升级指南以及两份 README 改为通过独立的指南更新通道发布，只下载有变动的文件。重新打开「使用教程」即可查看最新内容。下载失败时会继续使用上一份有效版本，首次启动使用内置副本。
- Native Codex Responses WebSocket 支持最大 128 MiB 的消息，超限帧在读取帧头时即被拒绝。
- 用量估算现已覆盖 GPT-6.2 Sol 和 Astra Ultrafast。升级时会刷新价格快照。
- Core 已集成新的可选 Aero 模组，可在「模组」中找到。是否可安装取决于对应平台的发布资源，首批提供 Linux x86_64 版本。

### English

Upgrade order

- Upgrade desktop Core to 0.15.2.
- Team budget features require desktop Core 0.15.2, the Team Workspace 0.10.1-beta module and the VPS team service 0.10.1-beta.
- Decision API, Device Gateway and Aggregate Routing keep their previous independent module versions.

Improvements

- A red dot appears on the Modules entry and on the relevant module when a compatible independent update is available. Open it to read the notes, then choose whether to install.
- Installing, enabling, disabling and updating a module hot-loads it without restarting Core. Requests already admitted run to completion. Updates retain the previous verified package for rollback.
- Tutorials now include a guide for joining a Team Workspace. It is available before the Team Workspace module is installed.
- Tutorial text, demonstration GIFs, installation and upgrade guides and both READMEs now update through a separately published guide channel that downloads only changed files. Reopen Tutorials to see the latest content. If a download fails, EMP keeps using the last valid edition, and first launch uses the bundled copy.
- Native Codex Responses WebSocket accepts messages up to 128 MiB and rejects oversized frames at the header.
- Usage estimates now cover GPT-6.2 Sol and Astra Ultrafast. Upgrading refreshes the price snapshot.
- Core integrates the new optional Aero module, listed under Modules. Availability follows the published release assets, starting with Linux x86_64.

## 0.15.1 (2026-11-20)

### 中文

问题修复

- macOS 浏览器登录。已安装官方 ChatGPT 或 Codex App 时，EMP 从 App 原位置验证并启动内置 CLI，修复有可用 CLI 却报「未找到」或探测失败的问题。仍从 WebUI 的 Native 账号登录入口发起。
- macOS 菜单栏。修复部分安装方式下 WebUI 能打开、菜单栏却没有 EMP 图标的问题。旧版 EMP 可继续通过应用内更新安装。

兼容性

- Codex 维护范围更新至 0.167–0.171。WebUI 左上角在检测到低于 0.167 的运行时时提供更新入口。

### English

Fixes

- macOS browser sign-in. EMP verifies and runs the CLI from the installed official ChatGPT or Codex app, fixing cases where sign-in reported a missing CLI or failed its version check. Start sign-in from the Native account in the WebUI.
- macOS menu bar. Fix a launch issue that could leave the EMP icon missing while the WebUI was available. In-app updates from older EMP versions continue to work.

Compatibility

- The Codex maintenance window is now 0.167–0.171. The WebUI header offers an update link for runtimes older than 0.167.

## 0.15.0 (2026-11-19)

### 中文

新增功能与入口

- 按需安装模组。在 WebUI 顶栏「设置」旁打开「模组」，可安装、停用或移除决策 API、多设备网关、聚合路由和团队空间。各模组有自己的「设置」入口。
- 加入团队。在团队空间填写昵称和七位团队码，等待管理员审批。团队码由管理员生成，有效期 3 分 22 秒。管理员可在顶部提示或成员管理中同意或拒绝。
- 共享 Codex 订阅。团队空间的「上传」发布自己的 Native Codex 订阅，获批后其他成员点击「同步」即可在 Codex 选择器使用。共享列表按持有者展示状态。VPS 上仍有效的凭据可以重新同步，同一账号更新凭据沿用已获批准的状态。
- 查看团队用量。团队首页显示个人用量，管理员还可看团队总量、成员资料和各模型明细，并设置每日 USD 上限、Fast／Ultrafast 和重置许可。成员在多台设备上的已上报用量汇总到同一人，接近或达到每日上限时会收到桌面通知。管理员可设置成员之间是否可查看用量。
- 接入个人 OpenAI 生图赠金。从「添加服务」连接项目，可填写赠金到期日和每月预算。派发前赠金不可用时，生图可使用合适的订阅额度。

使用变化

- 「团队空间」和「服务迁移」是独立入口。加入团队后，只有管理员使用导入、导出和隧道。现在可在「使用教程」查看团队、订阅共享、入组审批和成员管理的动画。
- Vertex AI 用 API Key 添加 Gemini 时，从每日更新的 LiteLLM 目录发现候选，再检查该 Key 是否可用。Google 发布新模型后可随目录更新获取。
- 跨服务子代理默认开启，已手动关闭的设置继续生效。服务图标旁的黄色提示表示仍在重试或等待重发历史，红色表示调用已经失败。

问题修复

- 更新订阅凭据、换设备和断网补报不再打断团队的正常同步与记账。退出或卸载团队模组会清理本机共享凭据，保留个人登录和用量。撤权后团队账号停止用于模型路由与决策请求。
- Native 非流式响应在无需转换工具内容时逐字节转发。长请求的转换与团队首页加载减少了重复处理。

升级与兼容性

- 从旧版应用内升级时，EMP 会检查原先使用的模组并在必要时回滚。Codex 维护范围为 0.166–0.170。

### English

New features and where to find them

- Install modules on demand. Open Modules next to Settings in the WebUI header to install, disable, or remove Decision API, Device Gateway, Aggregate Routing, and Team Workspace. Each has its own Settings button.
- Join a workspace. Enter a nickname and a seven-digit invitation code in Team Workspace, then wait for approval. Codes last 3 minutes and 22 seconds. Administrators can approve or decline requests from the top notice or the members table.
- Share Codex subscriptions. Upload publishes your Native Codex subscription for approval. Once approved, teammates can select Sync and use it from the Codex picker. The shared list shows availability by owner. A fresh sync recovers a stale local copy when the VPS copy is valid, and a credential refresh for the same approved account needs no new review.
- Track team usage. Workspace home shows your usage. Administrators can also view team totals, member profiles, and per-model breakdowns, and set daily USD limits, Fast/Ultrafast permissions, and reset permission. Reported usage rolls up across a member's devices. Desktop notifications appear when the daily limit is near or reached. Administrators control whether members can see one another's usage.
- Connect personal OpenAI image credits from Add Service. Set a monthly budget and credit expiration. If credits are unavailable before dispatch, image generation can use an eligible subscription.

Workflow changes

- Team Workspace and Service Migration have separate entries. After joining a workspace, only administrators can import, export, or transfer through a tunnel. Tutorials include walkthroughs for joining, sharing, approvals, and member management.
- Vertex AI discovers Gemini candidates for API-key connections through the daily LiteLLM catalog, then checks availability for the key. New Google models can appear without waiting for an EMP release.
- Cross-service subagents default to on. An explicitly disabled setting stays off. Yellow service notices indicate a retry or history recovery in progress. Red indicates a failed call.

Fixes

- Keep team sync and usage reporting working through credential refreshes, device changes, and temporary connectivity loss. Leaving or uninstalling the workspace module removes synced credentials from the current device and keeps personal sign-in and usage. Revoked shared subscriptions can no longer serve conversation or Decision requests.
- Forward Native non-streaming responses byte for byte when tool content needs no conversion. Reduce duplicate processing for long requests and the team home page.

Upgrade and compatibility

- In-app upgrades check previously used modules and roll back if required. The Codex maintenance window is 0.166–0.170.

## 0.14.3 (2026-11-18)

### 中文

新增功能与入口

- 新增 Linux ARM64 安装包。安装脚本和自动更新会按设备架构选包并校验。
- 网关设备用量可以逐台展开，查看每台设备的模型用量。
- 需要手动更新订阅模型时，可在对应订阅的模型设置中点「刷新模型列表」。EMP 也会每天后台刷新。
- 菜单栏图标不可用时，从「设置 → 通用」可以退出 EMP。

问题修复

- 修复 Codex 已运行新版、EMP 却优先选中闲置旧版的问题。自动选择优先采用可信的运行中引擎，同等候选优先选新版，继续尊重显式配置。
- 修复导入订阅后模型名称重复。模型选择器跟随当前账号目录，旧会话仍可使用历史路由。
- 修复中文和特殊符号的路由名在 Codex WebSocket 中引发断流。
- 登录失败会区分 CLI 缺失、信任失败、版本探测失败、启动或初始化失败。

### English

New features and where to find them

- Linux ARM64 packages are available. The installer and automatic updater select and verify the package for the current architecture.
- Expand a device in gateway usage to see its per-model totals.
- Select Refresh model list in the subscription's model settings to pull its latest catalog on demand. EMP also refreshes it daily.
- If the tray is unavailable, quit EMP from Settings → General.

Fixes

- Prefer the trusted, running Codex runtime over an idle old copy. Prefer newer versions among comparable candidates and honor explicit overrides.
- Fix duplicate model names after subscription import. The picker follows current catalogs and existing sessions retain their historical routes.
- Fix dropped Codex WebSocket connections for routes containing Chinese or special characters.
- Sign-in now distinguishes a missing CLI, failed trust or version checks, and startup or initialization failure.

## 0.14.2 (2026-11-15)

### 中文

使用变化

- 统计可识别通过 EMP 网关调用的 Python、Pi、OMP 等客户端。有会话 ID 的调用可按会话查看，其余按客户端或设备归属。
- EMP 在后台逐个转换空闲旧会话的压缩记录，方便以后关闭 EMP 继续使用原会话。任务会在退出时取消，并保留原始历史、工具结果和 Codex 密文。

问题修复

- 修复 Windows 关闭窗口或结束会话时，已经恢复原生配置却提示退出失败。
- 修复 Native 长会话发送内联图片时遇到的 400，也修复 App 页面上下文被误判为未配对工具结果的问题。纯文本请求仍使用压缩。
- 调用失败时显示上游状态码、错误参数和具体原因，并可用请求 ID 在诊断中查找。提示显示服务及实际上游模型名。

### English

Workflow changes

- Statistics identifies Python, Pi, OMP, and other clients using the EMP gateway. Calls with a session ID can be viewed by session. Other calls are attributed to the client or device.
- EMP migrates old compaction records in idle sessions in the background, so those sessions can continue after EMP is turned off. Migration stops on exit and preserves the original history, tool results, and Codex encrypted state.

Fixes

- Fix Windows reporting a failed exit after restoring Native settings when closing a window or ending a session.
- Fix HTTP 400 on Native requests with inline images, and fix App page context being mistaken for an unmatched tool result in long histories. Text-only requests still use compression.
- Failed calls show the upstream status, parameter, and reason when reported. Look up the request ID in diagnostics. The error names the service and upstream model.

## 0.14.1 (2026-11-14)

### 中文

新增支持

- Codex 的录音和音频附件可转发到支持的外部服务，覆盖 Responses、Chat 和 Vertex Gemini，恢复历史时也会保留音频引用。
- Vertex AI 参考目录收录 Gemini 4 Hydrogen 的能力信息，保留已设置的强度；该模型的 `xhigh` 会按原值发送。

问题修复

- 修复保存 API 服务后，图片生成或编辑因未读到密钥报 503。现有轻量图片工具也已验证 Image 2.6 的 GIF 生成、编辑和保存。
- 累计用量和统计显示调用当时的上游模型名。模型选择器继续用你设定的别名。无价格记录显示未知，已有价格的部分仍会计算小计。

兼容性

- Codex 维护范围为 0.165–0.169。已覆盖认证、Code Mode、增量工具和历史恢复的相关变化。

### English

New support

- Codex recordings and audio attachments can reach compatible external Responses, Chat, and Vertex Gemini services, including when history is restored.
- The Vertex reference catalog includes Gemini 4 Hydrogen capability information. Saved effort settings stay in place, and Hydrogen's `xhigh` is passed through.

Fixes

- Fix HTTP 503 on image generation and editing after saving an API service. The existing lightweight image tool was also verified for Image 2.6 GIF generation, editing, and saving.
- Usage and statistics display the upstream model name recorded at the time of the call. The picker keeps your aliases. Missing prices appear as unknown, with subtotals for calls that have known prices.

Compatibility

- The Codex maintenance window is 0.165–0.169, including checks for authentication, Code Mode, incremental tools, and history restoration.

## 0.14.0 (2026-11-12)

### 中文

新增功能与入口

- 「添加服务 → Vertex AI」支持用 Google Cloud API Key 连接 Gemini，或用应用默认凭据（ADC）和服务账号连接 Gemini、Claude。API Key 支持 Express 和指定项目，使用参考模型目录。
- 网关设备 Key 可持续查看、复制和撤销。老的仅摘要 Key 可重新生成，原设备用量继续归属到该设备。
- 额度趋势同图显示适用的 5h、7d、30d 曲线，可继续选择账号和时间范围。

使用变化

- 服务列表上方新增「聚合路由」入口。普通模型保留原样，聚合选项挨着对应模型，以「前缀 · 模型名」区分。WebUI 和 Codex 采用同一命名顺序。
- 原「路由设置」改为「生图、审查与记忆路由」，「决策模型」改为「决策 API」。配置决策服务时也可以就地添加服务。
- 自动审查只选用合适的订阅，排除 Free 和 Go。EMP 本地 Key 模式下，选择器只展示可调用的模型，仍可选其他订阅、外部模型和聚合路由。
- Codex 切换模型时显示可读的账号或服务名称。旧名称和既有会话仍能使用。添加、导入或改名时会检查重名和斜杠。

问题修复

- 修复 Vertex 项目模式误报 Key 无效、迁移合并不同项目或认证、重复导入 ADC 失败，以及切换表单后凭据文件被写入错误表单。
- 修复内置 GLM 添加失败、带 `@` 的版本化模型无法保存显示设置，以及目录同步后持续提示重载。重新导入外部服务会保留历史名称。
- 记录 Gemini 实际执行的模型版本和已报告的思考 Token。CPA 周额度耗尽的账号对号池 5h 可用额度按零计算，单账号数值仍按上游显示。
- 修复登录、网关和聚合路由的请求晚返回时打开旧页面或清空查询范围。

### English

New features and where to find them

- Add Service → Vertex AI supports a Google Cloud API key for Gemini, or Application Default Credentials (ADC) and service accounts for Gemini and Claude. API keys support Express and project modes using a reference catalog.
- View, copy, and revoke saved gateway device keys. You can regenerate legacy digest-only keys without losing past usage attribution.
- View the applicable 5h, 7d, and 30d quota lines together and continue to select the account and time range.

Workflow changes

- Open Aggregate Routing from the service toolbar. Aggregate options follow their original model and use “prefix · model name.” Individual models keep their usual format. The WebUI and Codex use the same names and order.
- Routing Settings becomes Image, Auto Review and memory routing. Decision models becomes Decision API. Add a service without leaving Decision API setup.
- Auto Review skips Free and Go subscriptions. With an EMP Local Key, the picker hides unavailable Native entries and keeps other subscriptions, external models, and aggregate routes.
- Codex model switches show readable account and service names. Existing names and sessions still work. Add, import, and rename check for duplicate names and slashes.

Fixes

- Fix false invalid-key errors in Vertex project mode, merging distinct connections on import, repeated ADC import failures, and credential-file reads writing to the wrong form.
- Fix adding the GLM preset, saving display names for versioned model IDs containing `@`, and repeated catalog reload prompts. Reimports preserve external service names used by existing sessions.
- Preserve the model version reported by Gemini and its reported reasoning tokens. A CPA account with no weekly quota left contributes zero to the pool's usable 5h quota. Individual readings keep the upstream values.
- Fix late sign-in, gateway, and aggregate responses that reopened an old view or lost the selected period.

## 0.13.29 (2026-11-10)

### 中文

新增支持

- Image 2.6 可选用指定 API 服务，并支持复杂度参数和 SVG／GIF。轻量图片工具也支持这些输出格式。

使用变化

- Claude CPA 的公开文字会边生成边显示。Codex 在工具参数完整后接收工具调用。
- 看过 30 天统计后，可用已读取明细切换到较短区间或修改筛选。各服务用量显示加载进度，可在原处重试。Fast 和 Ultrafast 分开统计。

问题修复

- 修复 macOS 上 Claude 长时间停在等待状态，以及离开统计页面后后台仍在扫描的问题。
- 修复服务详情返回后只能继续返回、无法关闭的问题。外部模型默认显示服务名称，原调用 ID 继续可用。
- 取消生图会一并关闭上游连接。通用 Responses 客户端收到适用于自身的事件，Codex 的专用事件保留。

### English

New support

- Image 2.6 supports explicit API-service selection, complexity, and SVG/GIF options. The lightweight image tool supports those output formats as well.

Workflow changes

- Claude CPA streams its visible text as it arrives. Codex receives tool calls after their arguments are complete.
- After loading 30 days of statistics, switch to a shorter period or change a filter using the records already read. Service usage shows progress and an inline retry. Fast and Ultrafast are shown separately.

Fixes

- Fix Claude waiting indefinitely on macOS and report scans continuing after the view is closed.
- Fix service details losing their Close control after going back. External models use the service name by default and retain their original route IDs.
- Cancelling an image request closes its upstream connection. Generic Responses clients receive compatible events, while Codex keeps its dedicated events.

## 0.13.28 (2026-11-07)

### 中文

新增功能

- Free Native 和 EMP 本地 Key 可用同一个图片工具生成、编辑图片。图片保存到本机，Codex 收到文件路径，原会话和登录可以继续使用。付费 Native 继续使用原生图片工具。
- 先在「路由设置」配置有额度的生图订阅，再按问号里的说明完成授权和运行时重启。

### English

New feature

- Free Native and EMP Local Key users can generate and edit images with a single image tool. Images are saved locally and Codex receives the file path. Keep your existing chats and sign-in. Paid Native continues to use its native image tools.
- Choose an eligible subscription in Routing Settings. The image help explains authorization and when to restart the Codex runtime.

## 0.13.27 (2026-11-07)

### 中文

新增功能与入口

- 聚合路由把同一模型的多个来源合成一个 Codex 选项。可设显示前缀，并按优先级、同等级额度均衡或新会话轮询分配。已开始的会话优先沿用原账号。
- 网关可为每台设备创建连接 Key。在设备管理可查看调用量、Token 与已知价格的 USD 估算，也可撤销 Key。经端口转发的请求仍按 Key 归属。
- CPA 额度趋势可查看整个号池或单账号的 5h／7d 余量，保留 15 天历史。服务接入说明在连接设置中，未提供号池接口的服务隐藏相关控件。
- 「路由设置」可分别调整记忆提取和整理的思考强度。问号内可查看 24 小时的 Token、缓存命中率和费用对比。主对话强度不受影响。

使用变化

- Claude CPA 由 Claude Code 执行推理，历史和工具继续在 Codex 中处理。转发保留长上下文、原图、PDF、思考强度和已报告用量。
- Decision API 调用单独计量。统计区分未报告数据和真实的零值，并保留上游错误与交付状态。

问题修复与兼容性

- 补齐后台记忆提取和整理漏掉的历史用量，避免重启或扫描后重复入账。
- 验证 Codex 0.167 的取消、压缩、分页历史和动态工具目录，维护范围为 0.163–0.167。

### English

New features and where to find them

- Aggregate Routing combines several sources for one model into one Codex option. Set a display prefix and allocate new sessions by priority, same-plan quota balance, or round robin. Existing sessions prefer their current account.
- Create a gateway key for each device. Device management shows calls, tokens, and USD estimates with known pricing, and lets you revoke a key. Forwarded requests remain attributed to their key.
- CPA trends show 5h and 7d quota for the pool or an individual account, with 15 days of history. Connection settings explain the quota API. Services without that API hide the quota controls.
- Routing Settings has separate reasoning-effort selections for memory extraction and consolidation. The help shows a 24-hour comparison of tokens, cache hits, and cost. Main conversation effort stays unchanged.

Workflow changes

- Claude CPA runs inference through Claude Code while Codex handles history and tools. The integration preserves long context, original images, PDFs, effort, and reported usage.
- Decision API calls are metered separately. Statistics distinguishes missing data from reported zero and retains upstream errors and delivery status.

Fixes and compatibility

- Recover missing historical usage for background memory extraction and consolidation, without counting it twice after a restart or scan.
- Verify cancellation, compaction, paginated history, and dynamic tool catalogs in Codex 0.167. The maintenance window is 0.163–0.167.

## 0.13.26 (2026-11-04)

### 中文

新增功能与入口

- CPA 服务行显示号池的 5h／7d 剩余额度和最近重置时间，悬停可查看各账号。
- 「使用教程」的 11 段动画更新到当前界面，包括模型设置、连接设置、移除服务和隧道迁移。

问题修复

- 修复外部模型在工具往返中反复要求重发历史的问题，保留工具结果与调用记录。
- 补发完成后，统计会把结果关联到原调用，避免继续标为等待重发。

### English

New features and where to find them

- CPA service rows show the pool's remaining 5h and 7d quota and closest reset time. Hover to see individual accounts.
- All 11 tutorial animations reflect the current interface, including model settings, connection settings, removal, and tunnel migration.

Fixes

- Fix repeated history-resend prompts during external-model tool exchanges while preserving tool results and usage records.
- A completed resend is now linked to the original call and no longer appears to be waiting for history.

## 0.13.25 (2026-11-03)

### 中文

使用变化

- 服务列表显示 DeepSeek 和 Kimi 的账户余额。其他服务的 Key 预算不再当作余额显示。

问题修复

- 外部 Chat 模型的进度和最终回答分开展示。完整与流式响应都保留工具调用和用量。
- DeepSeek 响应混入工具控制指令时显示明确的协议错误。正常文字和代码示例照常显示。

### English

Workflow changes

- Service rows show account balances for DeepSeek and Kimi. Other services' key budgets are no longer shown as balances.

Fixes

- Keep external Chat model progress separate from the final answer, preserving tool calls and usage in both complete and streamed responses.
- Report DeepSeek tool-control instructions in the answer as a protocol error while preserving ordinary text and code examples.

## 0.13.24 (2026-11-02)

### 中文

使用变化

- 减少长历史在请求解析、WebSocket 收发、统计和模型适配中的重复复制，更早释放大块缓冲区；Linux 在请求空闲时归还可释放的内存页。
- 原生模型目录按优先级读取，找到有效目录即停止，加快启动与目录刷新。
- 统计查询复用已筛选记录和二进制 JSON，速度更快，并保留完整汇总、分页、计费和实际查询进度。
- HTTP、SSE 和 WebSocket 使用统一的繁忙及请求体积超限提示；保留上游状态、已生成内容、工具进度与用量，不增加自动重试。

问题修复

- 恢复原生时，仅对确认需要后续处理的 EMP 历史检查点给出提示，不再对大量未检查会话显示无效警告。

### English

Workflow changes

- Reduce duplicate long-history copies across request parsing, WebSocket I/O, accounting and model adapters, and release large buffers sooner. On Linux, free heap pages are returned between requests.
- Read the native model catalog by priority and stop at the first valid source, speeding up startup and catalog refresh.
- Speed up statistics queries by reusing filtered records and binary JSON while keeping complete aggregates, pagination, pricing and actual query progress.
- Use the same busy and request-size errors across HTTP, SSE and WebSocket. Upstream status, generated content, tool progress and usage are preserved, with no added automatic retries.

Fixes

- During Restore Native, show history notices only for confirmed EMP checkpoints that need follow-up, removing unhelpful warnings about unchecked sessions.

## 0.13.23 (2026-11-01)

### 中文

问题修复

- 修复 Native 和导入的 Codex 订阅在续接含输入项 ID 的历史时，因上游拒绝而返回 400 的错误；兼容新旧两种上游错误格式，沿用一次性恢复，保留工具配对、历史引用和用量记录。

### English

Fixes

- Fix HTTP 400 errors when Native and imported Codex subscriptions continue history containing input item IDs. Both upstream error formats are recognized and handled with one-time recovery, preserving tool pairing, history references and usage records.

## 0.13.22 (2026-10-31)

### 中文

使用变化

- 流式与非流式请求使用同一协议转换，统一应用模型、推理强度和工具配置。
- Chat、Anthropic 和 Responses 共用工具目录处理，保留命名空间、增量工具和名称冲突规则。
- 减少响应转换时对整段输出的重复复制，保留回答、图片、工具调用关系与用量统计。

### English

Workflow changes

- Apply the same model, reasoning-effort and tool settings to streamed and complete responses through one request projection.
- Share tool-catalog handling across Chat, Anthropic and Responses while keeping namespaces, incremental tools and name-collision rules.
- Avoid repeated copies of complete response output while preserving answers, images, tool-call relationships and usage.

## 0.13.21 (2026-10-31)

### 中文

新增功能

- 设置新增决策模型配置，可选择服务、接口与模型。
- 为 DeepSeek、Moonshot 与 OpenRouter 显示余额或 Key 预算，为其他官方服务提供账单入口；补齐 Mercury 图标。
- 支持切换浏览器、系统匣和应用图标：默认 😎，可选服务商图标、彩色或单色，或自定义正方形 SVG。
- 额度趋势新增「近 15 天」，日期范围与额度窗口并排显示。

使用变化

- 设置按「通用」「外部服务」「原生服务」分组。
- 服务操作与账号信息页中，登录切换、刷新、趋势和模型设置使用统一图标；重置与移除位于账号行右侧。
- 统计数字使用 k/m/b 缩写，悬停显示完整数值；调整时间分段并加快查询，在「查询／刷新」旁显示实际处理百分比。

问题修复

- 修复切换到外部模型时，工具结果含图片导致的 `unsupported tool output` 错误；保留图片与文字顺序和工具调用关系。
- 外部模型统一响应过滤，Chat 接口的原始思考内容不再外显；保留最终回答、工具调用和推理用量统计。

升级与兼容性

- 适配 Codex 0.166 的工具历史、记忆与用量协议，维护范围更新为最近五个正式系列 0.162–0.166。
- 模型能力资料扩展至 123 条并补齐图像能力，自动更新旧缓存；保留用户已配置的上下文、输出限制和推理强度。

### English

New features

- Add decision-model settings for selecting a service, API and model.
- Show balances or key budgets for DeepSeek, Moonshot and OpenRouter, link billing pages for other official services, and add the Mercury icon.
- Customize browser, tray and application icons: the default 😎, service icons in color or monochrome, or a custom square SVG.
- Add Last 15 days to quota trends, with date ranges placed beside quota windows.

Workflow changes

- Group settings into General, External services and Native services.
- Use consistent icons for sign-in switching, refresh, trends and model settings in service actions and account details; reset and remove now sit to the right of the account row.
- Format statistics with k/m/b abbreviations and show exact values on hover. Time buckets and query speed are improved, with actual processing percentages beside Query/Refresh.

Fixes

- Fix `unsupported tool output` when switching to an external model with images in tool results, preserving media order and tool-call relationships.
- Apply one output filter to external models so raw Chat reasoning no longer reaches clients, while preserving final answers, tool calls and reasoning usage.

Upgrade and compatibility

- Support the Codex 0.166 tool-history, memory and usage protocols. Maintenance now covers the latest five stable series, 0.162–0.166.
- Expand capability records to 123 models with image capabilities and update older caches automatically, keeping saved context, output-limit and reasoning-effort settings.

## 0.13.20 (2026-10-30)

### 中文

新增功能

- 原生登录支持官方设备码、浏览器登录、自己的 OpenAI API Key，以及切换到已导入的 Other 订阅；正确显示 API Key 登录状态。
- 新增独立、稳定的 EMP 网关 Key，可在设置中复制 API 地址与凭证；更换 Native 登录后，API 客户端和端口转发继续使用原网关 Key。
- 新增 Jev Decisions 转发接口，使用已配置服务的凭据，保留上游状态与限流信息；Jev 不出现在 Codex 模型选择器中。

使用变化

- Claude CPA 适配器不再单独限制 10 MiB，改用统一的请求大小上限；保留原始图片和工具历史，大段文字通过 CLI 结构化输入发送，长历史仍按模型上下文压缩。

问题修复

- 修复 EMP 本地 Key 的认证文件格式；没有订阅和 OpenAI API Key 时也可直接启动 Codex。
- 修复两台电脑时钟不一致时隧道迁移预览失败的问题，保留五分钟有效期、发送端确认与一次性导入。
- 修复 macOS 内存统计被误判为零、导致大请求返回 503 的问题。

升级与兼容性

- 升级后自动修正旧版占位认证文件，保留已有真实登录。
- 切换原生登录方式后，按提示重启 Codex Runtime。

### English

New features

- Native sign-in supports official device-code or browser sign-in, your own OpenAI API key, and switching to an imported Other subscription. API-key sign-in status is shown correctly.
- Add a stable, separate EMP Gateway Key, with the API address and credential copyable from settings. API clients and port forwards keep using it when Native sign-in changes.
- Add a Jev Decisions endpoint that uses configured service credentials and preserves upstream status and rate-limit information. Jev does not appear in the Codex model picker.

Workflow changes

- Replace the Claude CPA adapter's separate 10 MiB cap with the shared request-size limit. Original images and tool history are preserved, large text is sent through the CLI's structured input, and long history is still compacted according to model context.

Fixes

- Fix the EMP local-key authentication file format. Codex can now start without a subscription or OpenAI API key.
- Fix tunnel migration previews failing when the two computers' clocks differ, keeping the five-minute expiry, sender approval and one-time import.
- Fix valid macOS memory statistics being treated as zero, which caused large requests to return HTTP 503.

Upgrade and compatibility

- Older managed placeholder authentication files are upgraded automatically; existing real sign-ins are kept.
- Restart Codex Runtime when prompted after switching the Native sign-in method.

## 0.13.19 (2026-10-29)

### 中文

新增功能

- 支持将 Native 订阅切换为 EMP 本地 Key，可选择保留为 Other 账号或移除当前登录；保留已有 Other 账号的名称和模型设置，完成后按提示重启 Codex Runtime。

使用变化

- 更新全部 11 项使用教程：隧道迁移在同一画面展示发送端和接收端；教程弹窗内动图自动缩放，左侧目录独立滚动，并可在新窗口查看大图。

### English

New features

- Switch a Native subscription to an EMP local key, either keeping the subscription as an Other account or removing the current login. An existing Other account's name and model settings are preserved; restart Codex Runtime when prompted.

Workflow changes

- Refresh all 11 tutorials. Tunnel migration shows the sending and receiving sides together; animations fit within the tutorial dialog, the left directory scrolls separately, and a larger view can be opened in a new window.

## 0.13.18 (2026-10-28)

### 中文

新增功能

- 调用明细分别显示所选与服务端回报的推理强度，以及请求与服务端回报的档位；性能统计按服务端档位分组，缺少回报的记录显示未确认，Ultra 与回报的 xhigh 分开记录。
- 使用教程新增隧道迁移演示，涵盖发送、接收、确认和导入。

使用变化

- 服务迁移的导入、导出和隧道列表独立滚动，服务文字不再与分隔线重叠，操作按钮不会被长列表挤出视野；隧道迁移代码与接收确认并排显示。

问题修复

- 正确保留上游「模型不存在」错误并提示重新选择模型，不再对此类错误进行无效重试和协议回退。
- 修复尚未生成迁移代码时仍显示上次过期提示的问题。

升级与兼容性

- 适配 Codex 0.165：记忆提取使用所选模型，会话中切换推理强度后记录新值；日常维护范围调整为最近五个正式系列 0.161–0.165。

### English

New features

- Call details show selected and upstream-reported reasoning effort, and requested and upstream-reported service tier, separately. Performance statistics are grouped by the reported tier; records without a report show as unconfirmed, and Ultra is recorded separately from a reported xhigh effort.
- Add a tunnel migration tutorial covering sending, receiving, approval and import.

Workflow changes

- Import, Export and Tunnel lists in service migration scroll independently. Row separators no longer overlap service text, long lists no longer push actions out of view, and tunnel codes sit beside receiver confirmation.

Fixes

- Preserve upstream model-not-found errors and prompt to select another model, without ineffective retries or protocol fallback.
- Stop showing a previous transfer's expiry warning before a new migration code is generated.

Upgrade and compatibility

- Support Codex 0.165: memory extraction uses the selected model, and reasoning-effort changes within a conversation are recorded. Routine maintenance now covers the latest five stable series, 0.161–0.165.

## 0.13.17 (2026-10-27)

### 中文

使用变化

- 退出 EMP 后，浏览器允许关闭的 WebUI 页面自动关闭；保留的页面显示「EMP 已退出」并停止发送请求。「恢复原生」仍保持 EMP 运行。
- 同一浏览器中的多个 WebUI 页面共享通知连接，退出通知和状态更新保持同步。

问题修复

- 改善原生和导入订阅的连接恢复：临时连接故障后，完整历史重试可通过 HTTP/SSE 继续；冷却后自动恢复 WebSocket 和增量传输，无需重启 Codex。
- 修复 WebSocket 连接到期后仍保留旧响应标识的问题；网络诊断新增发送和接收失败类型。

### English

Workflow changes

- After Quit EMP, WebUI pages close when the browser permits. Remaining pages show “EMP has exited” and stop sending requests. Restore Native keeps EMP running.
- WebUI pages in the same browser share one notification connection, keeping exit notices and state updates in sync.

Fixes

- Improve connection recovery for Native and imported subscriptions. After a temporary failure, full-history retries can continue over HTTP/SSE, and WebSocket and incremental transport resume automatically after cooldown without restarting Codex.
- Discard old response IDs after a WebSocket connection expires, and add send/receive failure types to network diagnostics.

## 0.13.16 (2026-10-26)

### 中文

新增功能

- 外部模型新增最大输出 Tokens 设置：请求采用模型设置与调用方限制中较小的值，留空则不设额外上限。
- 统计中的诊断新增连接与模型探测记录，单独展示目录读取及文本、图片、音频测试；测试用量计入用量统计，但不计入会话账单和对话性能样本。
- 更新说明汇总当前版本至目标正式版之间的改进与修复，按中英文分组显示并本地缓存；跨预发布页面继续读取正式版说明，返回异常或超出缓存上限时保留已有缓存。

使用变化

- 服务、模型和会话账单统一用量显示，区分未返回用量、已报告零用量和部分统计，不再将未知值显示为零。

问题修复

- 修复 Codex 配置目录文件较多时遗漏会话标题和父子关系的问题。
- 修复读取系统代理时阻塞并发请求的问题，刷新期间沿用已有代理结果。
- 修复频繁模型测试挤占普通请求诊断样本的问题，重启后分别保留普通请求和探测记录。

### English

New features

- Set a maximum output token limit for external models. Requests use the smaller of the model setting and the caller's limit; leave it blank to remove the extra cap.
- View connection and model checks separately in Statistics diagnostics, including catalog reads and text, image and audio tests. Test usage counts toward usage totals but stays out of session bills and conversation performance samples.
- Read improvements and fixes from the current version through the target stable release, grouped in Chinese and English and cached locally. Notes continue across preview-only pages, and the existing cache is kept when a response is invalid or exceeds the cache limit.

Workflow changes

- Services, models and session bills share one usage format that keeps missing usage, reported zero and partial totals distinct instead of showing unknown values as zero.

Fixes

- Fix missing conversation titles and parent relationships when the Codex configuration directory contains many files.
- Fix concurrent requests blocking while system proxy settings are read; the last proxy result is reused during refresh.
- Fix frequent model tests displacing ordinary request diagnostic samples; ordinary requests and check records are restored separately after restart.

## 0.13.15 (2026-10-25)

### 中文

新增功能

- 装置码登录前可选择添加为 Other 或替换 Native，替换成功后提示重启 Codex Runtime；auth.json 的文件选择与粘贴仍统一导入为 Other。

问题修复

- 修复部分 Native 和导入订阅请求因输入项目 ID 返回 400 的问题；保留消息、工具结果和增量会话关系，不重复执行已开始生成的请求。
- 当前 Codex 使用本地 Key、API Key 或未登录时，自动审查可正常选择可用的导入订阅，各入口的订阅凭证判断保持一致。
- 修复迁移提交结果被预览取消操作丢弃的问题；提交期间锁定编辑和返回，成功后刷新服务列表。

### English

New features

- Choose Other or Native before device-code sign-in, with a prompt to restart Codex Runtime after a successful Native replacement. File selection and pasted auth.json still import as Other.

Fixes

- Fix some Native and imported-subscription requests returning 400 because of input item IDs. Messages, tool results and incremental continuation are preserved, and requests that have started generating are never replayed.
- Let Auto Review select an available imported subscription when Codex uses a local key, an API key or no sign-in, with consistent subscription eligibility across entry points.
- Fix migration commit results being discarded by preview cancellation. Editing and Back are locked during commit, and the service list refreshes after success.

## 0.13.14 (2026-10-23)

### 中文

新增功能

- 无 Codex 订阅也能连接外部模型：EMP 为未登录的 Codex 创建本地连接 Key，可在服务列表中取消以恢复原生连接。
- 在 WebUI 中通过官方装置码登录订阅，成功后替换本地 Key，重启 Codex 生效；添加订阅提供装置码登录与 auth.json 两个入口，文件选择和粘贴共用导入表单。
- 服务迁移新增局域网隧道：自动选择本地网络，生成 5 分钟有效的一次性 49 位代码；发送端确认后，接收端预览并选择导入内容。
- Ultra 可按模型选择实际推理强度，显示上游模型名称及当前官方默认值，Native 与导入订阅共用该设置。
- 记忆提取与记忆整理可分别选择 GPT 模型，各自显示默认模型并可单独还原，保留已有设置；保存后重启 Codex 生效。
- WebUI 新增使用教程，左侧选择操作、右侧播放本地 GIF。

使用变化

- 导入、导出、隧道共用服务选择与预览流程，返回时保留已填内容；服务工具列按刷新额度、添加服务、服务迁移排列。
- 通知改为顶部提示与倒计时条，右下角信箱保留最近 50 条通知；不再每次打开页面都提示配置已加载。
- 帐号用量复用缓存并合并读取，从模型页返回时立即显示已有数据；弹窗标题与关闭按钮固定显示。

问题修复

- 修复 Codex 0.164 切换模型后增量工具定义冲突、已移除工具仍可调用的问题；更新后继续会话和恢复会话时保留工具历史。
- 跨服务子代理说明区分 Codex 0.164 Code Mode 明文任务与旧版加密历史；新旧速度测试分别标注日期、版本和测量范围。

### English

New features

- Use external models without a Codex subscription. EMP creates a local connection key for signed-out Codex; cancel it from the service list to restore the native connection.
- Sign in to a subscription with the official device code in WebUI. Successful sign-in replaces the local key; restart Codex to apply it. Add subscriptions through device-code sign-in or one auth.json form supporting file selection and paste.
- Transfer services over a LAN tunnel in Service migration, with automatic local-network selection and a single-use 49-character code valid for five minutes. The sender approves before the receiver previews and selects what to import.
- Choose Ultra's actual reasoning effort per model, with upstream model names and current official defaults shown. Native and imported subscriptions share the setting.
- Choose GPT models separately for memory extraction and memory consolidation, each with its own default label and reset, preserving existing settings. Restart Codex after saving.
- Open tutorials in WebUI: pick an operation on the left and play its local GIF on the right.

Workflow changes

- Import, Export and Tunnel share service selection and preview, and Back keeps entered values. The service toolbar is ordered Refresh quotas, Add service, Service migration.
- Notifications appear at the top with a countdown bar, and a bottom-right inbox keeps the latest 50. The configuration-loaded notice no longer appears on every page open.
- Account usage reuses cached totals and coalesces reads, so returning from the model page shows existing data immediately. Dialog titles and close buttons stay visible.

Fixes

- Fix conflicting incremental tool definitions and removed tools remaining callable after switching models in Codex 0.164. Tool history is preserved when continuing after an update or resuming a conversation.
- Cross-service subagent help distinguishes plaintext Code Mode tasks in Codex 0.164 from older encrypted history, and current and historical speed tests are labeled by date, version and scope.

## 0.13.13 (2026-10-22)

### 中文

使用变化

- 历史报告区分已确认待转换的 EMP 压缩点与未检查的会话，并显示对应的检查或继续转换操作。

问题修复

- 修复恢复原生和退出时因旧会话路径失效而反复扫描历史目录、长时间无响应的问题；系统匣菜单显示当前处理进度。
- 修复 Claude Code 旧版日期提示导致的历史格式误判，恢复 Windows 上受影响的 Claude 模型请求。
- 无法恢复的加密协作历史直接返回具体处理说明，不再反复重连同一请求；上游限流和临时故障仍会重试。

### English

Workflow changes

- The history report separates confirmed EMP checkpoints awaiting conversion from unchecked sessions, with the matching check or continue-conversion action.

Fixes

- Fix repeated history-directory scans and long stalls during Restore Native and Quit when saved session paths are stale. The tray menu shows the current operation.
- Fix history misclassification caused by the legacy Claude Code date reminder, restoring affected Claude model requests on Windows.
- Return specific guidance for unrecoverable encrypted collaboration history instead of repeatedly reconnecting the same request. Upstream rate limits and transient failures are still retried.

## 0.13.12 (2026-10-22)

### 中文

新增功能

- 新增记忆整理模型路由：可选择 GPT 模型，使用当前会话的订阅账号，外部模型会话使用 Native；账号不可用或额度耗尽时说明原因，模型设置不改变记忆权限。
- 新增 Codex 后台压缩 Beta 开关，默认关闭；启用后默认在窗口占用 65% 时触发，保留已有自定义比例。

使用变化

- 会话账单将 Auto Review、Sidechat 和子代理用量归入所属主会话，保留模型与任务明细。
- 上下文问号说明区分配置窗口、有效窗口与压缩阈值，并显示已有覆盖配置的影响。
- 设置按显示、通用服务、外部服务和原生服务分组；macOS 菜单栏改用随系统明暗外观显示的纯色模板图标。

问题修复

- 修正缺失元数据导致的会话名称、搜索异常和未知会话显示；修正窄屏问号说明的定位与滚动，保存失败时回滚本次配置写入。

升级与兼容性

- 启用或修改后台压缩后请重启 Codex；App／IDE 用户需重启其 Codex 后台服务。

### English

New features

- Add memory-consolidation model routing: choose a GPT model that uses the current conversation's subscription account, or Native for external-model conversations. Unavailable or exhausted accounts are explained, and the model setting does not change memory permissions.
- Add a Codex background-compaction Beta switch, off by default. When enabled it triggers at 65% window usage by default and keeps existing custom percentages.

Workflow changes

- Session bills attribute Auto Review, Sidechat and subagent usage to the owning main conversation, keeping model and task breakdowns.
- Context help distinguishes configured windows, effective windows and compaction thresholds, and shows the effect of existing overrides.
- Settings are grouped into display, general services, external services and native services. The macOS menu bar uses a monochrome template icon that follows the system appearance.

Fixes

- Fix conversation names, search and unknown-session display caused by missing metadata. Fix narrow-screen help positioning and scrolling, and roll back configuration writes when saving fails.

Upgrade and compatibility

- Restart Codex after enabling or changing background compaction; App/IDE users should restart their Codex backend service.

## 0.13.11 (2026-10-21)

### 中文

新增功能

- 桌面启动后保留系统匣图标（macOS 为菜单栏），左键或右键均打开菜单：打开 WebUI、恢复原生／启用 EMP、退出 EMP；关闭网页后可从图标返回，操作同步更新网页状态。
- 新增会话账单：按名称或 ID 搜索会话，查看所选时段的 token、缓存读写、各模型用量与 API 等价 USD，支持排序和分页，并区分未知价格、缺失用量与零价记录。
- 添加 Inception / Mercury 服务预设；快速教程提供 Native 模型、导入订阅、外部服务和数据迁移的操作 GIF。

使用变化

- 恢复原生、正常退出和离线恢复时自动转换空闲的 EMP 历史并保留原始备份；正在使用的会话暂缓处理，可在 WebUI 中继续完成。
- 调用详情新增按轮次合并的工具耗时；数据迁移预览超时后可直接重试并保留选择；连接详情默认折叠，调用错误显示实际发送次数及处理建议。

问题修复

- 修正批量返回造成的异常生成 TPS，无法可靠测量时显示未知，并保留生成 TPS 与全程 TPS 的区别。
- Windows 后台辅助进程不再闪出终端窗口；Claude CLI 按可用内存安排子进程，低内存时有界等待并保留登录状态。
- 官方 OpenAI Responses 接口保留增量工具加载位置，维持已有提示词前缀，减少对话历史的重复复制。

### English

New features

- Keep a system-tray icon after desktop launch (menu bar on macOS). Either mouse button opens the menu: Open WebUI, Restore Native / Enable EMP, Quit EMP. Reopen a closed page from the icon; tray actions stay in sync with the page.
- Add session bills: search by name or ID and view tokens, cache reads/writes, per-model usage and API-equivalent USD for the selected period, with sorting and pagination that distinguish unknown prices, missing usage and zero-price records.
- Add the Inception / Mercury service preset, plus quick-tutorial GIFs for Native models, imported subscriptions, external services and data migration.

Workflow changes

- Restore Native, normal Quit and offline restore automatically convert idle EMP history and keep the original backups. Active conversations are deferred and can be finished from WebUI.
- Call details add merged tool time per turn. Timed-out migration previews can be retried without losing selections. Connection details are folded by default, and call errors show actual send counts with guidance.

Fixes

- Fix inflated generation TPS from batched delivery; unreliable measurements show as unknown, and generation and end-to-end TPS remain separate.
- Windows helper processes no longer flash console windows. Claude CLI processes are scheduled by available memory, with bounded waits and preserved login state when memory is low.
- Preserve incremental tool-loading positions on the official OpenAI Responses endpoint to keep the existing prompt prefix and reduce repeated conversation-history copies.

## 0.13.10 (2026-10-19)

### 中文

问题修复

- 修复含图片的 Codex 会话切换到 Claude CPA 后请求失败的问题，之后可继续追加图片和工具结果。
- 转发时保留原始图片、文字及工具结果的顺序。

### English

Fixes

- Fix failures after switching Codex conversations that contain images to Claude CPA; later image and tool-result turns also work.
- Forwarding preserves the original order of images, text and tool results.

## 0.13.9 (2026-10-19)

### 中文

使用变化

- 主对话历史中的原生加密子代理消息沿用现有“按需恢复”，不新增开关，只转换当次转发内容，不改写 Codex 会话；在关闭或打开模式下会提示切换到按需恢复，并说明额外 GPT 用量。

问题修复

- 修复外部主线程发起子代理调用时缺少 Codex 明文标记的问题；按需恢复前统一验证全部来源和账号，每次最多处理八条加密消息。
- 兼容 Claude Code 将大 PNG 截图缩放为 JPEG 的行为，保留文字、图片顺序和工具权限校验。
- 修复资源耗尽或暂时错误后接收连接的监听被静默关闭的问题，改为记录并限速重试。
- 修复外部流式请求等待上游时被重复计入进行中请求的问题，取消后正确释放活动计数与上游连接。

### English

Workflow changes

- Native encrypted subagent messages in parent history use the existing On-demand recovery with no new setting; only the forwarded content is converted and Codex sessions are not rewritten. Off and On modes suggest switching to On-demand and explain the extra GPT usage.

Fixes

- Fix missing Codex plaintext markers on subagent calls from external parent threads. All sources and accounts are verified before On-demand recovery, with at most eight encrypted messages per request.
- Accept Claude Code's downscaling of large PNG screenshots to JPEG while keeping text, image order and tool-permission validation.
- Fix the connection listener silently closing after resource exhaustion or transient errors; failures are now logged and retried with rate limiting.
- Fix external streaming requests being counted twice as in progress while waiting for upstream, and release the activity count and upstream connection on cancellation.

## 0.13.8 (2026-10-18)

### 中文

新增功能

- 更新说明优先显示本地缓存，并在后台通过 ETag 检查变化。重启后可立即查看；检查失败时保留上次成功获取的说明。
- 统计区分「生成 TPS」和「全程 TPS」。首字等待从首段正文或工具参数开始计算；生成速度排除已报告的推理 token。历史、缓冲或重试请求如无法可靠测量，显示为未知。

使用变化

- 设置说明先解释三种跨服务子代理模式，再展开速度和耗时对比。对比补齐了配对样本，新增匿名的导入订阅与原生直连对比，并标明每项指标越高越好还是越低越好。

问题修复

- 修复服务柱状图颜色重复的问题，切换 Token／USD 或筛选时配色保持不变。
- 窄屏下调用记录自动换行，展开详情可同时查看两种速度。

### English

New features

- Release notes now load from a local cache first, and EMP checks for changes in the background with ETag. The notes are available right after a restart, and the last successful version stays readable if a check fails.
- Statistics now show Generation TPS and End-to-end TPS separately. First-token delay is measured from the first text or tool-argument content, and generation speed excludes reported reasoning tokens. Historical, buffered or retried calls without reliable timing show the rate as unknown.

Workflow changes

- Settings help now explains the three cross-service subagent modes first, then the speed and completion-time comparisons. The comparisons use complete matched samples, add anonymous results for imported subscriptions versus native direct connections, and state whether higher or lower is better for each metric.

Fixes

- Fix duplicate colors in the service bar chart. Colors now stay the same across Token/USD views and filters.
- Call records wrap in narrow panels, and expanded details show both speed metrics.

## 0.13.7 (2026-10-17)

### 中文

新增功能

- 跨服务子代理新增「关闭 / 按需恢复 / 打开」三种模式，默认关闭。
- 「按需恢复」使用原任务账号的 GPT-6 Luna（low），其调用和用量单独记录。
- 「打开」使用明文任务，可能导致 GPT 生成变慢。
- 处于关闭模式时，相关错误提示会说明在哪里开启。

使用变化

- 设置中的问号说明介绍三种模式的用途，并列出同模型 TPS 和全流程耗时比例。中英文布局均适配窄屏。
- 切换模式后会更新 WebSocket 工具目录。已恢复的任务复用内存缓存，不再重复调用恢复。

问题修复

- 修复配置外部 API 服务后协作工具被自动改写、导致原生 GPT 生成变慢的问题。现在默认保留官方原生协作工具。

### English

New features

- Cross-service subagents now offers three modes: Off / On-demand recovery / On. The default is Off.
- On-demand recovery uses GPT-6 Luna (low) on the task's original account and records its calls and usage separately.
- On sends tasks in plaintext and can slow GPT generation.
- When the mode is Off, related errors explain where to enable it.

Workflow changes

- The question-mark help in Settings explains what each mode is for and shows within-model TPS and full workflow time ratios. The Chinese and English layouts both fit narrow screens.
- Changing the mode refreshes the WebSocket tool catalog. Recovered tasks are reused from memory, so recovery is not called twice.

Fixes

- Fix Native GPT slowing down because collaboration tools were automatically rewritten whenever an external API service was configured. Official native collaboration tools are now preserved by default.

## 0.13.6 (2026-10-17)

### 中文

使用变化

- 界面文案改为中英文字典统一管理。动态参数、无障碍标签和启动恢复页都会跟随所选语言显示。

问题修复

- 修复 Claude 多轮对话在追加新内容后无法重用提示词缓存的问题，减少重复的缓存写入。对话历史、工具调用与结果、图片和文件均完整保留。
- 统一 Pro Lite、Pro、Pro Max 的订阅名称和标签显示。

### English

Workflow changes

- UI text now comes from shared Chinese and English dictionaries. Dynamic values, accessibility labels and the startup recovery page all follow the selected language.

Fixes

- Fix Claude prompt-cache reuse when new turns are added to a conversation, which reduces repeated cache writes. Conversation history, tool calls and results, images and files are all kept intact.
- Pro Lite, Pro and Pro Max subscription names and badges now display consistently.

## 0.13.5 (2026-10-16)

### 中文

新增功能

- 支持 Codex 独立的图片生成与编辑请求，并记录对应的调用和用量。
- 路由设置统一管理自动审查账号和图片账号。图片可选择跟随对话、指定账号或自动选择，同一轮及已有图片会保持原来的账号关联。

使用变化

- 统一弹窗返回行为：子页面只显示返回箭头，返回后上一层状态保留；保存后自动返回。
- 更新提示显示「当前版本 → 最新版本」。更新完成后复用已打开的管理页面，不再重复打开标签页。

问题修复

- 修复 Claude 工具名称和命名空间的还原、各模型的推理强度设置，以及已保存目录的能力刷新。
- 工具定义改由标准输入传递，并使用固定的输出结构，避免工具数量较多时超出 Windows 命令行长度。
- 修复延迟完成的请求会关闭之后打开的弹窗的问题。

### English

New features

- Support standalone Codex image generation and editing requests, with matching call and usage records.
- Routing settings now manage Auto Review and image accounts in one place. Images can follow the conversation, use a selected account, or pick one automatically, and they keep their account affinity within a turn and for existing images.

Workflow changes

- Nested dialogs now show a single Back arrow, the parent dialog keeps its state, and saving returns automatically.
- Update prompts show "current version → latest version". After an update, EMP reuses the open management page instead of opening a duplicate tab.

Fixes

- Fix restoration of Claude tool names and namespaces, model-specific reasoning effort, and capability updates for saved catalogs.
- Tool definitions are now sent through stdin with a fixed output schema, so large tool catalogs no longer exceed the Windows command-line length.
- Fix a late-finishing request closing a dialog that was opened afterwards.

## 0.13.4 (2026-10-15)

### 中文

使用变化

- 连接错误区分代理无法连接、DNS 解析失败和 TLS 连接失败，并给出对应的处理提示。本机 Codex 连接 EMP 与 EMP 连接上游服务分别选择代理。
- 当 Claude 适配器的输入或输出结构超出限制时，错误提示会注明问题来自 EMP，并说明输入限制以及可以调整的内容。

问题修复

- 修复 Claude Code 长对话在压缩历史前就因输入大小限制被拒绝的问题。现在先为可压缩的历史生成摘要再继续回答，当前提问、工具调用和结果保持不变。

### English

Workflow changes

- Connection errors now distinguish unreachable proxies, DNS failures and TLS failures, each with specific guidance. Proxy selection is handled separately for local Codex-to-EMP connections and for EMP-to-upstream connections.
- When a Claude adapter input or output schema exceeds a limit, the error identifies EMP as the source and explains the input limit and what can be adjusted.

Fixes

- Fix long Claude Code conversations being rejected by the input-size limit before history compaction runs. Eligible history is now summarized before the response continues, and the active request, tool calls and results are kept unchanged.

## 0.13.3 (2026-10-14)

### 中文

新增功能

- 新增简体中文／English 启动恢复页。旧配置或密钥无法使用时，可确认清除后重新配置。
- 启动恢复页中的备份为可选项，默认不勾选。中断的重建会自动继续，原生登录和对话保持不变。

使用变化

- 数据迁移改为先打开加密包并预览，再一次确认导入；返回时已选文件保留。相同账号按验证后的身份合并，模型显示与上下文冲突在提交前处理。
- 删除导入账号时立即显示进度，只取消该账号的额度任务。提交后列表立即更新，目录刷新在后台继续。
- 「恢复原生」和「退出 EMP」现在是两个独立操作：恢复后 EMP 保持运行，可随时再次启用；退出时会先还原当前实例管理的原生设置，再结束服务。

问题修复

- Native 模型目录不再写入 Codex 缓存格式，改由 EMP 自己的目录快照管理。
- 改进 Native 额度身份匹配、启动诊断和 Windows 本地地址迁移。

### English

New features

- Add a Simplified Chinese / English startup recovery page. When existing settings or keys can't be used, you can confirm clearing them and set up again.
- The backup option on this page is unchecked by default. Interrupted resets resume automatically, and native sign-in and conversations are kept.

Workflow changes

- Data migration now opens and previews the encrypted package first, then imports after a single confirmation. Going Back keeps the selected file. Identical accounts are merged by verified identity, and model visibility and context conflicts are resolved before anything is committed.
- Removing an imported account shows progress immediately and cancels only that account's quota tasks. The list updates as soon as the removal is committed, and catalog refresh continues in the background.
- Restore Native and Quit EMP are now separate actions. Restore Native keeps EMP running so you can enable it again. Quit EMP first restores the native settings managed by the current instance, then stops the service.

Fixes

- Native model catalogs are now kept in EMP's own snapshots instead of being written in Codex's cache format.
- Improve Native quota identity matching, startup diagnostics and Windows loopback address migration.

## 0.13.2 (unpublished / 未发布)

### 中文

发布状态

- 发布检查发现 Windows CI 使用代理控制台窗口，未发送实际关闭事件；没有发布安装包。本次产品变更随 0.13.3 一起发布，原标签保留作为检查记录。

### English

Release status

- Release validation found that the Windows CI proxy console window did not deliver an actual close event. No packages were published. Version 0.13.3 includes the product changes; the original tag remains as the validation record.

## 0.13.1 (2026-10-12)

### 中文

新增功能

- 诊断信息新增续传原因（不含正文）、实际连接复用情况和应用载荷字节数。同一轮内的全量恢复会关联显示，并分别列出压缩前后大小与网络流量。

问题修复

- 修复原生 WebSocket 的大上下文请求回退到 HTTP 后反复全量重传的问题。可以分段的历史会先通过不生成回答的预热建立状态，之后继续增量续传。
- 完整输入、图片、instructions 以及工具调用与结果的配对均保持不变。账号、路由、会话隔离和真实失效后的恢复照常生效。
- 无法安全分段的历史仍使用原有的回退方式。

### English

New features

- Diagnostics now show continuation reasons (without message content), actual connection reuse and application payload sizes in bytes. Full recoveries within the same turn are linked, and decoded size, encoded size and network traffic are listed separately.

Fixes

- Fix native WebSocket requests with large contexts repeatedly resending the full history after falling back to HTTP. Histories that can be split safely now establish state through a prefill that generates no response, and later turns continue incrementally.
- Complete inputs, images, instructions and tool call/result pairs are preserved. Account, route and session isolation and recovery after a genuine connection loss continue to work as before.
- Histories that cannot be split safely still use the existing fallback path.

## 0.13.0 (2026-10-12)

### 中文

新增功能

- 已确认的 Shadow 界面迁入正式版。
- 模型管理支持按服务查看、搜索、显示／隐藏、编辑、手动添加和测试模型；主页上重复的隐藏模型列表已移除。
- 每个模型可暂存「同步显示与上下文到其他订阅」的设置，点击保存时一并提交。
- 数据迁移可按服务／模型选择，并在导入预览中修改服务名称，一次导入完成提交。
- 更新窗口显示安全渲染的中英文更新说明。
- 新增 `migrate-history --thread-id`，用于迁移明确指定的会话；迁移时保留备份，并拒绝该会话的活动写入。

使用变化

- 新设置默认使用额度圆环样式；已选择的横条／圆环样式和不限额显示保持不变。
- 账户信息与累计 Token／USD 显示更紧凑，服务详情页签统一。返回和关闭按钮改为无边框，返回上级窗口和取消的行为不变。
- 连接表单简化标签。已保存的 API Key 显示为六个星号；输入新值时可见，留空或取消时保留原 Key。
- 订阅上下文说明区分目录默认值、上限和 Codex 可用比例，编辑时实时计算。
- 关闭迁移窗口不会改动原配置，演示模式为只读。
- 统计图纵轴按实际峰值上留 5% 余量。

问题修复

- 重试和协议回退计入所属请求，WebSocket 预热不再计入推理调用和用量。
- 额度辅助进程不再重复下载模型目录。
- 用量读取使用持久的身份映射；错误说明保留来源和原始代码，并给出针对性的历史修复建议。

升级与兼容性

- HTTP 上游支持 gzip／Brotli／Zstandard／deflate 压缩响应，解码后的大小限制、超时和显式 identity 请求保持不变。原生请求压缩和增量 WebSocket 继续兼容。
- 迁移预览中的凭据不会返回浏览器，只有选中的项目会进入加密导出和导入。迁移按账户／连接身份合并，可以区分同名服务。
- 恢复原生前会关闭闲置的 WebSocket，并等待进行中的请求完成；退出时只恢复配置，不批量改写会话。

### English

New features

- The accepted Shadow UI is now the formal UI.
- Model management lets you view, search, show/hide, edit, add and test models per service. The duplicate hidden-model list on the homepage has been removed.
- Each model can stage "sync visibility and context to other subscriptions"; the change is committed when you click Save.
- Data migration lets you select services/models and rename incoming services in the import preview, then commit everything in one import.
- The update window shows safely rendered release notes in Chinese and English.
- Add `migrate-history --thread-id` to migrate a specific conversation. It keeps a backup and refuses to run while that conversation is being written to.

Workflow changes

- New settings default to the quota ring style. Previously chosen bar/ring styles and unlimited-quota displays are kept.
- Account information and lifetime Token/USD totals are more compact, and service detail tabs are unified. Back and Close buttons are now borderless, with the same parent navigation and cancel behavior.
- Connection form labels are simplified. A saved API key shows as six stars; a new key stays visible while you type it, and leaving the field blank or cancelling keeps the existing key.
- Subscription context help distinguishes the catalog default, the maximum and the percentage usable by Codex, and recalculates live while you edit.
- Closing the migration dialog leaves your configuration unchanged, and demo mode is read-only.
- Statistics charts scale the vertical axis to the measured peak plus 5%.

Fixes

- Retries and protocol fallbacks are now counted under the request they belong to, and WebSocket prewarm is excluded from inference calls and usage.
- The quota helper no longer downloads the model catalog repeatedly.
- Usage is read through durable identity mappings. Error messages keep their source and original code and include targeted history-repair guidance.

Upgrade and compatibility

- Upstream HTTP now supports gzip/Brotli/Zstandard/deflate compressed responses. Decoded size limits, timeouts and explicit identity requests are unchanged, and native request compression and incremental WebSocket remain compatible.
- Migration previews never send credentials to the browser, and only selected items are included in encrypted export and import. Migrations merge by account/connection identity and tell apart services that share a name.
- Before Restore Native, EMP closes idle WebSockets and waits for active requests to finish. Quitting restores configuration only and does not rewrite conversations in bulk.

## 0.12.18 (2026-10-10)

### 中文

新增功能

- 正式 Web 界面采用新的展示方式：服务图标显示调用状态与详情，配合品牌颜色、限定范围的光晕和错误气泡。
- 新增左右对称的紧凑额度圆环，并记住所选样式；倒计时以两个单位显示。正式版默认仍为横条，无 5h 限制时仍显示 233% / 1m111s。
- 用量 API 支持筛选后的图表序列；总量与图表分组共用同一份去重记录，服务、模型、会话和调用结果筛选同时生效。

使用变化

- 性能与用量合并为「统计」，使用统一筛选，支持 Token／USD 图表，按服务／模型区分颜色，并可点击查看调用结果。
- 各控件使用可配置的点阵悬停／焦点反馈，语言和外观选择器已对齐；页面不可见或在后台时暂停动效。
- 只读开发者预览与正式界面使用同一套展示，预览的命令隔离和演示内容仍单独保留。

### English

New features

- The official Web UI adopts the new presentation: service artwork shows activity and call details, with brand colors, bounded halos and error bubbles.
- Add mirrored compact quota rings with persistent style selection and two-unit countdowns. Bars remain the default, and the 233% / 1m111s display is kept when there is no 5h limit.
- The usage API adds filtered chart series; totals and chart groups use the same reconciled ledger, and service, model, session and outcome filters apply together.

Workflow changes

- Performance and usage are combined into Statistics with common filters, Token / USD charts, service/model colors and click-through request outcomes.
- Controls use configurable dotted hover/focus feedback, the language and theme selectors are aligned, and effects pause outside the viewport or in the background.
- The read-only developer preview shares the official presentation; its command isolation and demos remain separate.

## 0.12.17 (2026-10-10)

### 中文

新增功能

- 点击请求成功率百分比可打开调用结果，查看筛选后的失败来源、原因和次数；关闭对话框后保留原有报告和筛选。
- 额度查询失败会单独记录，趋势中的空缺会说明失败原因，包括 RPC 阶段、耗时、状态及分类原因。
- 新增只读影子前端 scripts/shadow_frontend，可配置后端和状态路径，预览紧凑额度圆环、统一统计、服务错误气泡、活动图标及浅色／深色交互。

问题修复

- 调用记录、活动和诊断中区分失败请求、历史重发请求、主动中断、断开连接和缺失结果；读取时修正旧版 WebSocket 分类，不改写用量数据。
- 原生 WebSocket 被拒绝和流失败时按实际来源记录，保留上游错误原文，并将上游完成与交付结果分开。
- 请求成功率改为按已完成和失败的请求计算。

### English

New features

- Click the request success percentage to open Call outcomes and inspect filtered failure sources, reasons and counts; the existing report and filters are kept when the dialog closes.
- Failed quota observations are recorded separately, and trend gaps explain the recorded failure, including RPC phase, duration, status and categorized cause.
- Add a read-only shadow frontend under scripts/shadow_frontend with configurable backend/state paths, previewing compact quota rings, unified statistics, service error bubbles, activity artwork and light/dark interactions.

Fixes

- Call records, activity and diagnostics distinguish failed requests, history-resend requests, explicit interruption, disconnects and missing outcomes; legacy WebSocket classifications are corrected on read without rewriting usage.
- Native WebSocket rejections and stream failures are recorded with their actual source, upstream wire errors are preserved, and upstream completion is kept separate from delivery.
- Request success is calculated from completed and failed requests.

## 0.12.16 (2026-10-09)

### 中文

问题修复

- 中英文界面下，EMP 标志、状态和控件均保持在顶栏同一行；窗口较窄或缩放时在顶栏内横向滚动，不再换行或被强制堆叠。

### English

Fixes

- The EMP logo, status and controls stay in one top-bar row in both languages; narrow or zoomed windows scroll within the top bar instead of wrapping or stacking.

## 0.12.15 (2026-10-09)

### 中文

新增功能

- 在 Windows 和 macOS 上检查和下载更新时遵循系统代理，每次尝试读取当前代理设置，并在诊断日志中记录下载进度、重试阶段和安装结果。
- 经 Anthropic 路由时保留 Codex 指令角色，支持增量工具发现，并在 Responses 错误中提供 Retry-After 建议。

使用变化

- 服务详情与连接设置合并在同一个稳定的对话框中，保留已编辑的名称；用量和性能使用一致的刷新控件。

问题修复

- 切换语言或刷新服务数据时保留已加载的 EMP 状态；各缩放级别下中英文顶栏对齐，换行的控件随页面滚动，不再遮挡服务按钮。
- 切换服务、模型草稿或报告视图时取消过期的读取。
- 使用自动分配端口启动时公布实际监听端口，网页设置可以正常保存。

### English

New features

- Checking and downloading updates honors the system proxy on Windows and macOS, reading current proxy settings for each attempt and recording download progress, retry stages and install outcomes in the diagnostic journal.
- Codex instruction roles are preserved through Anthropic routing, with incremental tool discovery and Retry-After advice in Responses errors.

Workflow changes

- Service details and connection settings share one stable dialog that retains edited names; Usage and Performance use consistent refresh controls.

Fixes

- The loaded EMP status is kept when changing language or refreshing service data; English and Chinese top bars align at browser zoom levels, and wrapped controls scroll with the page instead of covering service buttons.
- Stale reads are cancelled when switching services, model drafts or report views.
- When starting with an automatically assigned port, the actual listening port is published so web settings can be saved.

## 0.12.14 (2026-10-09)

### 中文

新增功能

- 原生 WebSocket 连接支持 Codex 引导控制；中断时 HTTP／Claude 适配器可干净取消。
- 用量按服务类型和模型分组，各服务的累计用量按模型合并；请求详情与性能共享会话、所选／发送／上报模型、Token、缓存用量、耗时、TTFT、TPS、重试和交付等记录。
- 点击服务徽章即可打开服务设置。

问题修复

- 无状态预热响应不再误称可恢复历史，保留首轮 WebSocket 对话；Windows 上 WebSocket 连接在空闲读取和续传时保持连接。
- 调用详情和诊断回执区分 EMP、上游、连接及 Claude CLI 错误，保留上游原生响应并记录交付结果，不记录提示词、响应或凭据。
- Codex 发起的请求与 Claude CLI 的指令、工具、技能及本地项目设置相互隔离；输出预算耗尽时及时报告，并应用模型上下文预算，不额外引入 Claude 自身的压缩或工具执行。
- 改进浏览器缩放布局，Credit 使用自然宽度；Codex 返回不含 5 小时窗口的额度数据时仍显示 233% / 1m111s。

### English

New features

- Native WebSocket connections support Codex steering controls; HTTP/Claude adapters cancel cleanly when interrupted.
- Usage is grouped by service type and model, with each service's lifetime usage merged by model; request details and Performance share session, selected/sent/reported models, tokens, cache usage, duration, TTFT, TPS, retries and delivery.
- Click the service badge to open service settings.

Fixes

- Stateless warmup responses no longer falsely claim resumable history, preserving the first WebSocket turn; Windows WebSocket connections persist across idle reads and continuation.
- Call details and diagnostic receipts distinguish EMP, upstream, connection and Claude CLI errors, preserve native upstream responses and record delivery outcomes without logging prompts, responses or credentials.
- Claude CLI instructions, tools, skills and local project settings are isolated from Codex-owned requests; exhausted output budgets are reported promptly and model context budgets are applied without adding Claude's own compaction or tool execution.
- Browser-zoom layouts are improved, Credit uses natural widths, and the 233% / 1m111s display is kept when Codex returns quota data without a five-hour window.

## 0.12.13 (2026-10-08)

### 中文

新增功能

- 在「Services」中统一管理 Codex 订阅、Claude Code 连接和 API Provider，通过一个「Add service」入口添加，图标保持对齐。
- 打开「Model settings」可复用已保存的 Provider 模型列表，点击「Update model list」显式刷新；可通过 Claude Code 发现本地订阅模型，无需发起推理。
- 可刷新并查看本地 Claude 订阅额度趋势；在「Settings」中可将所有额度条和趋势在剩余量与已用量之间切换。
- 导入 Codex 订阅时可粘贴 auth.json 内容或选择该文件。

使用变化

- EMP 状态、启用、性能和用量控件移至顶栏；服务和模型行更紧凑，中英文下拉选择文字居中。

### English

New features

- Manage Codex subscriptions, Claude Code connections and API Providers together in Services, with one Add service entry and aligned provider icons.
- Open Model settings to reuse a saved Provider model list, and use Update model list to refresh it explicitly; local subscription models are discovered through Claude Code without starting an inference turn.
- Refresh and view local Claude subscription quota trends, and switch all quota bars and trends between remaining and used amounts in Settings.
- Import Codex subscriptions by pasting auth.json or selecting the file.

Workflow changes

- EMP status, activation, performance and usage controls move into the top bar; service and model rows are more compact, and Chinese and English select text is centered.

## 0.12.12 (2026-10-05)

### 中文

问题修复

- 修复在含图片或文档的历史中添加提示缓存标记时 Claude CLI 请求返回 502 的问题，保留原始内容、工具顺序和缓存元数据。
- 压缩后紧接失败轮次时恢复正确的对话检查点，在分叉边界内保留继承的父级历史，旧摘要不再覆盖更新的已提交内容。
- 尚未观测到原生模型目录时，Codex 可仅凭外部模型目录启动；原生目录可用时保留其提供的设置。
- HTTP、SSE 和 WebSocket 诊断记录具体的历史恢复和压缩失败原因，不记录对话或凭据。

### English

Fixes

- Fix Claude CLI requests returning 502 when prompt-cache markers are added to image or document histories, preserving original content, tool order and cache metadata.
- Restore the correct conversation checkpoint after compaction followed by a failed turn, retain inherited parent history within the fork boundary, and prevent an older summary from replacing newer committed work.
- Codex can start with an external-only model catalog before a native catalog has been observed; settings from available native catalogs are preserved.
- HTTP, SSE and WebSocket diagnostics record specific history-recovery and compaction failure reasons without recording conversations or credentials.

## 0.12.11 (2026-10-05)

### 中文

问题修复

- 额度查询设有时限，可干净取消独立的 Codex 辅助进程（包括保留输出管道的启动器子进程）；查询超时或取消时保留已轮换的凭据。
- Codex 设置仍指向旧的或无法解析的 EMP 监听地址时，EMP 保持可用，以便退出前完成原生配置恢复。
- 针对所选 Provider 和模型说明失败原因，保留 HTTP 状态、重试上限和用户的路由选择。
- 恢复 Claude 4.6 推理等级，接受省略或为 null 的 effort，兼容处理 Claude CLI 系统提醒且不丢失工具历史；不支持的显式 effort 值会在转发前说明。
- 诊断可跨阶段关联请求与管理操作，区分已保存设置、上游结果和响应交付；已取消的空响应不再报告为已交付，生成的 SSE 响应记录实际的最终事件。

### English

Fixes

- Quota queries are bounded and their isolated Codex helpers are cancelled cleanly, including launcher children that retain output pipes; rotated credentials are preserved when a query times out or is cancelled.
- EMP stays available when Codex settings still point to an older or unresolved EMP listener, so native restoration can finish before exit.
- Failures are explained for the selected provider and model while preserving HTTP status, retry limits and the user's route choice.
- Claude 4.6 reasoning levels are restored, an omitted or null effort is accepted, and compatible Claude CLI system reminders are handled without losing tool history; unsupported explicit effort values are explained before forwarding.
- Diagnostics correlate request and management-operation stages and distinguish saved settings, upstream outcomes and response delivery; cancelled empty responses are no longer reported as delivered, and generated SSE responses record the actual final event.

## 0.12.10 (2026-10-04)

### 中文

使用变化

- 请求和流结束时的清理延迟更短，仅在需要时监测断开，请求结束后立即唤醒；保留取消、未读的套接字数据和原有超时设置。
- 外部 HTTP、SSE 和 WebSocket 请求统一处理重试，保留协议回退、重试上限、原生响应字段和对话行为。
- 新增可选的本地性能分析，常规构建默认关闭。

问题修复

- 账户重新导入或移除无法提交时，保留待处理的已轮换凭据；配置、凭据文件和已发布状态保持各自的事务与回滚边界。
- 关闭诊断对话框后忽略迟到的诊断响应，管理页外观与控件保持不变。

### English

Workflow changes

- Request and stream teardown is faster: disconnect monitoring starts only when needed and wakes immediately when a request ends, preserving cancellation, unread socket data and the previous socket timeout.
- External HTTP, SSE and WebSocket requests share unified retry handling, preserving protocol fallback, retry limits, native response fields and conversation behavior.
- Optional local profiling is added; normal builds keep it disabled.

Fixes

- Pending rotated credentials are preserved when account reimport or removal cannot be committed; configuration, credential files and published state keep their transaction and rollback boundaries.
- Late diagnostic responses are ignored after the dialog closes; the management page keeps its existing appearance and controls.

## 0.12.9 (2026-10-02)

### 中文

问题修复

- 账号的 Credit 与重置次数数字现与 12px 的额度百分比字号一致，不再被账号标题样式放大。

### English

Fixes

- Account Credit and reset-count numbers now match the 12px quota percentages and are no longer enlarged by account-title styles.

## 0.12.8 (2026-10-01)

### 中文

新增功能

- 账号身份、订阅套餐、Credit 与重置次数合并为一行紧凑摘要；打开账号详情可查看已记录的模型/Token 用量与 API 等价费用，保存别名、选择重置或移除导入的账号。
- 点击账号、Provider 和模型的活动圆点可打开最近请求详情，显示所选模型、实际目标、上游声明的模型、耗时、分派次数与重试时间线；这些内存中的有限记录不保存对话和凭据。

使用变化

- 设置中将自动激活、外部模型 Codex 网页搜索与自动审查账号回退归为一组，三项默认开启，已保存的关闭选择保持不变。
- 模型的显示与隐藏改用眼睛按钮，与其他管理界面一致；所有账号行的 Edit、Refresh 和 Trend 按钮保持对齐。
- 检查更新与下载遇到临时失败时最多额外重试三次，并显示重试进度、具体失败阶段和相应恢复步骤；中断的下载从头重新开始并保留大小/校验和验证，安装、权限或校验和失败不重试。

问题修复

- 修复滑动开关在不同浏览器缩放比例下轨道高度与圆点对齐异常。

### English

New features

- Account identity, subscription plan, Credit and reset count are combined into one compact summary; open account details to view recorded model/token usage and API-equivalent cost, save an alias, choose a reset or remove an imported account.
- Click account, Provider and model activity dots to open recent request details showing the selected model, actual target, upstream-declared model, duration, dispatch count and retry timeline; these bounded in-memory receipts exclude conversations and credentials.

Workflow changes

- Settings groups automatic activation, external-model Codex web search and automatic-review account fallback; all three are enabled by default and saved opt-outs are kept.
- Models are shown or hidden with eye buttons, matching the rest of the management UI; Edit, Refresh and Trend stay aligned across all account rows.
- Temporary release-check and download failures are retried up to three more times, with retry progress, the specific failure stage and relevant recovery steps shown; partial downloads restart from zero with size/checksum validation kept, and installation, permission or checksum failures are not retried.

Fixes

- Fixed sliding-switch track height and circle alignment at different browser zoom levels.

## 0.12.7 (2026-09-30)

### 中文

使用变化

- 账号身份、Credit 与重置控件更紧凑且不隐藏主要操作；共享 Credit 金额的宽度按可见账号调整。
- 计划中的额度刷新保持不变，历史扫描的确认与完成状态通过现有事件流显示。

问题修复

- 刷新后的目录遗漏已知订阅模型时仍保留其路由，现有对话不再因本地未知模型错误而失败；保留的元数据按账号和后端隔离，并优先使用最新的模型设置。
- 修复 Claude Code 在系统提醒中重复推理强度时，Claude 请求在转发前被拒绝的问题；仍保留请求设置与精确对话检查，冲突设置和意外字段会被拒绝。
- Claude CLI 失败会记录具体错误码、HTTP 状态与耗时，不保存对话或凭据。

### English

Workflow changes

- Account identity, Credit and reset controls are more compact without hiding the main actions; shared Credit amounts are sized to the visible accounts.
- Scheduled quota refreshes are kept, and history-scan acknowledgement and completion appear through the existing event stream.

Fixes

- Known subscription models remain routable when a refreshed catalog omits them, so existing conversations no longer fail with a local unknown-model error; retained metadata is isolated by account and backend, and fresh model settings take precedence.
- Fixed Claude requests being rejected before forwarding when Claude Code repeats its reasoning effort on a system reminder; the request setting and exact conversation checks are preserved, and conflicting settings or unexpected fields are rejected.
- Claude CLI failures are recorded with their specific error code, HTTP status and duration, without storing conversations or credentials.

## 0.12.6 (2026-09-30)

### 中文

新增功能

- 保存最近一次更新失败的阶段、HTTP 状态或可用的系统错误码；该记录在暂存清理和重启后仍保留，更新对话框会显示具体失败阶段。

使用变化

- Credit 与重置控件位于共享的居中账号列，宽度固定、金额右对齐。
- Delete 移到 Edit、Refresh 和 Trend 之前，使 Native 与导入账号的按钮位置一致；小屏幕上完整的控件组保持在一起。

### English

New features

- The latest update failure is saved with its stage, HTTP status or available system error code; this receipt survives staging cleanup and restarts, and the update dialog shows the specific failure stage.

Workflow changes

- Credit and reset controls sit in a shared central account column with stable widths and right-aligned amounts.
- Delete now precedes Edit, Refresh and Trend so button positions match for Native and imported accounts; complete control groups stay together on smaller screens.

## 0.12.5 (2026-09-30)

### 中文

新增功能

- 在 CPA 与本地 Claude 连接中，为已知 Claude 模型补回缺失的推理级别和图像能力，现有模型加载时获得默认值，Provider 限制与手动设置仍优先；模型表单新增可编辑的推理级别，并发布到 Codex 模型选择器。
- 有新版本时 Check updates 显示红点，打开管理页面时会静默后台检查。
- 启用状态的 EMP 点阵 Logo 以灰绿色动画显示；账号、Provider 和模型的实际请求活动以更亮的脉冲和光晕呈现，并遵循减少动态效果设置。

使用变化

- Codex 版本与模型状态显示在同一行，详细的应用与引擎信息移到诊断中；订阅模型编辑器更紧凑，Credit 与重置控件移到账号标题，活动圆点统一放在名称前。

问题修复

- 修复 Windows 上更新交接时，等待已退出但进程句柄仍被启动器持有的 EMP 的问题。

### English

New features

- Restores missing reasoning levels and image capabilities for known Claude models in CPA and local Claude connections; existing models gain the defaults on load while provider restrictions and manual settings stay authoritative. The model form adds editable reasoning levels, published to the Codex model picker.
- Check updates shows a red dot when a newer release is available, with a quiet background check when the management page opens.
- The enabled EMP dot-matrix logo animates in gray and green; actual account, Provider and model request activity shows a brighter pulse and halo, respecting reduced-motion settings.

Workflow changes

- Codex version and model status appear on one line, with detailed application and engine information moved to diagnostics; the subscription model editor is more compact, Credit and reset controls move into account headers, and activity dots sit before names throughout the lists.

Fixes

- Fixed Windows update handoff waiting on an already-exited EMP whose process handle was still held by its launcher.

## 0.12.4 (2026-09-30)

### 中文

新增功能

- 新增两条经已安装 Claude Code CLI 的 Claude 路由：本地 Claude 订阅复用当前系统用户的 Claude Code 登录，EMP 在每次请求前检查，CLI API Key 和 Console 认证不适用此模式，请通过 Claude Code 登录；CPA 使用兼容 Claude Code 的 Base URL 与 API Key。本地登录的模型别名或完整 ID 需手动添加，直接使用 Anthropic API 仍为单独的 Anthropic API Provider。
- 仅当 Claude 模型的已配置能力和所选上游支持时，才转发图像输入和支持的内联文档内容；工具调用由 Codex 执行，回复在生成结束前保持缓冲。
- 从已认证的 Codex 目录响应自动同步原生与订阅模型目录及能力，刷新失败时保留上次可用数据，并保留可见性、别名与上下文覆盖；新开放的模型无需 EMP 新版或手动编辑模型列表。在 Codex 0.158 中，已打开的选择器可能需要重新加载，EMP 会显示重载提示。
- 模型、Provider 和订阅账号旁显示请求活动：活跃圆点表示已分派的请求，悬停显示进行中数量；最近圆点表示已完成的工作。活动来自已认证事件流，断开连接时清除活跃状态。

使用变化

- 识别 ChatGPT App 在 macOS 中嵌套的引擎、已验证的 Windows 引擎缓存以及共享 Applications 安装中已签名的 macOS App 引擎；App/扩展与引擎版本分开显示，在使用时检查具体操作，不再拒绝所有低于 Codex 0.158 的客户端，无法获取引擎版本时保留现有模型目录。
- 新增一个全局偏好，在 Native、Subscription 与 External Provider 目录路由中显示或隐藏上下文标签；该设置不改变模型上下文窗口。
- 模型搜索从放大镜按钮展开，按 Escape 清空并收起，支持减少动态效果。
- 目录控制接口不可用时报告为未验证目录，而非声称 App 已停止；原生恢复或退出需等待时说明活动对话锁，EMP 保持运行。

升级与兼容性

- 修复从旧版 Windows 版本的升级，不更改账号密钥或加密凭据，写入私有状态不再需要更改文件所有权的权限。
- 不含上下文标签偏好的旧迁移包会保留目标端的现有设置。

### English

New features

- Adds two Claude routes through the installed Claude Code CLI: Local Claude subscription reuses the current OS user's Claude Code sign-in, which EMP checks before each request (CLI API-key and Console auth do not qualify; sign in through Claude Code); CPA uses a Claude-Code-compatible Base URL and API key. Add local-login model aliases or full IDs manually; direct Anthropic API remains a separate Anthropic API Provider.
- Image inputs and supported inline document content are forwarded only when the Claude model's configured capabilities and the selected upstream support them; Codex executes tool calls, and replies stay buffered until generation finishes.
- Native and subscription model catalogs and capabilities sync automatically from authenticated Codex catalog responses, keeping last-good data on refresh failure and preserving visibility, aliases and context overrides; newly exposed models need no EMP release or manual model-list edit. In Codex 0.158 an already-open picker may need reloading, and EMP shows its reload notice.
- Request activity appears beside models, Providers and Subscription accounts: active dots mark dispatched requests with the in-flight count in their tooltip, and recent dots mark completed work. Activity comes from the authenticated event stream; a disconnect clears active status.

Workflow changes

- Recognizes the ChatGPT App's nested macOS engine, the verified Windows engine cache and signed macOS App engines in shared Applications installations; App/extension and engine versions are shown separately, operations are checked when used instead of rejecting every client below Codex 0.158, and existing model catalogs are kept when an engine version cannot be observed.
- One global preference shows or hides context labels across Native, Subscription and External Provider catalog routes; it does not change model context windows.
- Model search expands from a magnifier control; Escape clears and collapses it, with reduced-motion support.
- An unavailable catalog control interface is reported as an unverified catalog rather than a stopped App; when native restoration or exit must wait, EMP explains the active conversation locks and keeps running.

Upgrade and compatibility

- Fixed upgrades from older Windows releases without changing account keys or encrypted credentials; private state is written without needing permission to change file ownership.
- Old migration bundles without the context-label preference keep the destination's existing value.

## 0.12.3 (2026-09-29)

### 中文

新增功能

- 在 Windows、Linux、macOS Intel 与 Apple Silicon 上发现官方 VS Code 扩展、托管 Codex 守护进程和 ChatGPT 桌面应用运行时；Windows 上解析 npm 的原生载荷而不依赖桌面 Shell 的 Node 路径，并选择 CLI 资源而非启动应用界面。
- 从桌面启动器以精简 `PATH` 启动 EMP 时，可在已知的 Unix nvm 位置找到 npm 安装的 Codex，并使用其可信 Node 解释器，无需手动符号链接；`PATH` 上已有的 Codex 优先于 nvm 回退查找。
- 恢复原生设置前转换受影响的 EMP 所属压缩检查点（含受支持的父、子、孙级历史），改写前备份每个 rollout、恢复中断的写入，并拒绝不安全或不受支持的更改；修复只针对受影响的对话，无关的未完成对话不会阻碍恢复。

使用变化

- 恢复前请先结束进行中的对话并关闭 Codex，然后在 Web UI 中点击 **Restore Native**，或在命令行运行 `EMP restore`。
- 分开显示已保存的集成设置和运行中的 Codex 模型目录。目录检查只报告模型可见性，启动时重新查看当前状态。
- 隔离的额度与重置辅助进程禁用远程插件同步，避免反复下载插件包；普通 Codex 插件与账号隔离不受影响。

问题修复

- 缓存的 Codex 安装在可执行文件被替换、删除或不再可信时会重新检查，使额度查询能选到可用运行时。
- 显式处理 Codex 0.158.0 的数值推理控制：不兼容的外部协议映射会返回错误，而非静默忽略该设置。

升级与兼容性

- 基于 Codex 0.158.0 验证，最低接受的客户端版本提高到 0.158.0；更旧的客户端会收到更新提示。

### English

New features

- Discovers the official VS Code extension, managed Codex daemon and ChatGPT desktop app runtimes on Windows, Linux, macOS Intel and Apple Silicon; on Windows, npm's native payload is resolved without relying on a desktop shell's Node path, and the CLI resource is selected instead of launching the app GUI.
- When EMP starts from a desktop launcher with a minimal `PATH`, npm-installed Codex is found in known Unix nvm locations using the installation's trusted Node interpreter, with no manual symlinks; an existing Codex on `PATH` takes precedence over nvm fallback discovery.
- Before restoring native settings, affected EMP-owned compaction checkpoints are converted, including supported parent, child and grandchild histories; each affected rollout is backed up before rewriting, interrupted writes are recovered, and unsafe or unsupported changes are refused. Repair is limited to affected conversations, so unrelated unfinished ones do not block restoration.

Workflow changes

- Finish active conversations and close Codex before restoring, then use **Restore Native** in the Web UI or run `EMP restore` from the command line.
- View saved integration settings and the running Codex catalog separately. The catalog check reports model visibility and refreshes its observation at startup.
- Isolated quota and reset helpers disable remote plugin synchronization, avoiding repeated plugin bundle downloads; normal Codex plugins and account isolation are preserved.

Fixes

- Cached Codex installations are rechecked when their executable is replaced, removed or no longer trusted, so quota queries can select a usable runtime.
- Codex 0.158.0 numeric reasoning controls are handled explicitly: incompatible external protocol projections return an error instead of silently dropping the setting.

Upgrade and compatibility

- Validated against Codex 0.158.0, and the minimum accepted client version is raised to 0.158.0; older clients receive an update message.

## 0.12.2 (2026-09-28)

### 中文

新增功能

- 集成卡片在 Codex 连接到 EMP 时读取其 `model/list`，显示实际加载状态：「Codex is using EMP」「Restart」或「Codex is not running」；状态通过页面事件流更新，不再轮询。
- `EMP --emp-migrate-config SOURCE TARGET` 可将旧配置、其状态及引用的文件复制到桌面配置目录，并备份被替换的内容。

使用变化

- 重启提示始终指向 Codex，仅在 Codex 不受支持时显示版本说明；新增自动消失的通知和动画 Logo。
- 外部模型按 Codex 的 95% 有效上下文窗口显示。
- 带前缀的路由可计价，未知模型按 0 计费，并新增价格参考；错误信息会说明是密钥、网络还是服务出错；支持按模态测试模型，额度周期选择器统一共用。

升级与兼容性

- 支持 Codex 0.149.0 及以上所有版本，无需选择运行时。
- Linux 安装程序不再需要 Python，改用 `curl`、`tar`、`gzip` 和 `sha256sum`，读取新发布的 `EMP-linux-x86_64.tar.gz.sha256`，并由 EMP 迁移配置。
- 仓库改为纯 Rust：Python 服务及其测试与工具已移除，保留在 `python_archive` 分支。

### English

New features

- The integration card reads `model/list` from Codex when Codex reaches EMP and shows what it actually loaded: "Codex is using EMP", "Restart" or "Codex is not running"; updates arrive over the page's event stream instead of polling.
- `EMP --emp-migrate-config SOURCE TARGET` copies an older configuration, its state and referenced files into the desktop configuration directory, backing up anything it replaces.

Workflow changes

- Restart messages always refer to Codex, and the version note appears only when Codex is unsupported; adds an auto-dismissing notice and an animated logo.
- External models are shown with Codex's 95% effective context window.
- Prefixed routes are priced, unknown models count as 0, and pricing references are added; error messages state whether the key, network or service failed; models can be tested per modality, and quota period pickers are shared.

Upgrade and compatibility

- Supports every Codex from 0.149.0 on without a runtime selector.
- The Linux installer no longer needs Python: it uses `curl`, `tar`, `gzip` and `sha256sum`, reads the newly published `EMP-linux-x86_64.tar.gz.sha256`, and migrates configurations with EMP.
- The repository is Rust-only: the Python service, its tests and tools are removed and remain on the `python_archive` branch.

## 0.12.1 (2026-09-25)

### 中文

新增功能

- 发布版改为 Rust 重写的原生 EMP 服务：`main` 分支从 Rust 工作区构建，Python 源码移至 `python_archive` 分支。
- 流式读取 Codex rollout 历史并有界重建，不再加载整个文件；分页恢复时支持反向扫描与 Codex 风格的检查点重放。

使用变化

- 外部 429/504 处理：遵循 `Retry-After`（最长 300 秒），缺失时指数退避（500 毫秒起，最高 8 秒，抖动不超过 25%）；容量类 429 会重试，额度耗尽仍直接终止，免费路由从不重试。
- Gemma 系列模型在 Codex 输出中将推理内容与回答文本分开。

问题修复

- 下游客户端断开时立即取消 Responses-WebSocket 轮次，并马上释放原生与外部上游流。

### English

New features

- Releases now ship the native EMP service from the Rust rewrite: the `main` branch builds from the Rust workspace, and the Python source moves to the `python_archive` branch.
- Codex rollout history is streamed with bounded reconstruction instead of loading the whole file, with reverse scan and Codex-style checkpoint replay for paginated resumes.

Workflow changes

- External 429/504 handling honors `Retry-After` up to 300 s and otherwise backs off exponentially (500 ms base, 8 s cap, up to 25% jitter); capacity 429s are retried, quota exhaustion stays terminal, and free routes are never retried.
- Gemma-family models separate reasoning from answer text in Codex output.

Fixes

- Responses-WebSocket turns are cancelled as soon as the downstream client disconnects, immediately releasing native and external upstream streams.

## 0.11.11 (2026-09-28)

### 中文

新增功能

- Codex Voice 现经由 EMP 路由，重启时会迁移原生 Voice sideband。
- 诊断中新增已脱敏的支持报告。
- 可选择要使用的配额重置额度并需确认，选择器中以 ISO 日期显示到期时间。

使用变化

- 每次启动 EMP 都使用同一个按用户划分的配置路径。

问题修复

- 管理界面和 API 改由一次性引导链接与按浏览器区分的会话保护；现有 cookie 会话页面可继续使用，会话轮换失败时不再消耗登录链接。
- 配额历史按上游账户身份记录；删除或复用某个本地账户 id 前，其下保存的历史会归属到当时记录它的凭据，否则丢弃；原生登录下记录的历史不会分配给当前登录者。
- 仅当 Codex 可执行文件路径上的每一项都归 root 或当前用户所有时才信任它；拒绝 set-id 程序；组可写的路径项所属组不得有其他成员。
- 修复 Codex Voice 订阅握手；绑定本地 EMP 监听地址时不再进行反向 DNS 查询；服务端网页搜索不再出现在外部工具 schema 中。

升级与兼容性

- 新导出的 `.emp` 迁移包使用更强的密钥派生（envelope v2）加密，导出密码至少 12 字节；旧迁移包和 8 字节密码仍可导入，但 EMP 0.11.10 及更早版本无法导入 v2 迁移包。

### English

New features

- Codex Voice is now routed through EMP, and the native Voice sideband is migrated on restart.
- Diagnostics now include a redacted support report.
- Quota reset credits can be selected and require confirmation, with ISO-dated expiry times shown in the picker.

Workflow changes

- Every EMP launch now uses one per-user configuration path.

Fixes

- The management UI and API are now protected by a one-time bootstrap link and per-browser sessions; existing cookie-session pages keep working, and a failed session rotation no longer consumes the login link.
- Quota history is recorded by upstream account identity; before a local account id is deleted or reused, history saved under it is attributed to the credentials that recorded it or dropped, and history recorded under the native login is never assigned to whoever is signed in now.
- A Codex executable is trusted only when every entry on its path is owned by root or the current user; set-id binaries are rejected, and group-writable entries must belong to groups with no other members.
- Fixed the Codex Voice subscription handshake; binding the local EMP listener no longer performs a reverse DNS lookup; server-side web search no longer appears in external tool schemas.

Upgrade and compatibility

- New `.emp` migration bundles are encrypted with stronger key derivation (envelope v2) and require export passwords of at least 12 bytes; earlier bundles and 8-byte passwords still import, but EMP 0.11.10 and older cannot import v2 bundles.

## 0.11.10 (2026-09-23)

### 中文

使用变化

- 模型 TPS 改为以全部已报告输出 token 除以完整请求时长计算，与 OMP 的稳定吞吐指标一致，适用于缓冲输出和推理模型；TTFT 仍单独测量。

问题修复

- Codex 工作区路由发现失败时，会按 DNS、VPN/TUN、代理和连通性分类给出可操作的指引，且不暴露后端细节。
- 仅限导入的订阅：工作区路由出现包装后的失败时，会启用凭据刷新重试一次，保留轮换后的凭据，并区分登录状态过期与真正的传输故障；原生凭据读取仍保持只读。

### English

Workflow changes

- Model TPS is now calculated from all reported output tokens over the complete request duration, matching OMP's stable throughput metric for buffered and reasoning models; TTFT is still measured separately.

Fixes

- Codex workspace-routing discovery failures are classified with actionable DNS, VPN/TUN, proxy, and connectivity guidance without exposing backend details.
- For imported subscriptions only, a wrapped workspace-routing failure is retried once with credential refresh enabled, preserving rotated credentials and distinguishing expired login state from a true transport failure; native credential reads remain read-only.

## 0.11.9 (2026-09-23)

### 中文

新增功能

- Codex 自动审批复核在请求时经可用订阅路由，原生订阅有配额时优先使用，失败或配额耗尽的账户会被移开，无需重启 Codex。

使用变化

- 模型显示简化为模型名和路由 slug，并提供紧凑的编辑器用于修改显示名称和上下文标签。

问题修复

- `.emp` 导出/导入会保留外部模型的视觉能力值及其来源；若导入会悄然改变这些值，将在生效前被拒绝。
- 未报告上限的彩虹配额条与带重置倒计时的已报告配额条保持垂直对齐。

### English

New features

- Codex automatic approval reviews are routed through an available subscription at request time, preferring Native while it has quota and moving failed or exhausted accounts aside without a Codex restart.

Workflow changes

- Model display is simplified to model names and route slugs, with a compact editor for display names and context labels.

Fixes

- `.emp` export/import preserves external-model vision capability values and their provenance, rejecting an import before it can silently change them.
- Unreported rainbow quota bars stay vertically aligned with reported quota bars that also show a reset countdown.

## 0.11.8 (2026-09-23)

### 中文

使用变化

- 未报告上限的配额窗口显示 233%，并带持续流动的彩虹填充和刷新式光带扫过，保留说明标签。
- 「Usage & estimates」使用独立的饼图样式操作图标，与相邻 EMP 操作风格一致，不再沿用配额趋势图标。

升级与兼容性

- Linux 不再提供 DEB 下载，改为经过校验的中英双语用户安装程序，可明确选择是否移除旧版软件包以及是否迁移原有配置。

### English

Workflow changes

- Quota windows with no reported limit show 233% with a continuously moving rainbow fill and a refresh-style light sweep, keeping the explanatory label.
- Usage & estimates now has its own pie-style action icon, consistent with neighboring EMP actions instead of reusing the quota-trend chart symbol.

Upgrade and compatibility

- Linux DEB downloads are replaced by a verified bilingual user installer that offers explicit choices for removing the legacy package and migrating the source config.

## 0.11.7 (2026-09-23)

### 中文

使用变化

- 报告了 30 天配额窗口的订阅视为 Free 订阅，只显示该窗口，不再显示不可用的 5 小时和 7 天占位。
- 配额趋势默认打开在所选时间范围内有观测数据的窗口，不再打开空的历史窗口。

升级与兼容性

- 跟进 Codex CLI 0.156.x，推荐使用 0.156.1，订阅目录刷新后可选用原生 GPT-6 Sol 和 GPT-6 Luna 模型。

### English

Workflow changes

- A subscription reporting a 30-day quota window is treated as Free and shows only that window instead of unavailable 5-hour and 7-day placeholders.
- Quota trends default to a window with observations in the selected time range instead of opening an empty historical window.

Upgrade and compatibility

- Tracks Codex CLI 0.156.x and recommends 0.156.1, so subscription catalog refreshes expose the native GPT-6 Sol and GPT-6 Luna model choices.

## 0.11.6 (2026-09-21)

### 中文

新增功能

- 粘贴外部服务商 URL 时，无论是裸域名、`/v1` 还是具体的 Responses/Chat Completions 端点，都会规范化为 EMP 所需路由。
- 可通过官方 Codex app-server 方法使用可用的配额重置额度，需明确确认，带幂等键，并立即刷新配额。

使用变化

- 主要管理与模型操作保持可见，服务商、模型和订阅卡片更紧凑、对齐更一致。
- 订阅身份与套餐合并为一个分段徽章，5 小时与 7 天配额并排显示；未报告上限以重复斜线标示，在配额条外也可见。
- 配额历史记录订阅套餐，额度显示为紧凑徽章，已报告的重置时间同时显示UTC 时间戳和剩余时长。

问题修复

- Codex 0.155.0 的原生 WebSocket TLS 握手失败时切换到 HTTP 一次，避免循环重连。

### English

New features

- Pasted external-provider URLs, whether a bare origin, `/v1`, or a concrete Responses/Chat Completions endpoint, are normalized into the route EMP needs.
- An available quota-reset credit can be consumed through the official Codex app-server method with explicit confirmation, an idempotency key, and an immediate quota refresh.

Workflow changes

- Main management and model actions stay visible, while provider, model, and subscription cards are more compact and consistently aligned.
- Subscription identity and plan appear as one segmented badge, 5-hour and 7-day quotas sit side by side, and an unreported limit is marked with repeated slashes that remain visible outside the quota bar.
- Quota history records subscription plans, credits appear as compact badges, and reported reset times are shown as both a UTC timestamp and a remaining duration.

Fixes

- If the native WebSocket TLS handshake fails in Codex 0.155.0, switch to HTTP once to avoid a reconnect loop.

## 0.11.5 (2026-09-21)

### 中文

新增功能

- 外部模型的输入模态现在可以编辑，图像支持可标记为“支持”“不支持”或“未知”。
- 新增可选的内置图标测试。测试成功只作为观察结果，不会自动修改模型元数据。

使用变化

- 使用 Chat Completions 的模型，其推理内容会作为独立的 Codex 流条目保留，不再并入可见回答。
- README 已重写，围绕两大主要用途展开：在同一个 Codex 模型选择器中使用多个订阅，以及以接近原生的流程使用外部 API 模型。

问题修复

- 外部提供商 URL 中重复的 `/v1` 段会被自动规范化。
- 读取导入订阅的额度时，不再强制轮换凭据。
- 诊断信息会记录有界的流失败阶段和重试次数，不存储请求或响应内容。缺少终止事件时仍按失败处理。

### English

New features

- Input modalities for external models can now be edited, and image support can be marked as supported, unsupported, or unknown.
- Added an opt-in bundled-icon test. A successful reply only counts as an observation and does not silently change model metadata.

Workflow changes

- For Chat Completions models, reasoning is kept as separate Codex stream items instead of being folded into the visible answer.
- The READMEs are rewritten around the two main uses: one Codex model picker for multiple subscriptions, and external API models with a native-like workflow.

Fixes

- Duplicate `/v1` segments in external provider URLs are now normalized.
- Reading imported subscription quota no longer forces credential rotation.
- Diagnostics record a bounded stream failure phase and retry count without storing request or response content. Missing-terminal handling still fails closed.

## 0.11.4 (2026-09-18)

### 中文

使用变化

- 当上游目录提供 GPT-5.5 时，订阅列表和上下文编辑中会重新显示该模型。用户已选择隐藏的模型保持隐藏。
- 原生连接和模型目录按所选用户或工作区所有者隔离。
- 对外部模型：配置的 JSON Schema 输出会转换给 Chat Completions 和 Anthropic，并保留 Anthropic 支持的推理强度。官方 OpenAI Responses 端点会保留 store、include 和 cache 设置。
- 改善低并发 WebSocket 的首字等待时间。

问题修复

- 原生 WebSocket 请求如果在产生任何响应事件前被对端以 1009 关闭，EMP 会在内部改用 HTTP/zstd 恢复该请求。该本地连接和路由的后续完整请求会继续走 HTTP；请求被接受或已有输出后不会重放。
- 已知原生模型别名与请求的目录 slug 保持一致，避免误报账户风险警告，同时仍会如实反映真实的模型变化。
- 保留原生失败代码和输出前的 HTTP 状态，永久性策略错误和无效提示错误会直接报告，不再进行不必要的生成重试。
- 流式 UTF-8 可以跨网络分块正确解析，并会校验完整的 SSE 事件。仅原生 Responses 路径会匹配 Codex 对异常事件的容错，外部协议校验保持严格。
- 压缩摘要按实际映射的历史前缀和输出预算缓存，避免复用过期缓存或丢失历史。原生服务端工具搜索成功执行后，不再被误报为失败。

升级与兼容性

- 适配 Codex CLI 0.155.0 的模型路由和运行时兼容性检查。推荐使用 0.155.0，最低版本要求仍为 0.149.x。

### English

Workflow changes

- GPT-5.5 reappears in subscription lists and context editing when the upstream catalog exposes it. Models you chose to hide stay hidden.
- Native connections and model catalogs are isolated by the selected user or workspace owner.
- For external models, configured JSON Schema output is translated for Chat Completions and Anthropic, and supported Anthropic reasoning effort is preserved. The official OpenAI Responses endpoint keeps store, include, and cache intent.
- Reduce time to first token in low-concurrency WebSocket calls.

Fixes

- If the peer closes a native WebSocket request with code 1009 before any response event, EMP recovers it internally over HTTP/zstd. Later full requests on that local connection and route stay on HTTP, and nothing is replayed after acceptance or output.
- Known native model aliases stay consistent with the requested catalog slug. This prevents false account-risk warnings while real model changes are still reported.
- Native failure codes and pre-output HTTP status are preserved, so permanent policy errors and invalid-prompt errors are reported without unnecessary generation retries.
- Streamed UTF-8 is parsed correctly across network chunks, and complete SSE events are validated. Codex's malformed-event tolerance applies only on native Responses paths; external protocol validation remains strict.
- Compaction summaries are keyed by the actual mapped history prefix and output budget, which prevents stale cache reuse and missing history. Successful native server-side tool search is no longer reported as a failure.

Upgrade and compatibility

- Model routing and runtime compatibility checks are adapted to Codex CLI 0.155.0. Version 0.155.0 is recommended, and the minimum remains 0.149.x.

## 0.11.3 (2026-09-17)

### 中文

使用变化

- 诊断信息中的任务、轮次、会话和父级标识改为仅在单次运行内有效的假名引用。请求关联仍然保留，且不记录提示词、工具参数或凭据。

问题修复

- 大请求容量改为按 16 MiB 步进增长，不再按倍数翻倍，并保留 512 MiB 系统余量。普通的远程压缩请求不再因为 64 MiB 边界而失败。
- 内存暂时不足时返回可重试的 `503 request_capacity_unavailable`，并附带当前内存使用、可用内存和所需容量。真正超出请求大小限制时仍返回 `413`。
- 替换凭据前会先校验账户元数据。凭据、账户和主配置的写入在同一个可回滚事务中完成。重新导入时，会根据替换后的凭据判断是否为重复的 Native 身份。
- 在不提供 `HTTPResponse.read1()` 的 urllib3 版本上，SSE 也能及时流式输出。Windows 上的连接复用、取消行为和有界清理保持不变。

升级与兼容性

- Linux：EMP 会检测旧的系统级安装，但不会修改它，而是引导用户移除旧软件包并安装用户级归档包。现有配置保留在用户目录中。
- Linux：移除了需要特权的 `pkexec` 更新路径。

### English

Workflow changes

- Task, turn, session, and parent identifiers in diagnostics are now run-local pseudonymous references. Request correlation is kept, and prompts, tool arguments, and credentials are not recorded.

Fixes

- Large request capacity now grows in 16 MiB steps instead of doubling, with 512 MiB of system headroom kept free. Ordinary remote-compaction requests no longer fail at the 64 MiB boundary.
- Temporary memory pressure is reported as a retryable `503 request_capacity_unavailable`, along with current memory usage, available memory, and the required capacity. Actual request-size violations still return `413`.
- Account metadata is validated before credentials are replaced. Credential, account, and main-configuration writes happen in one rollback transaction. On reimport, duplicate Native identity is determined from the replacement credential.
- SSE streams promptly on urllib3 versions that lack `HTTPResponse.read1()`. Connection reuse, cancellation behavior, and bounded cleanup on Windows are unchanged.

Upgrade and compatibility

- Linux: EMP detects legacy system-level installations without modifying them and guides you to remove the old package and install the user-level archive. Existing configuration stays in your user profile.
- Linux: the privileged `pkexec` update path has been removed.

## 0.11.2 (2026-09-16)

### 中文

新增功能

- 可以为 Native 和导入订阅中的每个模型单独设置上下文窗口，上限为该模型在订阅目录中的最大值。显示和请求检查使用同一上下文，并保留 Codex 的有效百分比。模型限制会使用所选账户的凭据刷新。
- 导出加密 `.emp` 文件时，可以分别选择 Native、其他订阅和外部提供商，导出内容只包含所选路由、显示设置和托管凭据。迁移格式不变，默认仍为全量导出。
- 导出 Native 时会包含本机的 Codex 登录凭据。导入后它会作为额外订阅加入，不会替换目标机器上的登录。
- 外部提供商支持 Codex 客户端执行的工具搜索，包括发现的工具定义、工具执行和后续历史重建。冲突的定义和不支持的服务端搜索会被拒绝。同名的命名空间工具不再被合并。

使用变化

- 订阅模型列表和编辑选项中隐藏 GPT-5.5。外部提供商和现有任务路由不受影响。
- 新的 ChatGPT 用量按所选账户的不透明身份分组，不再按本地路由 ID 分组，Native 和订阅分类保持不变。旧用量的账户归属保持“未确认”。外部用量仍按提供商来源分组。
- 每次打开“Check updates”时都会重新检查新版本，正在进行的更新不受影响。对话框中重复的检查按钮已移除。
- 迁移时，冲突的账户会以唯一的目标 ID 和前缀保留。导出时会报告实际导出数量，缺少 Native 登录凭据时会给出警告。
- 超大的活跃轮次中，已完成的工具批次会被压缩，同时保留指令、最近结果和待处理的工具调用。

问题修复

- HTTP 代理（包括 CONNECT 隧道）改用仅发给代理的认证头，支持解码 URL 编码的凭据，源站凭据和连接池保持隔离。
- 在所有操作系统上都以 UTF-8 读取 Codex 额度子进程的 JSON-RPC 输出。输出无效或管道读取失败时会明确报告，不再导致读取线程崩溃。
- 在 Windows 上，连接池中各连接的 TLS 验证上下文相互隔离，避免并发请求之间串用握手状态。
- 外部流的失败原因在 HTTP 错误转换中得到保留，并区分“流未完成”和“上游 HTTP 服务器错误”。

升级与兼容性

- Linux 归档包会安装到当前用户的数据目录，并附带命令启动器和桌面入口。用户配置保持不变，此类安装更新时无需管理员授权。
- Linux 系统目录安装通过桌面的管理员授权对话框更新，deb 安装会使用 deb 包。授权被取消或不可用时会给出提示，不会停止现有服务。
- 上传迁移文件时允许 Base64/JSON 编码带来的体积膨胀，但解码后的 `.emp` 文件上限仍为 32 MiB。

### English

New features

- You can set a context window for each model in Native and imported subscriptions, capped at that model's subscription catalog maximum. Display and request checks use the same context, and Codex's effective percentage is preserved. Model limits are refreshed with the selected account's credentials.
- When exporting encrypted `.emp` files, you can choose Native, other subscriptions, and external providers. Only the selected routes, display settings, and managed credentials are included. The migration format is unchanged, and full export remains the default.
- Exporting Native includes this machine's Codex login credentials. On import they are added as an additional subscription and do not replace the destination's login.
- External providers support Codex client-executed tool search, including discovered definitions, tool execution, and subsequent history reconstruction. Conflicting definitions and unsupported server-executed searches are rejected. Namespaced tools with the same name are no longer merged.

Workflow changes

- GPT-5.5 is hidden from subscription model lists and editing options. External providers and existing task routes are unaffected.
- New ChatGPT usage is grouped by the selected account's opaque identity instead of local route IDs, and the Native and Subscription categories are kept. Attribution of older usage stays unconfirmed. External usage is still grouped by provider source.
- Opening Check updates now always checks for a fresh release, and an update already in progress is not affected. The duplicate check button in the dialog has been removed.
- During migration, conflicting accounts are kept under unique destination IDs and prefixes. Exports report the actual counts and warn when Native login credentials are missing.
- In oversized active turns, completed tool batches are compacted while instructions, recent results, and pending tool calls are kept.

Fixes

- HTTP proxies, including CONNECT tunnels, now authenticate with proxy-only headers. URL-encoded credentials are decoded, and origin credentials and connection pools stay isolated.
- Codex quota subprocess JSON-RPC output is read as UTF-8 on every OS. Invalid output and pipe read failures are reported explicitly instead of crashing the reader threads.
- On Windows, TLS verification contexts are isolated between pooled connections, so handshake state no longer leaks across concurrent requests.
- External stream failure reasons are preserved through HTTP error translation, and an unfinished stream is distinguished from an upstream HTTP server error.

Upgrade and compatibility

- Linux archives install into the current user's data directory with a command launcher and desktop entry. User configuration is kept, and updates of these installations need no administrator authorization.
- Linux system-directory installations update through the desktop's administrator authorization dialog, and deb installations use deb packages. If authorization is cancelled or unavailable, you are notified and the existing service keeps running.
- Uploading a migration allows for Base64/JSON size expansion, but the decoded `.emp` file limit stays at 32 MiB.

## 0.11.1 (2026-09-13)

### 中文

使用变化

- “Enable EMP”和“Restore Native”合并为同一位置的单个按钮，按钮的标签和操作随已确认的集成状态切换。
- 应用设置期间按钮会被禁用，失败后可以重试。遇到冲突时，原有的确认对话框和恢复操作保持不变。
- 移除“Check again”按钮，状态仍会自动验证。

### English

Workflow changes

- Enable EMP and Restore Native are combined into one button in the same position. Its label and action follow the confirmed integration state.
- The button is disabled while settings are being applied, and you can retry after a failure. The existing confirmation dialog and recovery action for conflicts are unchanged.
- The Check again button is removed; status is still verified automatically.

## 0.11.0 (2026-09-13)

### 中文

新增功能

- 新增常驻的“Usage & estimates”视图，支持按本地时间选择范围，提供按小时和按天的柱状图，并分别统计 Native、其他订阅和外部提供商的总量。
- 基于每日刷新的公开价格目录，估算与 API 等价的 USD token 费用。历史费率会被保留，有价格可用时会补全缺失的价格，并区分“未定价”请求与免费用量。
- 在后台扫描本地 Codex 会话和归档历史，之后只读取有变化的文件。无需重启 Codex 即可浏览全部可用历史。

使用变化

- 重复的用量更新、复制或归档的 rollout，以及 fork 回放都会去重。历史记录会按轮次、模型和上报的数量与 EMP 的实时观测进行匹配，重叠记录无法可靠匹配时会给出警告。
- 历史账户身份与当前登录分开处理。来源未知和旧版本中不一致的用量会如实显示，不会猜测对应的账户或价格。

升级与兼容性

- 现有配置和额度历史保持不变。
- 用量数据只保存在本地。历史费用基于扫描时可用的价格计算，并非实际账单。

### English

New features

- Added a persistent Usage & estimates view with local-time ranges, hourly and daily bars, and separate totals for Native, other Subscription, and External Provider usage.
- API-equivalent USD token costs are estimated from a public price catalog refreshed daily. Historical rates are preserved, missing prices are filled in when available, and unpriced requests are distinguished from free usage.
- Local Codex session and archive history is scanned in the background, and afterwards only changed files are read. You can browse all available history without restarting Codex.

Workflow changes

- Repeated usage updates, copied or archived rollouts, and fork replay are deduplicated. History is matched with live EMP observations by turn, model, and reported counts, with a warning when overlapping records cannot be matched reliably.
- Historical account identities are kept separate from current logins. Unknown sources and inconsistent legacy usage are shown as-is instead of guessing the account or price.

Upgrade and compatibility

- Existing configuration and quota history are unchanged.
- Usage data stays local. Historical costs use the prices available at scan time and are not actual invoices.

## 0.10.2 (2026-09-12)

### 中文

使用变化

- 通知移到右下角并增加关闭按钮：成功通知约 4 秒后自动消失，错误通知需手动关闭，不再遮挡顶部“退出 EMP”。
- “检查更新”改用独立的升级图标，与导入、导出和刷新余量区分。

问题修复

- 原生 Codex 连接遇到临时 TLS 或网关错误时，EMP 不再过早切换到 HTTP 并暂停 WebSocket 路由，临时故障交由 Codex 重试；不支持 WebSocket 的上游仍保留 HTTP 兼容。
- 健康统计不再把同一次请求在切换传输方式前的连接失败重复计入请求总数和 502 比例；详细日志仍保留这些尝试。

升级与兼容性

- 在 EMP 网页点击“检查更新”并选择“后台更新”，或下载对应平台文件：Windows 为 EMP.exe；Linux 为 EMP-linux-x86_64.deb 或 EMP-linux-x86_64.tar.gz；macOS Intel 为 EMP-macos-x86_64.dmg；macOS Apple Silicon 为 EMP-macos-arm64.dmg。

### English

Workflow changes

- Notifications now appear in the bottom-right corner with a dismiss button. Success messages close after about 4 seconds, errors stay until dismissed, and the top "Exit EMP" button is no longer covered.
- "Check for updates" uses a dedicated upgrade icon, distinct from import, export and quota refresh.

Fixes

- Transient TLS or gateway errors on native Codex connections no longer make EMP switch to HTTP too early and pause WebSocket routing; Codex retries these failures itself. HTTP fallback is kept for upstreams that do not support WebSocket.
- Health statistics no longer count connection failures that occur before a transport fallback as extra requests or toward the 502 ratio; detailed logs still keep those attempts.

Upgrade and compatibility

- Click "Check for updates" in the EMP web UI and choose "Background update", or download the file for your platform: EMP.exe for Windows; EMP-linux-x86_64.deb or EMP-linux-x86_64.tar.gz for Linux; EMP-macos-x86_64.dmg for macOS Intel; EMP-macos-arm64.dmg for macOS Apple Silicon.

## 0.10.1 (2026-09-12)

### 中文

新增功能

- 在“性能与健康”中按模型、账号／Provider 和 Fast／普通模式查看缓存命中率：每 10 分钟按“缓存输入 token 总数 ÷ 输入 token 总数”计算，并显示有缓存命中的调用次数。
- 缓存统计支持 Responses、Chat Completions（包括 DeepSeek）和 Anthropic Messages，以及 NA2H／Gemini 返回的兼容用量字段；流式与非流式请求都保留上游报告的缓存数字。
- 近期失败记录可展开查看所用服务、连接状态、是否已输出内容和诊断 ID，不展示对话正文。

使用变化

- 不同账号的余量可独立刷新，同一账号的重叠查询共享结果，减少重复请求和凭据刷新；后台刷新完成后自动同步到网页，不打断正在查看的趋势。
- 导入、导出移到顶部，与更新、GitHub、语言和主题等控件按用途分组，小窗口下自动排列。

升级与兼容性

- 无调用的时段不生成记录；上游未提供缓存用量时显示“未提供”，不当作 0%。新记录在重启后保留，旧日志缺少缓存字段的部分无法补算。
- 统计范围为最近 7 天、最多 512 次已保留请求，不是完整账单；命中率反映上游报告结果，判断 EMP 是否影响缓存需做同账号、同模型、同上下文的原生对照。
- 在 EMP Web UI 点击“检查更新”，或从 Assets 下载对应平台安装包。

### English

New features

- View prompt cache hit rates in "Performance and health" by model, account/provider and Fast/standard mode, calculated every 10 minutes as total cached input tokens ÷ total input tokens, along with the number of calls that hit the cache.
- Cache statistics cover Responses, Chat Completions (including DeepSeek) and Anthropic Messages, plus compatible usage fields returned by NA2H/Gemini; upstream-reported cache counts are kept for both streaming and non-streaming requests.
- Recent failure records can be expanded to show the service used, connection state, whether output had started and the diagnostic ID, without displaying conversation content.

Workflow changes

- Account quotas refresh independently, and overlapping reads for the same account share results, reducing duplicate requests and credential refreshes. Background refresh results sync to the web page automatically without interrupting the open trend view.
- Import and export moved to the top toolbar, grouped by purpose with update, GitHub, language and theme controls, and arranged automatically in narrow windows.

Upgrade and compatibility

- Idle periods produce no records; when the upstream provides no cache usage, "Not provided" is shown instead of 0%. New records persist across restarts; older logs without cache fields cannot be backfilled.
- Statistics cover the last 7 days and up to 512 retained requests, not full billing. Hit rates reflect upstream reports; judging whether EMP affects caching requires a native comparison with the same account, model and context.
- Click "Check for updates" in the EMP Web UI, or download the installer for your platform from Assets.

## 0.9.99 (2026-09-12)

### 中文

新增功能

- 趋势图按所选 5h／7d 额度的 reset 记录分段，一周视图按本地日期分天；点击某一段可放大查看，点击“返回”回到上一层。

使用变化

- 切换 1 小时、1 天、1 周、15 天及 5h／7d 时直接使用已加载的历史，不再先清空图表；后台刷新和额度窗口切换会保留正在查看的区间，刷新失败也不清掉已有曲线。
- 手动提前重置按首次记录到变化的采样时间标记。

问题修复

- 修复悬停辅助竖线一直隐藏的问题，现在跟随最近的采样点显示时间和具体余量。

升级与兼容性

- 在 EMP Web UI 点击“检查更新”，或从 Assets 下载对应平台安装包。

### English

New features

- Trends are divided by the recorded reset periods of the selected 5h/7d quota, with local calendar-day divisions in the weekly view. Click a period to zoom in, then click "Back" to return.

Workflow changes

- Switching between 1 hour, 1 day, 1 week, 15 days and 5h/7d uses the already loaded history instead of clearing the chart first. Background refreshes and quota window switches keep the inspected range, and failed refreshes no longer erase existing curves.
- The chart marks an early manual reset at the first sample that records the change.

Fixes

- Fixed the hover guide line staying hidden; it now follows the nearest sample and shows the time and remaining quota.

Upgrade and compatibility

- Click "Check for updates" in the EMP Web UI, or download the installer for your platform from Assets.

## 0.9.98 (2026-09-11)

### 中文

使用变化

- Subscription 编辑在设置保存成功后立即结束等待，模型目录和 Codex 状态在后台同步；同步失败单独提示，不会把已保存的修改报成保存失败。
- 工具命名冲突有独立错误码，诊断记录工具命名空间数量和增量请求标记，不记录对话正文或凭据。

问题修复

- WebSocket 连接失败转走 HTTP 时不再重复转换工具定义，避免出现 `reserved EMP collaboration namespace collision` 导致的 422；后续请求也能正常走回退流程。
- 本次修复的是 EMP 回退流程引发的 422，代理或上游自身的 TLS 故障仍需分别排查。

升级与兼容性

- 在 EMP Web UI 点击“检查更新”，或从 Assets 下载 Windows、Linux、Mac Intel 或 Apple Silicon 安装包。

### English

Workflow changes

- Subscription edits finish as soon as settings are saved, while the model catalog and Codex status sync in the background. Sync failures are reported separately and no longer make a saved change look like a failed save.
- Tool naming collisions have a dedicated error code, and diagnostics record tool namespace counts and incremental-request markers without storing conversation content or credentials.

Fixes

- When a WebSocket connection falls back to HTTP, tool definitions are no longer converted twice, avoiding 422 errors with `reserved EMP collaboration namespace collision`; subsequent requests also fall back correctly.
- This fixes 422 errors caused by EMP's fallback flow; TLS failures from proxies or upstreams still need to be investigated separately.

Upgrade and compatibility

- Click "Check for updates" in the EMP Web UI, or download the Windows, Linux, Mac Intel or Apple Silicon installer from Assets.

## 0.9.97 (2026-09-11)

### 中文

使用变化

- 修改模型名称、隐藏状态或添加模型后，EMP 会通知 Codex 拉取新列表；检查时同时核对名称和描述，避免模型 ID 未变时误报“已更新”。
- 上游限流或繁忙时保留错误类别和建议等待时间，Chat、Anthropic 流式转换不再丢失这些信息；未增加自动重试或延长超时。

问题修复

- 修复部分 Windows Python 环境下的后台连接，并补充扫描 Linux 已知插件安装位置。
- 重启后按调用记录自身的时间恢复性能历史，避免日志文件时间相同或被复制后近期记录乱序。

升级与兼容性

- 推荐 Codex runtime 更新到 0.154.0，支持范围为 0.149.x–0.154.x。
- 模型更新仍受 Codex 刷新时机影响，不保证空闲菜单保存后立即重绘；修改 Base URL 或从旧静态模型目录切换，仍需安全重启客户端。
- [Codex 0.154 兼容详情](https://github.com/Killow1998/EasyMultiProvider/blob/v0.9.97/docs/codex-0.154-review.md)。

### English

Workflow changes

- After renaming, hiding or adding models, EMP notifies Codex to fetch the new list, and checks both names and descriptions so an unchanged model ID is not misreported as "updated".
- Upstream rate-limit or busy errors keep their category and suggested wait time, which Chat and Anthropic streaming conversion no longer drop; no automatic retries or longer timeouts were added.

Fixes

- Fixed the background connection in some Windows Python environments and added scanning of known Linux plugin install locations.
- After a restart, performance history is restored by each record's own time, so recent records no longer appear out of order when log file times match or files are copied.

Upgrade and compatibility

- Codex runtime 0.154.0 is recommended; the supported range is 0.149.x–0.154.x.
- Model updates still depend on Codex's refresh timing, and an idle menu may not redraw immediately after saving. Changing the Base URL or switching from the old static model catalog still requires a safe client restart.
- [Codex 0.154 compatibility details](https://github.com/Killow1998/EasyMultiProvider/blob/v0.9.97/docs/codex-0.154-review.md).

## 0.9.96 (2026-09-09)

### 中文

使用变化

- 网页登录保留 30 天，重启 EMP 后可直接打开普通管理地址。
- 未登录或登录过期时显示中文提示页。

问题修复

- 修复 bootstrap 参数和 Cookie 含非 ASCII 字符时导致请求崩溃的问题。

### English

Workflow changes

- Web UI login is remembered for 30 days, so the regular admin address opens directly after restarting EMP.
- A readable sign-in page is shown when you are not logged in or the session has expired.

Fixes

- Fixed request crashes when bootstrap parameters or cookies contain non-ASCII characters.

## 0.9.95 (2026-09-09)

### 中文

新增功能

- 网页新增“退出 EMP”：先恢复原生配置再停止服务，恢复失败时保留服务。

问题修复

- 修正 Windows 更新和回滚后的终端启动方式，改为可见控制台启动。
- Windows 控制台关闭和正常关机通知时会恢复 EMP 接管的配置。

升级与兼容性

- 断电或强制终止时无法保证执行恢复。
- 从旧版更新到本版时，旧更新器仍可能隐藏启动，可使用新增的网页“退出 EMP”；后续更新使用修正后的可见启动。

### English

New features

- Added "Exit EMP" to the web UI: it restores native configuration before stopping the service, and keeps the service running if restoration fails.

Fixes

- Windows now relaunches EMP in a visible console after updates and rollback.
- EMP-managed integration settings are restored on Windows console close and normal shutdown notifications.

Upgrade and compatibility

- Restoration cannot be guaranteed after power loss or forced termination.
- When updating from an older version to this one, the old updater may still start EMP hidden; use the new web "Exit EMP" action. Later updates use the corrected visible launch.

## 0.9.94 (2026-09-09)

### 中文

问题修复

- 修复多个正常 SSE 事件合并到达时被误判为单个事件超限并返回 502 的问题。
- 诊断和客户端错误中补充具体原因，区分无效 JSON、事件超限和终止状态异常，不包含内容。

### English

Fixes

- Fixed false 502 errors when multiple valid SSE events arrive together and were treated as a single oversized event.
- Diagnostics and client errors now give specific, content-free reasons, distinguishing invalid JSON, oversized events and unexpected terminal states.

## 0.9.93 (2026-09-08)

### 中文

新增功能

- 调用日志新增请求编号，以及客户端提供的任务／会话 ID，可追查隐藏模型的调用来源；不记录聊天内容或凭据。
- 传输失败时记录 TLS 错误原因和 WebSocket 接收状态，并用不透明请求 ID 与上游网关（NA2H）的请求关联。

使用变化

- 每次建立新上游连接和查询额度时都会读取当前系统代理；代理地址变化后不再复用旧的 WebSocket 连接。
- 即使在 EMP 运行期间启用代理，本机 Provider 仍保持直连，减少切换代理后本地请求失败。

升级与兼容性

- 在 EMP 网页顶部点击“检查更新”即可升级。
- 隐藏仍只表示不出现在模型列表中，不会阻止已有任务按模型 ID 调用。

### English

New features

- Call logs now include a request ID and client-provided task/session IDs, so you can trace where hidden-model calls come from. Chat content and credentials are not recorded.
- Transport failures now record structured TLS causes and WebSocket receive state. Upstream requests are linked to the upstream gateway (NA2H) through an opaque request ID.

Workflow changes

- System proxy settings are read for each new upstream connection and quota query. Native WebSocket connections are not reused after the proxy endpoint changes.
- Loopback providers stay direct even if a proxy is turned on while EMP is running, which reduces local request failures after switching proxies.

Upgrade and compatibility

- To update, click “检查更新” (Check for updates) at the top of the EMP web page.
- Hiding a model still only removes it from the model list. Existing tasks can still call it by model ID.

## 0.9.92 (2026-09-08)

### 中文

新增功能

- 使用 ChatGPT 登录并启用动态模型目录后，修改模型名称、上下文前缀、隐藏状态或新增模型，都无需重启 Codex。
- 实测 Codex runtime 0.153.4 约每 4.5 分钟自动刷新一次目录；刷新后重新打开模型选择器即可看到变化。

使用变化

- 额度重置时间改为刷新图标加简短倒计时（如 2h30m），每分钟更新。

问题修复

- 原生模型目录与合并后的模型缓存分开保存，隐藏的模型可以恢复显示，改名后也能恢复默认名称。

升级与兼容性

- 从静态目录的旧版升级后，需重启 Codex 一次；修改 Base URL 仍需重启。
- 仅使用 API Key 登录时继续使用静态目录。
- 如需回退到 0.9.91，请先在新版中恢复原生配置。

### English

New features

- With a ChatGPT login and the dynamic catalog enabled, model name, context prefix, and visibility changes take effect without restarting Codex. Newly added models also appear without a restart.
- In testing, Codex runtime 0.153.4 refreshed the catalog automatically about every 4.5 minutes. Reopen the model picker after a refresh to see the changes.

Workflow changes

- Quota reset times now appear as a refresh icon with a compact countdown (e.g. 2h30m), updated every minute.

Fixes

- The original native catalog is now stored separately from the merged model cache. Hidden models can be shown again, and renamed models can be restored to their default names.

Upgrade and compatibility

- After upgrading from a version that used the static catalog, restart Codex once. Changing the Base URL still requires a restart.
- API Key–only logins continue to use the static catalog.
- To roll back to 0.9.91, restore the native configuration in the new version first.

## 0.9.91 (2026-09-07)

### 中文

问题修复

- 已是最新版时，更新弹窗不再显示“后台更新”按钮。
- 检查更新和安装期间，应隐藏的按钮保持隐藏。

升级与兼容性

- 包含 v0.9.9 的全部修复。
- Windows 使用 EMP.exe；Linux 使用 .deb 或 .tar.gz；Mac 按 Intel / Apple Silicon 选择对应 DMG。

### English

Fixes

- When EMP is already up to date, the update dialog no longer shows the background-install button.
- Buttons that should be hidden stay hidden while checking for or installing updates.

Upgrade and compatibility

- Includes all fixes from v0.9.9.
- Windows: use EMP.exe. Linux: use .deb or .tar.gz. Mac: pick the DMG that matches your Intel or Apple Silicon machine.

## 0.9.9 (2026-09-07)

### 中文

问题修复

- 修复外部模型子任务在 Codex Responses Lite 委派中的加密任务 422 错误，后续任务和工具调用均可正常进行；已在 Codex runtime 0.153.4 上实测 Gemini 3.7 Flash 和 3.8 Flash high。
- 无法转发的加密任务会明确说明原因。WebSocket 连续请求中的明文委派标记也不再丢失。
- 回答中出现 `<think>`、`<tool_call>` 等普通文字时，不再误报 502。
- 模型的拒绝回答内容会保留，不再变成空回答；Chat token 用量完整转换，包括缓存 token、推理 token 和流末尾报告的用量。
- 模型能力未知时，不再静默删除用户选择的推理等级。
- Codex Spark 额度正确命名，趋势图可分别查看 Codex/Spark 与 5h/7d，密集采样点不再遮住曲线。

升级与兼容性

- runtime 推荐版本仍为 0.153.4，支持范围保持 0.149.x–0.153.x。
- Windows 使用 `EMP.exe`；Linux 使用 `.deb` 或 `.tar.gz`；Mac 按 Intel / Apple Silicon 选择对应 DMG。

### English

Fixes

- External subagent delegation in Codex Responses Lite no longer fails on encrypted tasks with a 422 error, and follow-up tasks and tool calls work. Verified with Gemini 3.7 Flash and 3.8 Flash high on Codex runtime 0.153.4.
- Encrypted tasks that cannot be forwarded now report the reason explicitly. Plaintext collaboration markers are no longer lost on incremental WebSocket requests.
- Ordinary text such as `<think>` or `<tool_call>` in an answer no longer triggers a 502 error.
- Model refusals are now kept instead of showing up as empty answers. Chat token usage is fully translated, including cached tokens, reasoning tokens, and usage reported at the end of a stream.
- A selected reasoning effort is no longer silently dropped when model capabilities are unknown.
- Codex Spark quotas are now labeled correctly. The trend view shows Codex/Spark and 5h/7d histories separately, and dense sampling points no longer hide the curves.

Upgrade and compatibility

- The recommended runtime is still 0.153.4, and the supported range remains 0.149.x–0.153.x.
- Windows: use `EMP.exe`. Linux: use `.deb` or `.tar.gz`. Mac: pick the DMG that matches your Intel or Apple Silicon machine.

## 0.9.8 (2026-09-05)

### 中文

新增功能

- 支持 Astra，兼容 Codex 0.149.x–0.153.x，推荐 0.153.4。
- 网页顶部新增更新按钮和 GitHub 入口，可在后台下载并安装正式版。EMP 会等当前请求结束再重启，新版本启动失败时自动恢复旧版。
- 性能历史在重启后保留，按模型显示近期 TTFT（开始输出前的等待时间）和 TPS（每秒输出 token 数），并与前一段记录对比；能识别模式时分别统计 Fast 和普通模式。
- 新增连接日志，帮助判断故障出在 EMP、网络还是上游服务；不记录聊天内容或凭据。

问题修复

- 修复 Windows、macOS 上找不到 ChatGPT，以及更新应用后仍显示旧 Codex runtime 版本的问题。
- 修复 Side chat 在历史索引缺失或继承主对话历史时的历史重建失败（#8）；修复外部模型与原生模型切换后，工具调用 ID 错误导致对话无法继续的问题（#9）。
- 原生模型的上下文、推理设置和指定的 Fast/普通模式得以保留，且原生路由设置不再被套用到其他模型。
- TTFT/TPS 统计修正：空事件不再算作开始输出，推理 token 不再抬高输出速度，短突发、未完成或无法可靠测量的回答不再显示失真速度。
- 5h、7d 额度曲线不再混淆，各曲线有明确标注，悬停可查看重置时间，额度重置或采样中断处不再连线。切换账号或时间范围后，旧请求不再覆盖当前趋势，服务返回的各组额度数据也会全部保留。
- 修复其他页面区域报错时 Codex 状态卡在“正在读取状态”的问题，以及 Windows 后台更新遇到新程序无法启动时卡在系统错误弹窗的问题。

升级与兼容性

- Windows x64 使用 `EMP.exe`；Linux x64 使用 `.tar.gz` 或 `.deb`；Intel Mac 使用 `EMP-macos-x86_64.dmg`；Apple Silicon Mac 使用 `EMP-macos-arm64.dmg`。
- 手动安装前先退出 EMP；安装后启用 EMP，并重启正在使用的 Codex/ChatGPT 客户端。

### English

New features

- Adds Astra support. Compatible with Codex 0.149.x–0.153.x, with 0.153.4 recommended.
- New update button and GitHub shortcut at the top of the web page download and install stable releases in the background. EMP waits for active requests to finish before restarting and restores the previous version if the new one fails to start.
- Performance history now survives restarts and shows recent per-model TTFT (time until output starts) and TPS (output tokens per second), compared with the preceding window. Fast and Standard modes are tracked separately when the mode is known.
- New connection diagnostics help pinpoint whether a failure is in EMP, the network, or the upstream service, without recording chat content or credentials.

Fixes

- ChatGPT is detected reliably on Windows and macOS, and the correct Codex runtime version shows after updating the app and rescanning.
- Side chat history now rebuilds when the local history index is missing or the chat inherits history from its parent conversation (#8). Conversations can continue after switching between external and native models without tool-call ID errors (#9).
- Native model context, reasoning settings, and the requested Fast/Standard mode are preserved, and native routing settings are no longer applied to other providers.
- TTFT/TPS figures are now accurate. Empty events don't count as first output, reasoning tokens don't inflate output speed, and short bursts, incomplete responses, or unreliable measurements show no speed value.
- The 5h and 7d quota curves are kept separate and labeled, and hovering shows reset times. Lines break at quota resets or sampling gaps. Old trend responses no longer overwrite the selected account or time range, and all quota groups returned by the service are retained.
- An error in another part of the page no longer leaves Codex stuck on “Reading status”. Windows background updates no longer hang on a system error dialog when the new executable cannot launch.

Upgrade and compatibility

- Windows x64: `EMP.exe`. Linux x64: `.tar.gz` or `.deb`. Intel Mac: `EMP-macos-x86_64.dmg`. Apple Silicon Mac: `EMP-macos-arm64.dmg`.
- Quit EMP before installing manually. After installing, enable EMP and restart any Codex/ChatGPT client you're using.

## 0.9.7 (2026-09-02)

### 中文

问题修复

- 打包版使用操作系统的 TLS 信任库，包括 macOS 钥匙串。构建时验证该后端能启用，避免本机 EMP 看起来正常、所有上游请求却返回 TLS 502。
- 原生 Responses WebSocket 保留明确的 429、认证和上游 5xx 错误。请求派发后由 Codex 管理重试，EMP 不重复发送可能已到达上游的请求。

连接与兼容性

- 原生 WebSocket 支持压缩和系统代理，连接超时为 15 秒，单条消息空闲超时为 300 秒。增量请求可沿用 `previous_response_id`。
- 移除 EMP 的原生 WebSocket 出站请求 64 MiB 限制以及累计响应 16 MiB 和事件数量限制。Codex 的入站 64 MiB 消息边界仍适用。
- 移除每个订阅账号最多四路生成和 45 秒排队限制。请求通过本机资源检查后即可发往上游，由服务商决定并发额度。
- 验证了 0.9.0 至 0.9.7 的 `.emp` 互导，并覆盖缺少新模型显示字段的旧包。

### English

Fixes

- Packaged builds use the operating system’s TLS trust store, including macOS Keychain. Packaging checks that this backend can load, catching builds that would serve the local UI but return TLS 502 for upstream requests.
- Native Responses WebSocket preserves explicit 429, authentication, and upstream 5xx errors. Codex owns post-dispatch retries, so EMP does not resend a request that may already have reached the upstream.

Connections and compatibility

- Native WebSocket supports compression and the system proxy, with a 15-second connect timeout and a 300-second per-message idle timeout. Incremental turns retain `previous_response_id` continuity.
- Removes EMP’s 64 MiB outbound native WebSocket request cap, 16 MiB cumulative response cap, and fixed event-count cap. Codex’s 64 MiB inbound message limit still applies.
- Removes EMP’s four-generation subscription gate and 45-second queue. Once admitted locally, requests dispatch immediately and the provider controls account concurrency.
- Verified `.emp` imports across 0.9.0 through 0.9.7, including older bundles without newer model display fields.

## 0.9.6 (2026-09-02)

### 中文

新增与改进

- Windows、Linux 和 macOS 的程序统一叫 EMP，下载文件不带版本号，WebUI 标题旁显示正在运行的版本。Codex 兼容范围扩到 0.152.x，并推荐该系列。
- 额度显示随后台采样更新。电量表只在数值变化时动画，趋势图悬停可查看最近数据点。
- 按模型记录跨重启的近期 TTFT/TPS 中位数，可区分 Fast 和普通模式。输出速度排除推理 token，过短或不可靠的测量不显示。
- 健康视图显示近期请求结果，区分本地排队、代理、DNS、TLS、连接超时和上游错误。模型显示留在桌面侧栏，上下文预览即时更新，设置控件更紧凑。

使用变化

- 独立搜索自动选择账号，优先当前 Codex 登录，再尝试可用的导入订阅。旧的固定账号设置会迁移。
- 本机在可用内存允许时将 Responses、压缩和 WebSocket 单请求容量从 64 MiB 扩到最高 1 GiB。请求超限时明确报错，历史不会截断。
- 本机监听按需求从 64 路增到 256 路，为短请求单独预留容量。此版本每个订阅从 4 路生成起步，有持续排队且完成成功时逐步增到 16 路，收到 429 后减半。
- 原生流开始输出前最多等待 120 秒，开始后空闲限时为 300 秒。复用连接前主动探测；HTTP 回退仅用于派发前握手失败。大帧的字节处理也改为批量完成。

问题修复

- 外部模型发现的 400、401 和 403 提供 API Key 或权限提示。Windows 包使用实际加载的 OpenSSL 库，额度辅助进程保留 `SYSTEMROOT`。
- 只有 Native 模型时也可应用模型名称和可见性。配置保存和目录验证各自显示结果，搜索保存和重新扫描保留未保存的客户端勾选状态。

### English

Improvements

- The app is named EMP on Windows, Linux, and macOS. Download names are versionless, and the WebUI shows the running version beside the title. Codex compatibility now covers, and recommends, 0.152.x.
- Quota displays follow background samples. Battery meters animate on changes, and the trend chart shows the closest point on hover.
- Recent median TTFT and TPS are tracked per model across restarts, with Fast and Standard modes separated. Output speed excludes reasoning tokens and omits short or unreliable measurements.
- The health view summarizes recent outcomes and distinguishes local queue limits, proxy, DNS, TLS, timeouts, and upstream failures. Model display stays in a desktop side panel with immediate context previews and more compact settings.

Workflow changes

- Standalone search chooses an account automatically, preferring the current Codex login and then an available imported subscription. Older pinned-account settings migrate.
- With available memory, EMP can raise the per-request allowance for Responses, compaction, and WebSocket from 64 MiB up to 1 GiB. Oversized requests receive an explicit error, preserving history.
- The local listener scales from 64 to 256 connections, reserving capacity for short requests. In this version, each subscription starts at four simultaneous generations, grows to 16 after successful calls under queue pressure, and halves its limit after a 429.
- Native streams wait up to 120 seconds for substantive output and allow 300 seconds of idle time after it starts. Idle connections are checked before reuse. HTTP fallback is limited to pre-dispatch handshake failures, and large frames use bulk decoding.

Fixes

- Provider discovery turns 400, 401, and 403 responses into API-key or permission guidance. Windows packages use the OpenSSL libraries actually loaded at build time, and quota helpers keep `SYSTEMROOT`.
- Catalogs containing only Native models can apply display names and visibility. Configuration and runtime verification show separate outcomes. Saving search settings or rescanning preserves unsaved client selections.

## 0.9.5 (2026-08-31)

### 中文

新增与改进

- WebUI 可查看并选择多个 Codex 客户端。扫描涵盖桌面 App、Codex 托管运行时、VS Code 和 Cursor 扩展，以及 `PATH` 中的独立 CLI，并合并指向同一程序的条目。
- 选中的客户端可同时使用 EMP。版本检查和额度查询单独选择兼容的辅助程序。

问题修复

- 仅采纳与主机系统和架构匹配的编辑器运行时，避免 Windows 扫描到的 WSL 程序盖过本机安装。单个版本探测失败时，其余客户端照常显示。
- 逐个显示兼容性和探测结果，区分扫描失败、扫描不可用和确实没有找到客户端。

### English

Improvements

- The WebUI discovers and lets you select multiple Codex clients. It scans the desktop app, the managed runtime, VS Code and Cursor extensions, and the standalone CLI on `PATH`, deduplicating identical executables.
- Selected clients can use EMP concurrently. Version checks and quota lookups choose a compatible helper independently.

Fixes

- Editor runtimes must match the host operating system and architecture, so a WSL binary no longer masks a Windows installation. Healthy clients remain visible when another version probe fails.
- Compatibility and probe results appear for each client, distinguishing a failed or unavailable scan from an empty inventory.

## 0.9.4 (2026-08-30)

### 中文

使用变化

- 启用、恢复、刷新目录或检查重载状态时，EMP 保持现有 Codex App Server 进程由 Codex 管理。
- 通过系统控制套接字的 WebSocket 握手和 `model/list` 查询实际模型。WebUI 分开显示文件已保存和运行时已加载；后台暂不可用时等待其恢复。

### English

Workflow changes

- Enable, restore, catalog refresh, and reload checks leave the existing Codex App Server under Codex’s control.
- EMP uses a WebSocket handshake on the local control socket and queries `model/list`. The WebUI distinguishes saved files from models observed in the running server and waits for its owner when the listener is unavailable.

## 0.9.3 (2026-08-30)

### 中文

新增与改进

- 服务列表同时显示当前 Codex 登录与导入订阅。Native 可刷新额度并控制模型可见性；同账号的导入项保留额度刷新。
- 每五分钟采样各个唯一订阅的额度，提供近 1 小时、1 天、1 周和 15 天趋势。图表按实际范围缩放。
- 显示名称、上下文标签和推理摘要策略按模型系列设置，模型保留原有来源前缀。订阅可用独立显示名称，包括 Emoji。浏览器标题简化为 EMP，软件图标改用透明底。

问题修复

- 优先使用 Codex 托管运行时，并单独显示较旧的独立 CLI。额度刷新失败时保留已存凭据。
- 原生 WebSocket 断线后，Codex 可重新发送完整请求，避免在新连接中复用失效的 `previous_response_id`。

### English

Improvements

- The service list shows the current Codex login alongside imported subscriptions. Native supports quota refresh and model visibility controls, and duplicate imported rows can still refresh quota.
- Each unique subscription is sampled every five minutes. Quota charts show the last hour, day, week, or 15 days and scale to the observed range.
- Display names, context labels, and reasoning-summary policy apply per model family, preserving route prefixes. Subscriptions can have a separate display name with emoji. The browser title becomes EMP, and app icons use a transparent background.

Fixes

- The Codex-managed runtime takes precedence, with an older standalone CLI shown separately. Failed quota refreshes preserve the stored credentials.
- After a Native WebSocket disconnect, Codex can resend the full request without reusing a stale `previous_response_id` on the new connection.

## 0.9.2 (2026-08-30)

### 中文

问题修复

- Codex 模型选择器隐藏用户关闭的 Native 模型，内部 Auto Review 等工具仍可使用自己的服务模型。

安装

- GitHub Release 只显示五个常用安装文件；CI 保留全平台构建和校验和。

### English

Fixes

- The Codex picker hides Native models disabled by the user, while internal tools such as Auto Review can still use their service models.

Installation

- GitHub Releases show five standard downloads. CI retains the full build matrix with checksums.

## 0.9.1 (2026-08-30)

### 中文

兼容性

- WebUI 显示 Codex CLI 支持范围 0.149.x–0.151.x 和推荐版本。独立探测区分过新、过旧、不可用和无法识别的安装。
- 额度走官方 Codex App Server，支持 0.151 的限额响应，以及 Windows 与 Codex 配置的证书信任库。
- Codex 重启后自动核验活动目录，运行时恢复时无需再手动应用模型。

### English

Compatibility

- The WebUI shows the supported Codex CLI range, 0.149.x–0.151.x, and the recommended release line. A separate probe distinguishes newer, older, unavailable, and unrecognized installs.
- Quota refresh uses the official Codex App Server and supports Codex 0.151 rate-limit responses, Windows trust roots, and Codex-specific CA settings.
- EMP passively verifies the active catalog after Codex restarts, so a recovered runtime does not require reapplying models.

## 0.9.0 (2026-08-29)

### 中文

安装

- 提供面向 Windows x64、Linux x64、Intel Mac 和 Apple Silicon 的原生安装包。Mac DMG 内含 App，Linux Debian 包带桌面菜单项。
- 安装版直接启动会打开经认证的 WebUI，前台终端可用 Ctrl+C 停止。新增 EMP 图标，CI 在隔离环境运行安装包并校验文件。

使用变化

- HTTP 和 WebSocket 共用路由，Native Responses、外部 Responses、Chat Completions 和 Anthropic 适配各自处理协议。
- 保留 Codex 历史和 Native 压缩，外部切换时按目标模型处理上下文。已实测多个来源切换中的工具、生图输入与压缩续聊。

### English

Installation

- Adds native packages for Windows x64, Linux x64, Intel Mac, and Apple Silicon. Mac disk images contain an app bundle, and the Debian package adds a desktop menu entry.
- Launching the installed app opens the authenticated WebUI and can be stopped with Ctrl+C in its foreground terminal. Adds EMP artwork and isolated package checks with checksums.

Workflow changes

- HTTP and WebSocket share routing, with separate adapters for Native Responses, portable Responses, Chat Completions, and Anthropic.
- Codex continues to own history and Native compaction, with context handled for the destination model on an external switch. Cross-source turns were checked with tools, image input, and repeated compaction.

## 0.8.1 (2026-08-27)

### 中文

兼容性

- 支持 Codex 0.150 的命名独立工具输出。Responses 保留原始结构，Chat Completions 和 Anthropic 接收可见上下文。

问题修复

- 原生目标保留 Codex 压缩过的历史，选择其他上游模型后旧模型的不透明工具数据不会串用。
- 限制流事件和首次输出前的重试缓冲，矛盾的成功结果可清除过时的上下文失败估算。保存 Provider Key 或导入 `.emp` 失败会恢复原凭据。
- 在 Codex 0.150.1 上验证压缩后切换模型、图片和工具，以及回到 Native 继续对话。

### English

Compatibility

- Supports named standalone tool output in Codex 0.150. Responses retains the original item structure, and Chat Completions and Anthropic receive visible context.

Fixes

- Native destinations keep Codex’s compacted history. Changing the upstream model keeps opaque tool data from the previous model from crossing the remap.
- Bounds stream events and pre-output retry buffers. A contradictory success can clear stale context-failure estimates. Failed Provider key saves or `.emp` imports restore prior credentials.
- Verified compacted model switches, images, tools, and a return to Native on Codex 0.150.1.

## 0.8.0 (2026-08-26)

### 中文

使用变化

- Codex 0.149 的远程压缩 v2 可从最近的 `replacement_history` 和后续记录恢复。超出目标模型上下文时，先按目标压缩和转换再发送。
- 外部模型可使用订阅支持的独立网页搜索。EMP 只在外部目标收到无法读取的 Native 压缩历史时重建可见记录。

问题修复

- `previous_response_id` 失效时，返回 Codex 标准重试事件，让客户端重新发送完整回合；同一窗口在压缩后可继续。
- 客户端取消 HTTP 流时关闭上游读取，避免误报内部 500 或再写一次响应。

### English

Workflow changes

- Codex 0.149 remote compaction v2 can recover from the latest `replacement_history` and subsequent turns. Oversize input is compacted for the destination and re-projected before sending.
- External models can use subscription-backed standalone web search. EMP only reconstructs visible history when an external destination receives unreadable Native compacted state.

Fixes

- An unavailable `previous_response_id` triggers Codex’s standard retry event, letting the client resend the full turn. The same window can continue after compaction.
- A client-cancelled HTTP stream closes the upstream reader, avoiding a spurious internal 500 or second response.

## 0.7.6 (2026-08-25)

### 中文

新增与改进

- 为外部目标模型分层压缩历史，严格检查流结束信号，并支持用订阅额度执行独立搜索。
- 派生压缩检查点只保存在内存，诊断事件只写元数据。Codex 自己管理会话与续聊。

### English

Improvements

- Adds hierarchical compaction for the destination model, stricter stream termination, and subscription-backed standalone search for external models.
- Derived checkpoints live in memory and diagnostics record only metadata. Codex owns threads and resume state.

## 0.7.5 (2026-08-24)

### 中文

使用变化

- 通过只读 Codex App Server、SQLite 索引和会话记录恢复历史。遇到不透明的 Native 压缩时，只有切换至外部模型才重建可见部分。
- 简化模型显示和管理控件，由 Codex 继续负责 Native 压缩、重试和历史存储。

### English

Workflow changes

- Reads history through the Codex App Server, SQLite locator, and session rollouts. Across opaque Native compaction, visible history is reconstructed only for an external handoff.
- Simplifies model display and management while Codex retains Native compaction, retries, and history storage.

## 0.6.0 (2026-08-23)

### 中文

新增与改进

- 原生登录、导入订阅和外部服务共用一份模型目录。命令行、TUI 和桌面可显示模型上下文，外部模型带服务前缀。
- 按模型能力转换文本、图片、推理和结构化工具。使用加密的 Native 压缩来源绑定与可移植的外部压缩，在来源切换时继续已压缩任务。
- Native 保留 Codex Zstandard 压缩。等待响应头前可用相同数据重试一次；外部路由仅在明确支持时使用压缩。

问题修复

- 切换失败时，HTTP、SSE 和 WebSocket 返回对应的可恢复错误，WebSocket 可继续处理后续请求。认证和限流错误会终止本次调用。
- 外部流式响应要求明确结束信号，并保留并行工具以及分片调用的类型和索引。WebUI 的账号和模型编辑原子保存，凭据密钥路径拒绝符号链接。
- 通过 813 项测试；隔离验收覆盖了六路同时进行的混合请求及 Native 压缩完整性。

### English

Improvements

- Native login, imported subscriptions, and external providers share one catalog. CLI, TUI, and desktop show context labels and provider-qualified external models.
- Projects text, images, reasoning, and structured tools by model capability. Encrypted Native compaction bindings and portable external compaction preserve compacted task context across sources.
- Native retains Codex Zstandard compression and can retry once before response headers using the same encoded bytes. External routes use compression only when supported.

Fixes

- Failed handoffs return recoverable errors over HTTP, SSE, and WebSocket. The WebSocket remains available for another request. Authentication and rate-limit errors terminate the current call.
- Translated streams require an explicit ending and preserve parallel tools and fragmented call indexes and types. WebUI account and model edits save atomically, and credential key paths reject symlinks.
- Passed 813 tests, with isolated coverage for six concurrent mixed requests and an exact Native compression round trip.

## 0.5.0 (2026-08-22)

### 中文

新增与改进

- 从独立 profile 启动改为接入默认 Codex，保留 `codex`、`codex resume`、`/model` 和桌面会话身份。可离线运行 `doctor` 和 `restore` 检查或恢复接入配置。
- 模型发现支持搜索、已选和总数统计，以及对筛选结果批量全选或清空。已有选择会保留，新模型默认未选。
- 模型显示可带有效上下文后缀；外部 API 发现和配置可识别图像能力，Responses 和 Chat Completions 转发保留图片。

使用变化

- 启用和恢复只需一次确认。可温和停止确认属于当前用户与当前 `CODEX_HOME` 的官方后台，再等待其拥有者重启。运行时验证结果单独显示。
- 自动协议仅在上游明确拒绝协议时回退。请求发送前检查目标上下文，过长请求会明确提示，Codex 继续管理会话和历史。
- 未加前缀的已知 Native 模型直接走当前有效登录。导入同账号时，可管理 Native 可见性且不会出现重复前缀。Auto Review 等内部模型在选择器中隐藏。

验证

- 在 Linux 的 Codex 0.149.0 验证 Native、导入订阅、外部模型、桌面选择器、续聊及图片输入。新增 11 项多模态回归。

### English

Improvements

- Moves from a separate startup profile to the default Codex installation, preserving session identity in `codex`, `codex resume`, `/model`, and the desktop app. The `doctor` and `restore` commands can inspect or restore integration offline.
- Model discovery adds search, selected and total counts, and bulk select or clear for filtered results. Existing selections are preserved and new models start unselected.
- Model display can show usable context size. Discovery and configuration recognize image capability, and Responses and Chat Completions preserve images.

Workflow changes

- Enable and restore each require one confirmation. EMP can gracefully stop a verified official backend owned by the current user and `CODEX_HOME`, then waits for its owner to reopen it. Runtime verification is shown separately.
- Automatic protocol selection falls back only when the upstream explicitly rejects a protocol. Destination context is checked before dispatch, with an explicit over-limit error. Codex retains session and history ownership.
- Known unprefixed Native models use the current valid login. Importing the same account can control Native visibility without a duplicate prefixed entry. Internal models such as Auto Review stay hidden from the picker.

Verification

- Validated Native, imported subscriptions, external models, desktop selection, resume, and image input on Linux with Codex 0.149.0. Added 11 multimodal regressions.

## 0.4.0 (Unreleased)

### 中文

开发变更

- EMP profile 沿用 Codex 的 `openai` 会话身份，经 `openai_base_url` 转发，默认续聊可看见原历史。支持 Native Responses WebSocket、请求解压和远程压缩 v1/v2。
- 每个订阅可设置模型可见性，外部服务可一键显示或隐藏已导入模型。Auto Review 等内部模型留在内部使用。
- 辨认可用但缺少 `Content-Type` 的订阅流。连接与响应分别限时，连接故障可重试一次；自动协议仅在明确拒绝后回退，并记住可用协议。
- 流失败向 Codex 传递实际 HTTP 状态。外部 API Key 请求不携带 Codex 客户端遥测，网关或 WAF 返回 HTML 时显示简洁诊断。

### English

Development changes

- The EMP profile keeps Codex’s native `openai` session identity and uses `openai_base_url`, so default resume can find earlier history. Adds Native Responses WebSocket, request decompression, and remote compaction v1/v2.
- Subscriptions get per-account visibility, and external providers can show or hide all imported models in one action. Internal models such as Auto Review stay internal.
- Accepts valid subscription streams missing `Content-Type`. Connect and response deadlines are separate, with one transient connection retry. Automatic protocol selection only falls back on explicit rejection and remembers the working protocol.
- Stream failures surface the actual upstream HTTP status in Codex. External API-key requests omit Codex client telemetry, and gateway or WAF HTML becomes concise diagnostics.

## 0.3.0 (Unreleased)

### 中文

开发变更

- 增加 ChatGPT 订阅转发服务，支持 Responses 和 Chat 的结构化工具调用与历史。
- 处理非 SSE、空流、上游错误，以及正文里出现的 `<think>` 和 `<tool_call>` 标记。
- 验证真实 Luna、Sol 试调用、按会话续聊和取消恢复，并覆盖 401、404、429、500 和流格式错误。

### English

Development changes

- Adds ChatGPT subscription forwarding, with structured tools and history across Responses and Chat.
- Handles non-SSE responses, empty streams, upstream errors, and leaked textual `<think>` and `<tool_call>` markers.
- Verified live Luna and Sol canaries, explicit-thread resume, and cancellation recovery, with coverage for 401, 404, 429, 500, and malformed streams.

## 0.2.0 (Unreleased)

### 中文

开发变更

- 通过加密 `.emp` 文件迁移配置、模型路由、服务 Key 与 Codex 订阅。导入后用目标设备的本地密钥重新加密，迁移文件不含主密钥。
- 修复 WebUI 状态通知与多账号批量额度刷新，并限制同一账号的重复刷新频率。

### English

Development changes

- Encrypted `.emp` bundles transfer configuration, model routes, provider keys, and Codex subscriptions. The destination re-encrypts credentials with its local key. Bundles never include a master key.
- Fixes WebUI status notifications and multi-account quota refresh, with rate limits on repeated refreshes for one account.

## 0.1.0 (MVP)

### 中文

首次发布

- 本机 WebUI 可管理加密订阅、API 服务、模型发现与路由，以及额度快照。生成一份 EMP 模型目录和独立 Codex profile。
- 支持 Responses、Chat Completions 和 Anthropic Messages，检测代理环境，并可通过 Codex CLI 试用模型。首版在 Linux 验证。

### English

Initial release

- The local WebUI manages encrypted subscriptions, API providers, model discovery and routing, and quota snapshots. Generates an EMP model catalog and a separate Codex profile.
- Supports Responses, Chat Completions, and Anthropic Messages routing, detects the proxy environment, and can test a model through Codex CLI. The initial release was verified on Linux.
