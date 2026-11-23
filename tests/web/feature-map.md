# WebUI 功能地图与浏览器基线

U01–U06 的浏览器测试入口。这里的入口来自 `crates/emp-app/web/` 的正式页面，模组入口还取决于已安装制品及启用状态

## 固定运行入口

在仓库根目录构建一次包含正式页面资产的 debug EMP 二进制；在 `tests/web` 安装测试依赖。Cargo 构建不调用 Node，浏览器测试不调用 Python

```bash
cargo build --locked -p emp-app
cargo build --locked -p emp-mod-decision -p emp-mod-gateway -p emp-mod-aggregate -p emp-mod-team --bins
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
npm test -- usage.spec.mjs
npm test -- recovery.spec.mjs
npm test -- modules.spec.mjs
```

完整入口应发现 **23 个测试**（U06 为 5 个），其中 U04 编辑中打开通知信箱的 1 项标为 `test.fail`（已知产品问题，预期失败）；不能把零测试、跳过项或过滤后的局部结果当作全部基线通过。只运行一个 Chromium headless project，`workers: 1`、不自动重试。用例以 U 编号命名，结果附二进制路径、SHA-256、浏览器和 Node 版本；失败保留 trace、截图和脱敏 EMP 日志

每个 EMP 实例使用独立临时工作目录、HOME、CODEX_HOME、XDG 目录和配置。只继承 PATH、系统路径和语言环境；不继承凭据或桌面会话。端口随机分配且必须高于 4300。外网请求在夹具环境中指向不可达的环回代理，环回假上游直连；浏览器只放行 `127.0.0.1` 上高于 4300 的 HTTP 端口，其余请求中止。结束时对自己的进程组发送 TERM，退出后清除残余子进程，再回收临时目录。不会连接运行中的 EMP 或使用真实模型凭据

`fixtures.mjs` 默认将 `EMP_UPDATE_TEST_REPOSITORY_URL` 和 `EMP_UPDATE_TEST_API_URL` 设为不可达的 `http://127.0.0.1:9/releases`；U05、U06 通过 `release: { repositoryURL, apiURL }` 将其指向本地 Release 服务。`services/updates.rs` 的 `test_endpoints()` 仅在 debug 构建接收它们，因此本基线要求 debug 二进制；默认夹具日志中的 `EMP update retry ... os_error 111` 是预期结果

U06 从 `EMP_BIN` 同目录读取四个 `empmod-*` 可执行文件，缺失或不可执行时直接失败并给出上面的模组构建命令；不自动构建或下载。模组版本和宿主 API 由真实二进制握手读取，Core 版本由 `EMP --version` 读取。`modules-fixture.mjs` 提供与 Rust `ModuleRelease` 相同的资产命名、大小、SHA-256 digest、下载地址和分页 Release 元数据：第一页为当前 Core tag，第二页为独立模组 tag。所有安装及 registry 写入都由正式 UI 安装入口完成，夹具不预写模组 registry

Team 运行真实 `empmod-team serve --state DIR --listen 127.0.0.1:PORT`，状态目录和 HOME 隔离。夹具只通过真实 Team HTTP 接口建立团队管理员账号和邀请码；浏览器负责登录、成员申请、批准、上传、审核及同步。环回 relay 将团队请求交给真实 Team 服务，将模型目录和推理请求交给本地假上游。浏览器的非环回请求及 `/api/team/discover` 被中止，测试使用手动填写的环回地址；邀请码在 Team 服务端直接签发，避免启动局域网发现广播。EMP、Release、上游、relay 和 Team 监听端口均随机且高于 4300

U06 用 `/api/modules` 与操作系统进程表同时确认安装、版本、digest、启停及退出；测试收尾停止自己的 EMP 进程组、清理隔离目录下的残余模组进程并确认退出，Team 子进程等待回收后清理目录。该进程夹具沿用现有 POSIX 运行前提，CI 使用 Linux，并需提供 `ps`

U05 本地 Release 用例通过同一组环境变量切到 Node 环回服务，提供重定向、版本 `9999.0.0`、双语说明及各平台资产元数据，随后返回 HTTP 503，验证上次成功取得的说明仍可读。夹具不提供可安装的包，用例只点击检查并阅读说明；真正的替换安装和回滚由 `crates/emp-app/tests/update_process.rs` 覆盖，浏览器自动重连仍未覆盖

