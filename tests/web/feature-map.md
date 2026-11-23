# WebUI 功能地图与浏览器基线

U01–U03 的浏览器测试入口，U04–U06 尚未编写。这里的入口来自 `crates/emp-app/web/` 的正式页面，模组入口还取决于已安装制品及启用状态

## 固定运行入口

在仓库根目录构建一次包含正式页面资产的 debug EMP 二进制；在 `tests/web` 安装测试依赖。Cargo 构建不调用 Node，浏览器测试不调用 Python

```bash
cargo build --locked -p emp-app
export EMP_BIN="$PWD/target/debug/EMP"
cd tests/web
npm ci
npx playwright install --with-deps chromium
npm test
```

`EMP_BIN` 必须指向本次构建的 debug 二进制；使用自定义 target 目录时按实际产物路径设置。本地和 CI 都用 `npm ci` 按 `package-lock.json` 安装

```bash
npm ci
npm test -- management.spec.mjs
npm test -- migration.spec.mjs
npm test -- modal-ownership.spec.mjs
```

完整入口应发现 **8 个测试**，不能把零测试或过滤后的局部结果当作全部基线通过。只运行一个 Chromium headless project，`workers: 1`、不自动重试。用例以 U 编号命名，结果附二进制路径、SHA-256、浏览器和 Node 版本；失败保留 trace、截图和脱敏 EMP 日志

每个 EMP 实例使用独立临时工作目录、HOME、CODEX_HOME、XDG 目录和配置。只继承 PATH、系统路径和语言环境；不继承凭据或桌面会话。端口随机分配且必须高于 4300。外网请求在夹具环境中指向不可达的环回代理，环回假上游直连。结束时对自己的进程组发送 TERM，退出后清除残余子进程，再回收临时目录。不会连接运行中的 EMP 或使用真实模型凭据

`fixtures.mjs` 将 `EMP_UPDATE_TEST_REPOSITORY_URL` 和 `EMP_UPDATE_TEST_API_URL` 设为不可达的 `http://127.0.0.1:9/releases`。`services/updates.rs` 的 `test_endpoints()` 仅在 debug 构建接收它们，因此本基线要求 debug 二进制；日志中的 `EMP update retry ... os_error 111` 是预期结果

## 功能入口

斜线两侧为中文 / 英文可访问名称；表中 U04–U06 只是功能归属，不代表本步骤已经覆盖

| 功能区 | 入口与关键控件 | 典型操作 | 可观察结果 | U 编号 |
|---|---|---|---|---|
| 管理登录 | 终端 `Open in browser:` 输出的一次性 bootstrap URL | 打开链接，刷新页面，重用已消费的 bootstrap | URL 去掉 bootstrap；首次 `/api/session` 成功、重放 401；刷新后仍可管理 | U01 |
| 服务管理 | 添加服务 / Add service → 自定义 API / Custom API；服务徽章 `名称 · API · Model: 数量` → 连接设置 / Connection settings | 输入名称、服务地址、协议和 API Key；保存、编辑、移除 | 服务卡片和 `/api/config` 与持久化配置一致；移除后服务及其模型消失 | U01、U03 |
| 账号与登录目的地 | 添加服务 / Add service → 设备码登录 / Device-code sign-in 或 `auth.json`；Native → 切换登录方式 / Change sign-in method | 导入文件或粘贴；选择添加 Other 或替换 Native；取消、重试登录 | 相同订阅合并；导入 Other 保留 Native 认证；替换 Native 需重读认证文件确认 | U01；设备码完整流程尚未迁入 |
| 服务内模型管理 | 服务卡片的 模型管理 / Manage models → 更新模型列表 / Update model list、手动添加 / Add manually | 读取上游目录，显示/隐藏、编辑和测试模型；搜索后进入编辑，再 Back | `/v1/responses` 实际命中选定上游；隐藏后目录不显示；Back 保留搜索；关闭取消读 | U01、U03 |
| 订阅模型偏好 | 订阅卡片的 模型管理 / Manage models → 最大上下文窗口（Tokens）/ Maximum context window (tokens) | 修改上下文、显示/隐藏、同步显示与上下文到其他订阅 / Sync visibility and context to other subscriptions，保存修改 / Save changes | 取消不写入；保存后刷新仍保留；同步到 Other；已有历史模型偏好保留 | U01 |
| 模型显示 | 模型显示 / Model display 区域 → 修改模型显示 / Edit model display；搜索模型 / Search models | 改显示名称、上下文显示；展开搜索再 Escape | 页面目录和实际模型选择器呈现已保存的设置 | U01；显示名称编辑尚未迁入 |
| 首次设置与接入 | 启用 EMP / Enable EMP → 将 EMP 应用于 Codex / Apply EMP to Codex | 未登录时添加外部服务和模型，然后启用 | 生成隔离 CODEX_HOME 下的 `emp-local-` 认证；配置状态 `emp_applied`；刷新仍可见模型 | U02 |
| 服务迁移 | 服务迁移 / Service migration → 导出 / Export、导入 / Import、局域网 / Local network；打开迁移包 / Open package | 选择服务和模型、预览；Back 切回直接父层；提交导入 | 草稿保留；预览不写配置；提交锁定选择及 Back/Escape；完成后只出现所选服务 | U02、U03；LAN 完整传输尚未迁入 |
| 用量与账单 | 统计 / Statistics；账号或服务徽章；统计中的服务、模型、会话视图 | 按账号→服务→会话查看账单，改变时间范围并刷新 | 实际请求与账单归属一致；未知费用保留未知语义；刷新改变读到的数据 | U04 |
| 通知 | 通知信箱 / Notification inbox；清空通知记录 / Clear notification history、关闭通知信箱 / Close notification inbox | 打开历史、查看未读、清空；关闭通知后继续当前弹窗 | 通知持久化、未读变化；关闭通知不丢失当前编辑 | U04 |
| 更新 | 检查更新 / Check updates | 检查、阅读版本说明、更新；重连或回滚后重读 | 实际进程版本与更新状态相符；失败后说明仍可读 | U05 |
| 恢复与退出 | 恢复原生 / Restore Native；设置 / Settings → 通用 / General → 退出 EMP / Quit EMP；启动失败后的恢复页 | 恢复 Native、退出；处理恢复页操作 | 真实进程完成恢复或退出，Native 配置文件恢复；有活动请求时保留既有排空保障 | U05 |
| 教程 | 使用教程 / Tutorials → 文档 / Documentation、项目说明 / README、图文教程 / Written tutorials | 打开中英文教程，查看操作演示，返回目录 | 随 Core 内嵌的教程离线可读；返回不影响管理状态 | U05 |
| 模组设置 | 模组 / Modules → 对应模组行的安装 / Install、启用 / Enable、停用 / Disable、检查更新 / Check for updates、`设置{模组名} / Configure {name}` | 安装并启用；进入面板；更新后再打开；停用、卸载 | 实际制品版本和请求结果正确；旧 revision 的读和回调停止 | U06 |
| 团队 | 模组 / Modules → 团队空间 / Team Workspace 的配置按钮；安装启用后出现团队入口 | 加入/登录团队；管理员审核共享与成员权限；查看团队用量 | 团队角色及共享订阅与真实团队服务一致 | U06 |
| 语言与键盘 | Switch to English / 切换为简体中文；Tab、Enter、Escape | 切换语言；键盘打开添加服务、填写并保存；键盘隐藏模型、Escape 关闭 | 服务 / Services 关键文案切换；键盘操作保存真实数据；保存模型后关闭弹窗的焦点问题见下方已知问题 | U01；其余 U 可复用 |

当前模型窗口没有独立“删除模型”按钮。U01 验证隐藏模型、目录撤下，以及移除服务时级联删除模型。首次设置没有独立向导，U02 使用当前真实的接入流程，不把 Runtime 已加载模型当作配置保存的同义词

定位优先使用 role、名称、label 和 placeholder。当前自定义服务表单的名称、服务地址、协议，以及模型编辑器的上游名缺少关联 label；`ui.mjs` 的 `field` 先查 label，再接受现有输入 ID：`modal_provider_name`、`modal_provider_base`、`modal_provider_protocol`、`modal_model_upstream`。迁移密码框也复用 `field`，接受 `shadow_export_password`、`shadow_export_password_confirm`、`shadow_import_password`；迁移文件直接定位当前 dialog 内的 `input[type="file"]` 并调用 `setInputFiles`。Lit 页面补齐 label 后密码等字段可继续按名称定位；不要求保留组件结构、HTML 字符串或内部 state

## 已知产品问题

- 保存模型并关闭弹窗后焦点落到 body：`persistState()` 调用 `fill()` 重绘服务卡片，`modalOpener` 指向已断开的旧按钮，`closeModal()` 无法恢复入口焦点。U01 不断言关闭后的焦点，修复时补上回到入口按钮的断言
- `migration-view.js` 的密码和文件 `<label>` 内，帮助按钮排在输入框之前，隐式 label 关联到帮助按钮，密码框没有可访问名称。测试经 `ui.mjs` 的 `field` 回退到输入 ID 定位。迁移提交期间提交按钮显示 Working… 并禁用，U02 按这个名称断言锁定