U04 先经真实 `/v1/responses` 发送订阅账号、两个外部服务和不同会话的请求；假上游可省略 usage。页面断言与实际 `/api/usage`、保存后的配置交叉核对，不注入账本、页面 store 或通知。U05 把无效密钥写入隔离目录的 `state/master.key`，由正常启动流程输出正式恢复页；取消、重置、可选备份和退出均通过该页面操作。退出断言等待所启动 EMP 的 exit 事件与退出码，不以 toast 或端口关闭代替进程结果

订阅账号请求体按 Codex 方式以 zstd 压缩，假上游用 `fzstd` 解码（Node 20 的 `zlib` 没有 zstd）

## 功能入口

斜线两侧为中文 / 英文可访问名称；具体覆盖与缺口见下方测试映射，U06 的更新切换等未覆盖项见下方边界

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
| 通知 | 通知信箱 / Notification inbox；清空通知记录 / Clear notification history、关闭通知信箱 / Close notification inbox | 打开历史、查看未读、清空；关闭通知后继续当前弹窗 | 通知历史和已读状态重载后保留；清空持久化；编辑中开信箱待修复 | U04；编辑保留场景为 `test.fail` |
| 更新 | 检查更新 / Check updates | 检查失败后阅读提示；本地 Release 新版本及双语说明；再检查失败后重读说明 | 真实更新 API 返回对应状态与版本；页面保留上次说明；不执行安装 | U05；安装与回滚由 Rust 进程测试负责，页面重连未覆盖 |
| 恢复与退出 | 恢复原生 / Restore Native；设置 / Settings → 通用 / General → 退出 EMP / Quit EMP；启动失败后的恢复页 | 恢复 Native、退出；处理恢复页操作 | 真实进程完成恢复或退出，Native 配置文件恢复；有活动请求时保留既有排空保障 | U05 |
| 教程 | 使用教程 / Tutorials → 文档 / Documentation、项目说明 / README、图文教程 / Written tutorials | 打开中英文教程，查看操作演示，返回目录 | 随 Core 内嵌的教程离线可读；返回不影响管理状态 | U05 |
| 模组设置 | 模组 / Modules → 对应模组行的安装 / Install、启用 / Enable、停用 / Disable、移除 / Remove、`设置{模组名} / Configure {name}` | Decision 安装、启停、进入面板、修改服务与模型、保存、卸载 | `/api/modules` 和真实进程一致；请求命中所选上游；另一标签页停用/卸载后旧面板刷新被 EMP 以 `module_disabled` 拒绝，不到上游 | U06；更新切换未覆盖 |
| 团队 | 模组 / Modules → 团队空间 / Team Workspace 的配置按钮；安装启用后出现团队入口 | 申请加入、批准成员、管理员上传与批准共享、成员同步 | 管理员能进入订阅审核；普通成员没有审核入口；审批前无共享账号、审批后同步并刷新仍可见 | U06；团队用量完整矩阵由 M03 负责 |
| 语言与键盘 | Switch to English / 切换为简体中文；Tab、Enter、Escape | 切换语言；键盘打开添加服务、填写并保存；键盘隐藏模型、Escape 关闭 | 服务 / Services 关键文案切换；键盘操作保存真实数据；保存模型后关闭弹窗的焦点问题见下方已知问题 | U01；其余 U 可复用 |

当前模型窗口没有独立“删除模型”按钮。U01 验证隐藏模型、目录撤下，以及移除服务时级联删除模型。首次设置没有独立向导，U02 使用当前真实的接入流程，不把 Runtime 已加载模型当作配置保存的同义词

定位优先使用 role、名称、label 和 placeholder。当前自定义服务表单的名称、服务地址、协议，以及模型编辑器的上游名缺少关联 label；`ui.mjs` 的 `field` 先查 label，再接受现有输入 ID：`modal_provider_name`、`modal_provider_base`、`modal_provider_protocol`、`modal_model_upstream`。迁移密码框也复用 `field`，接受 `shadow_export_password`、`shadow_export_password_confirm`、`shadow_import_password`；迁移文件直接定位当前 dialog 内的 `input[type="file"]` 并调用 `setInputFiles`。Lit 页面补齐 label 后密码等字段可继续按名称定位；不要求保留组件结构、HTML 字符串或内部 state

模组列表的操作组没有独立可访问名称，U06 从对应的 `Configure {name}` 按钮定位直接父级操作组，再按 Install/Enable/Disable/Remove 名称操作；没有断言该组的标签或组件名称。Decision 与 Team 表单均按现有可访问名称定位。安装当前会自动启用模组，因此生命周期场景包含安装后停用、重新启用，再进入设置面板的路径

## 已知产品问题