## 测试、旧验收和故障对应

| 新测试（文件 / 标题） | U 编号 | 承接的 Python 脚本与断言主题 | 会让测试失败的产品错误 |
|---|---|---|---|
| `management.spec.mjs` / bootstrap、键盘添加服务、模型请求、上游 401 与移除 | U01 | `e2e.py`、`e2e_formal.py` 的真实登录和请求；`e2e_model_manager.py` 的读取、显示/隐藏和键盘返回；`e2e_connection_guidance.py` 的上游鉴权失败不清管理登录 | bootstrap 可重放；键盘不能保存服务；Key 未保存或路由到错误模型；上游 401 导致管理退出；服务移除后模型残留 |
| `management.spec.mjs` / auth.json 文件与粘贴合并到 Other；偏好取消、同步、保存并重读 | U01 | `e2e_account_merge_demo.py`、`e2e_login_destination.py`、`e2e_onboarding.py` 的文件/粘贴同账号去重及 Native 保护；`e2e_subscription_catalog.py`、`e2e_subscription_sync.py` 的上下文、可见性、同步和历史偏好保留 | 同一账号重复增加；Other 导入覆盖 Native；取消也保存；同步取了旧草稿；保存后刷新丢失设置或删掉历史模型偏好 |
| `migration.spec.mjs` / 首次设置：从未登录状态添加服务并启用 EMP 本地 Key | U02 | `e2e_onboarding.py` 的未登录用户建立本地 Key；`e2e_model_manager.py` 的服务内手动添加 | 首次启用失败；Key 写到错误目录；模型未保存；刷新后接入状态丢失 |
| `migration.spec.mjs` / 迁移模式返回直接父层并保留各自草稿和选择 | U02 | `e2e_migration_navigation.py`、`e2e_navigation.py` 的 chooser→form→chooser、各模式草稿保留、根层关闭不写 | Back 跨级或直接关闭；重新进入模式时丢失服务选择、密码、文件或选项；导航偷偷保存 |
| `migration.spec.mjs` / 加密文件预览只导入所选服务；提交时禁用 Back 和选择 | U02 | `e2e_migration_navigation.py` 的真实加密包、preview→form、选择重算和提交锁定；`e2e_onboarding.py` 的成功导入结果 | 预览就写入；错误导入未选服务；提交中允许改变选择/Back/Escape；导入后 UI 和持久化结果不一致 |
| `modal-ownership.spec.mjs` / 关闭模型窗口取消尚未完成的读请求 | U03 | `e2e.py`、`e2e_navigation.py`、`e2e_modal_ownership.py` 的窗口读所有权；`e2e_model_manager.py` 的加载和刷新 | Close 未触发浏览器请求取消；离开后继续占有旧窗口；再开后刷新按钮一直禁用 |
| `modal-ownership.spec.mjs` / 旧模型回复不能覆盖新打开服务的窗口和搜索草稿 | U03 | `e2e_modal_ownership.py` 的延迟回复隔离；`e2e_navigation.py` 的新窗口保留 | 第一服务的旧结果覆盖第二服务；新窗口搜索草稿被清空；旧回调改变新窗口控件 |
| `modal-ownership.spec.mjs` / 已提交写操作在关窗后完成；结果不关闭新窗口也不丢失 | U03 | `e2e_modal_completion.py`、`e2e_modal_ownership.py` 的已完成写入、Back/Close/新草稿隔离 | 关窗使已保存结果丢失；旧保存完成后关闭新窗口；覆盖新草稿或禁用新提交；重开看不到保存结果 |

此表是已迁移主题的映射，不是旧脚本整份覆盖完成的声明。开发路由拒绝、设备码/浏览器登录完整目的地矩阵、模型探测矩阵、Vertex、重名冲突、LAN、多服务窄屏、预览超时、模组设备写入和额度重置等独有场景尚未承接。所有原 Python 脚本保留，阶段 7 删除前仍需逐项核销

假上游由 Node 内置 `http` 提供，记录路径、鉴权、模型、请求序号和连接取消，支持显式 gate。管理请求使用 Playwright 路由门控：先向真实 EMP 发请求并取得结果，再暂停向页面交付响应。U03 的读取消观察浏览器 `requestfailed`，不声称覆盖 Rust 到上游的取消链；写入以真实 `/api/config` 和持久化文件为证。U02 导出源实例停止后才启动目标实例，减少驻留内存