- 切换语言后模组入口按钮的可访问名称不更新：`module-settings.js:17` 在初始化时按当时语言设置 `ariaLabel`，语言切换只更新 `data-i18n` 文本。U06 用中英文名称都接受的正则定位，修复后改回精确名称
- 保存模型并关闭弹窗后焦点落到 body：`persistState()` 调用 `fill()` 重绘服务卡片，`modalOpener` 指向已断开的旧按钮，`closeModal()` 无法恢复入口焦点。U01 不断言关闭后的焦点，修复时补上回到入口按钮的断言
- `migration-view.js` 的密码和文件 `<label>` 内，帮助按钮排在输入框之前，隐式 label 关联到帮助按钮，密码框没有可访问名称。测试经 `ui.mjs` 的 `field` 回退到输入 ID 定位。迁移提交期间提交按钮显示 Working… 并禁用，U02 按这个名称断言锁定
- 编辑中通知入口被遮罩覆盖（已实测）：`notifications.css` 的信箱按钮为 `z-index:19`，`style.css` 的整屏弹窗遮罩为 `z-index:20`，点击被 `modal_backdrop` 拦截；键盘 Tab 也被 `handleModalKeydown` 限在弹窗内。`usage.spec.mjs` 的“在编辑中关闭通知信箱保留草稿”标为 `test.fail`，保留期望的 Close/Escape 后草稿断言；修复后该测试会意外通过而报错，届时删除标记

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
| `usage.spec.mjs` / 真实请求的账号、服务和会话账单；未知费用、时间范围与刷新 | U04 | `e2e_account_usage.py`、`e2e_service_usage.py`、`e2e_session_bills.py`、`e2e_report_cache.py` 的真实归属、缺失 usage、时间筛选和刷新 | 账号或服务记错；不同会话混账；缺失 usage 的 token 或费用显示成 0；时间范围无效；刷新仍读到旧记录；搜索未筛掉其他会话 |
| `usage.spec.mjs` / 通知未读、历史重读与清空来自真实保存操作 | U04 | `e2e_notifications.py` 的信箱、已读变化、历史和清空持久化 | 保存无通知；打开后未读未消除；重载丢失历史或已读状态；清空后重载又出现历史 |
| `usage.spec.mjs` / 在编辑中关闭通知信箱保留草稿（`test.fail`） | U04 | `e2e_notifications.py` 的独立信箱导航 | 修复入口并启用后，鼠标无法开信箱、Close/Escape 把编辑弹窗一并关闭或丢失未提交密码都会失败；当前预期失败，不能视为覆盖完成 |
| `recovery.spec.mjs` / 不可达发布源检查失败后提供中英文可读提示 | U05 | `e2e_update_cache.py`、`e2e_updates.py` 的检查失败提示和更新窗口 | 失败后一直停在检查中；只显示错误码；缺少中英文网络原因与操作提示 |
| `recovery.spec.mjs` / 本地 Release 新版本说明在检查失败后仍可重读，不执行安装 | U05 | `e2e_update_cache.py` 的本地发布源、双语说明与失败后保留 | 新版本或说明不显示；检查 HTTP 503 后抹掉旧说明；中文展示英文说明；检查意外下载包或改变运行版本 |
| `recovery.spec.mjs` / 恢复 Native 还原 Codex 文件；通用设置退出真实 EMP 进程 | U05 | `e2e_native_history_restore.py` 的页面恢复及退出入口；本例只承接配置恢复和进程生命周期 | 恢复接口成功但 Codex 文件未还原；覆盖原认证；恢复误退出 EMP；设置退出仅显示成功而进程仍活着；退出未恢复配置 |
| `recovery.spec.mjs` / 中英文教程与演示离线可读，返回目录保留管理状态 | U05 | `e2e_tutorials.py`、`e2e_guide_updates.py` 的正式离线教程与返回 | 教程依赖外网；演示损坏；中英文 README 选错；本地指南链接跳到外网；返回后目录选择、管理登录或保存的服务丢失 |
| `recovery.spec.mjs` / 启动恢复页取消保留旧数据，直接重置进入正式管理页 | U05 | `e2e_startup_recovery.py` 的正式失败入口、取消、默认不备份、重新配置 | 损坏主密钥没有正式恢复页；取消已改文件；未确认就清除；重置无效；残留暂存数据；Native 文件被重置；管理页无法登录 |
| `recovery.spec.mjs` / 启动恢复页取消保留旧数据，备份后重置进入正式管理页 | U05 | `e2e_startup_recovery.py` 的可选备份与正常启动 | 勾选备份却丢失旧配置或密钥；备份内容改变；新密钥未创建；重置后不能进入管理页；Native 认证被清除 |
| `recovery.spec.mjs` / 启动恢复页退出真实进程并保留损坏数据 | U05 | `e2e_startup_recovery.py` 的独立恢复退出 | 退出请求成功但进程仍运行；退出误清除旧配置、密钥或 Native 文件 |
| `modules.spec.mjs` / 模组安装、启停、设置入口与卸载对应真实状态和进程 | U06 生命周期与设置入口 | `e2e_official_modules.py` 的页面安装启停与面板入口 | UI 只显示成功而未安装；启用无进程、版本/digest 错误；停用进程仍存在；未启用也能进入面板；卸载状态或入口残留 |
| `modules.spec.mjs` / Decision 面板保存所选服务和模型后真实请求切换上游 | U06 路由型模组设置与实际请求 | `e2e_decisions.py`、`e2e_routing.py` 的设置选择、保存与真实请求 | 保存或刷新后丢失选项；继续命中原服务；未转发新模型；使用错误服务凭据 |
| `modules.spec.mjs` / 另一标签页停用后旧面板刷新被 EMP 拒绝且不到上游 | U06 停用后的旧回调 | `e2e_official_modules.py` 的旧面板生命周期 | 停用后 `/api/decisions/models` 仍返回模型或转发到上游；旧面板不提示失败或抛页面错误 |
| `modules.spec.mjs` / 另一标签页卸载后旧面板刷新被 EMP 拒绝且不到上游 | U06 卸载后的旧回调 | `e2e_official_modules.py` 的卸载与旧面板生命周期 | 卸载后 `/api/decisions/models` 仍返回模型或转发到上游；旧面板不提示失败或抛页面错误 |
| `modules.spec.mjs` / Team 加入、管理员审核入口与成员同步获批共享订阅 | U06 Team 角色页面与共享结果 | `e2e_official_modules.py` 的团队加入、权限页面和共享订阅同步 | 成员申请直接生效；管理员缺少审核入口；成员看到审核操作；未审核就能同步；获批后无法同步、凭据缺失或页面刷新丢失账号 |

此表是已迁移主题的映射，不是旧脚本整份覆盖完成的声明。开发路由拒绝、设备码/浏览器登录完整目的地矩阵、模型探测矩阵、Vertex、重名冲突、LAN、多服务窄屏、预览超时、模组设备写入和额度重置等独有场景尚未承接。所有原 Python 脚本保留，阶段 7 删除前仍需逐项核销

U04/U05 的其余缺口：通知自动消失计时、悬停暂停和存储阻断；账号用量缓存失效的完整竞态矩阵；真实升级期间的页面重连和回滚提示；Native 历史文件修复、活动请求排空和 Runtime 目录核验。本步骤不执行安装，也不添加故障注入产品入口；页面重连和回滚提示仍缺一个不替换当前二进制的正式触发入口。启动恢复页已能用损坏的临时主密钥正式触发，无需新增产品入口

假上游由 Node 内置 `http` 提供，记录路径、鉴权、模型、请求序号和连接取消，支持显式 gate。管理请求使用 Playwright 路由门控：先向真实 EMP 发请求并取得结果，再暂停向页面交付响应。U03 的读取消观察浏览器 `requestfailed`，不声称覆盖 Rust 到上游的取消链；写入以真实 `/api/config` 和持久化文件为证。U02 导出源实例停止后才启动目标实例，减少驻留内存

U06 的旧回调用例先证明刷新控件能完成真实请求，再在第二个标签页停用/卸载，原页面的 Decision 面板保持打开。页面没有跨标签页的模组状态推送，旧面板仍会发出 `/api/decisions/models`；契约是 EMP 返回 `404 module_disabled`、两个假上游都收不到请求、面板显示失败提示且没有页面异常。Decision 假上游的模型条目带 `architecture.output_modalities: ["decisions"]`，`openai` 协议的决策发现只收这类模型（`gpt-6-luna` 除外）

U06 已由 coz 用真实 debug Core 和四个模组二进制运行 5/5 通过

本步骤没有覆盖更新前后 revision 切换、旧 revision 的在途读与定时器回收：当前只读取 `EMP_BIN` 同目录的单版本制品，缺少另一份版本更高、digest 不同且兼容同一宿主 API 的真实制品及其 CI 输入约定。安装、更新和面板入口本身已有，缺的是升级制品；同版本重复下载不能替代更新证据。Gateway/Aggregate 的业务面板、Team 的 VPS 维护者完整权限矩阵、局域网发现及完整用量由对应 M/U 场景继续承担，本步骤未写 U04/U05 或修改旧 Rust/Python 验收
