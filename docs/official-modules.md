# EMP 官方模块

从首页右上角「设置」旁的「模组」安装 Decision API、多设备网关、聚合路由或团队空间。发现兼容的独立更新时，「模组」入口和相应的「检查更新」图标会显示红点。打开更新说明后可选择安装。点击「?」查看模组详情。安装、启用、停用和更新即时生效。先停用再移除。移除团队空间会清除本机共享凭据，VPS 账本继续保留。

启用中的模组用图标绿色外沿表示。停用会关闭相应入口，已进入上游的 Decision 请求继续完成。Aero 还在接力时，可先停用并等待当前请求完成，随后继续更新或移除。

EMP 从官方 GitHub Release 读取对应平台的包。独立模组发布标签使用 `emp-mod-<id>-api2-v<版本>`，例如 `emp-mod-decision-api2-v0.10.0-beta`。没有兼容的独立发布时，安装器从 Core 对应的 Release 取包。EMP 核对下载地址、大小和 SHA-256，以及子进程报告的模组 ID、版本和宿主接口。已安装且兼容的模组可离线启动，并可在 Core 升级时复用。单个模组更新后仍保留此前可启动的版本，以便启动失败时回退。

从 v0.14.3 通过 EMP 的应用内更新升级时，新 Core 从过渡更新包内校验并启用 Decision API、多设备网关和聚合路由。校验或启动失败时，旧更新程序会恢复原安装。手动替换程序后可在模块页安装原有模块。

## Aero 上令下效

Aero `0.10.0-beta` 独立安装。启用后，首页服务工具栏显示「上令下效」。选定规划和执行所用模型，Codex 选择器会显示对应的 `aero/配对名称`。Core 管理权限、实际调用和用量。Aero 保留配对与会话侧车库，由 Codex 处理工具与用户批准。

Codex 优先通过 WebSocket 调用 Aero，HTTP + SSE 也可使用。Core 将 Worker 的流式事件转发给客户端。Aero 用同一调用方、会话和配对最后已完成的响应接收增量输入。请求取消时丢弃本轮未完成的状态；失效的响应 ID 可用完整输入重新续聊。

从 Aero 实验版切换时，把原配置中的 `aero_profiles` 和配置目录下的 `aero/sessions.sqlite3` 一起保留。沿用原配对名称和 Codex 会话，便于匹配已经保存的编排状态。

Aero 采用平台独立的单文件资产。开发时在安装了 PyInstaller 的 Python 环境中运行：

```sh
EMP_AERO_PYTHON=/path/to/python cargo xtask package-module aero --output /tmp/emp-aero
python3 -B modules/aero/verify_worker.py --binary /tmp/emp-aero/EMP-mod-aero-linux-x86_64
python3 -B modules/aero/verify_core.py --core target/debug/EMP --module /tmp/emp-aero/EMP-mod-aero-linux-x86_64
```

## 团队空间

团队空间客户端和 VPS 服务使用同一个官方团队模块。VPS 只需要该模块和提供 HTTPS 的反向代理。

首次启动时通过 VPS 的服务环境配置以下变量。生产环境可用权限为 `0600` 的 systemd 环境文件保存。

| 环境变量 | 含义 |
|---|---|
| `EMP_TEAM_NAME` | 第一次创建空间时保存的显示名称 |
| `EMP_TEAM_ADMIN` | 首位 VPS 管理员的账号 |
| `EMP_TEAM_ADMIN_PASSWORD` | 首次创建 VPS 管理员的登录密码 |
| `EMP_TEAM_SPACE_PASSWORD` | 用于持久数据加密的空间密码，每次启动需要 |

密码至少 12 个字符。首次初始化结束后，启动服务时只需要空间密码。VPS 维护者可在团队首页下载加密的 `.empteam` 备份。备份保留工作空间 ID、成员、订阅快照和用量账本。浏览器下载支持最多 64 MiB 的 SQLite 快照。更大的工作空间可在 VPS 上用 SQLite 的 `.backup` 命令生成一致快照。

在目标 VPS 上设置与原空间相同的 `EMP_TEAM_SPACE_PASSWORD`，恢复到空目录后再启动服务和 HTTPS 反向代理。

```sh
empmod-team restore --state /var/lib/emp-team --from /path/to/EMP-team.empteam
empmod-team serve --state /var/lib/emp-team --listen 127.0.0.1:4310
```

恢复后，成员在 EMP 中使用原账号重新登录。沿用原 HTTPS 域名即可继续使用同一空间。

```sh
./EMP-mod-team-linux-x86_64 serve --state /var/lib/emp-workspace --listen 127.0.0.1:4310
```

将工作空间的 HTTPS 域名反向代理到 `127.0.0.1:4310`。团队服务只监听环回地址，无需在 VPS 上安装 Codex。管理员在自己的 EMP 中登录该域名后管理空间。

| 角色 | 可用操作 |
|---|---|
| VPS 维护者 | 指定团队管理员，管理成员和权限，查看全部用量 |
| 团队管理员 | 管理普通成员及其共享权限，查看全员用量，为自己和普通成员设置当前周期的 USD 上限 |
| 成员 | 查看自己的用量，按授权上传或同步 |

启用模组后，主页把「团队空间」放在「服务迁移」右侧。团队成员登录后，导入、导出和隧道需要 VPS 确认管理员权限。首次打开团队空间时填写 HTTPS 域名、昵称、七位团队码及账号密码。管理员点击「管理 → 成员管理 → 添加成员」开启 3:22 的口令，按钮显示团队码和倒计时，再点即可复制。同一 IPv4 局域网的客户端可在有效期内查找域名。管理员按昵称审批，已有账号可直接登录。

团队首页直接显示身份、团队名称、成立和加入日期。成员可查看今日 USD 用量和上限，管理员和 VPS 维护者可查看团队人数与团队用量。点用量卡片右侧箭头展开明细，可切换今天（UTC）、近 1 天、7 天、30 天或全部。成员可直接查看共享订阅或同步。管理员从「管理」进入「成员管理」或「订阅管理」。

成员、团队管理员和 VPS 维护者均可从自己的 EMP 登录工作空间。要登录另一台设备，点击团队主页的「复制登录」，粘贴到新设备团队登录页的「账号」栏，再输入密码。复制内容只包含团队名称、VPS 地址和账号。工作空间的管理、同步和用量数据都以该 VPS 为准，各设备用量按成员合并。

「退出登录」清除本机团队登录和已同步凭据。VPS 收到请求后撤销这台设备的会话。管理员可从成员列表的「移出」结束该成员所有设备的团队会话、停止其订阅共享，并保留团队用量和审计记录。若要重新加入，该账号需用团队码重新申请。团队管理员可从「成员管理」移交权限，选择一位已启用的成员接任。交接后，原管理员的其他设备继续登录并变为普通成员。

获授权的持有者可以上传当前 Native Codex 订阅。VPS 加密保存有到期时间的访问快照，刷新令牌保留在持有者设备。管理员审核后，成员可同步并在 Codex 模型选择器中使用，持有者可以离线。同一账号重新上传只更新凭据，已批准的无需再次审核；换成其他账号仍需审核。停止共享后下次同步会停用对应路由，已下载的上游凭据由持有者通过上游撤销。退出团队登录或卸载团队模组会删除本机已同步的凭据，个人登录与本机、VPS 账本继续保留。

切换到 EMP Local Key 时选「保留原账号」，之后在团队空间点「上传」，EMP 会使用本机保留的原 Native 订阅。已经切换的设备可在第一次上传时选择本机订阅。选择「移除原账号」后，请重新登录要上传的订阅。上传窗口会显示本次使用的账号。

「共享订阅」横向显示成员昵称、套餐、团队用量和 VPS 快照状态。「订阅管理」在昵称左侧用绿点和红点显示有效或失效。成员本机的副本失效时，点击「同步」可恢复仍有效的 VPS 版本。如果共享账号已是本机 Native，同步只登记用量归属。本机已有同账号的 Other 时，模型选择器优先显示本机名称和模型设置。本机凭据失效后同步，原入口可使用仍有效的共享凭据，并按团队权限计量。同名订阅会加数字区分，已使用的路由名称会保留供历史会话继续使用。

管理员可在「订阅管理」点击垃圾桶移除 VPS 保存的订阅凭据。成员下次同步时停用该团队入口。历史用量和离线补报仍计入原共享订阅，持有者重新上传后需要再次审批。

VPS 按成员和设备计量已批准的共享订阅。登录团队后，本机 Native 或 Other 与已批准的共享订阅匹配时，EMP 可直接归集新用量。需要获得共享凭据时，点击「同步」。已产生的用量会在本机排队，连接恢复后补报。每日上限以 UTC 零点为边界，优先采用能够确认的实际美元金额，其次采用 API 等价估算。价格未知的请求单独显示。达到 90% 或 100% 时，成员 EMP 发送非模态系统通知，记录可在 WebUI 通知中心查看。

团队管理员可在「成员管理」为全队选择「每日 USD」或「每周 USD」。看板和提醒采用当前周期，切换后仍保留两种周期各自的上限。每周模式可通过「每周重置」选择星期和整点。首次打开团队看板的管理员决定团队时区，默认周日 16:00，之后沿用该时区及其夏令时。VPS 按请求完成时间将各设备的花销合计到同一个成员。团队订阅请求发出前，EMP 先补报本机已完成的用量，再向 VPS 核对当前周期的预算。达到上限或暂时无法核对时，EMP 暂停新的团队请求。已发出的请求可继续完成，多设备同时开始的请求可能让最终花销超过上限。该功能需更新桌面 EMP、团队模组和 VPS 上的团队服务。

「成员管理」可在表格内直接修改当前周期的 USD 上限、Fast／Ultrafast、上传和同步权限。取消勾选「可用」会终止该成员的团队登录。VPS 维护者可以从昵称左侧的身份图标更改角色，也可以任命多位团队管理员。预算字段按 Enter 或离开时保存，留空显示 ∞。Token 和花销显示在同一列，悬停或用键盘聚焦可查看所选时间内各模型的用量。统计区间和额度周期独立。成员同步或重新登录后，由 EMP 对团队共享模型的请求校验。个人模型及账号额度沿用本机设置。

管理员和 VPS 维护者可以在「成员管理 → 操作日志」查看成员审批、权限及每日上限变更、管理员交接、共享批准和停止共享、团队共享订阅的重置请求。共享订阅的重置在 VPS 确认有权操作时记录，具体结果由上游返回。

Banked Reset 的次数和到期信息可在同步后的订阅详情查看。个人 Native 持有者按原流程操作。普通成员可查看共享订阅，管理员要在自己的 EMP 同步该订阅后执行重置；EMP 向 VPS 验证当前版本的权限。

「成员资料」显示团队成立和加入日期、团队账本中的累计 Token、估算及已知实际费用。近 30 天活动按固定 PST 日（UTC−08:00）显示。客户端在在线当天签到后生成该日的记录。当天没有上线记录时留空。团队管理员可查看所有团队成员的资料，并决定成员能否查看彼此的用量，默认只开放自身。

## 开发验收

Team 的 Rust 黑盒入口是 [`tests/e2e/team.rs`](../crates/emp-app/tests/e2e/team.rs)，专用夹具在 [`tests/support/team.rs`](../crates/emp-app/tests/support/team.rs)。五个 `m03_` 测试均为 ignored，运行时需要与 EMP 同目录的真实 `empmod-team`；热更新还需要 `EMP_E2E_MODULE_NEXT_DIR` 指向版本更高的 Team 构建目录，沿用 M01 的本地 Release 夹具。

夹具运行两个独立 EMP 和一个真实 Team 服务端。`empmod-team` 无参数时提供桌面模组协议；服务端使用 `empmod-team serve --state DIR --listen 127.0.0.1:PORT`，随机端口大于 4300，SQLite 保存在临时目录的 `workspace.sqlite3`。初始化环境变量为 `EMP_TEAM_SPACE_PASSWORD`、`EMP_TEAM_ADMIN`、`EMP_TEAM_ADMIN_PASSWORD`、`EMP_TEAM_NAME`，夹具只使用合成凭据。桌面端经 `/api/team/*` 调用模组生成的 `/workspace/v1/*` 接口；服务端分别由 enrollment、profiles、accounts、usage、backup 处理加入、资料、身份、账本和备份，共享订阅通过正式发布、审核、同步入口进入本地账号配置。

| 测试名（均以 `m03_` 开头） | 场景与失败条件 |
|---|---|
| `roles_require_enrollment_review_and_enforce_management_permissions` | `vps_admin`、`team_admin`、`member` 的审核与权限；未审核可登录、越权晋升或备份会失败 |
| `two_devices_forward_shared_subscription_and_keep_personal_data_after_uninstall` | 共享订阅真实转发、两设备同成员账本、卸载保留个人服务、凭据和用量；错用凭据、记错成员或删除个人数据会失败 |
| `offline_completion_retries_unacknowledged_usage_without_double_billing` | 已获准的流在服务端停机后完成，恢复后补报；真实服务端入账后丢弃一次回执，要求同事件重试且再次插入数为零；用量丢失、提前确认或重复计费会失败 |
| `budget_and_revocation_block_shared_requests_before_the_upstream` | 预算上限、取消同步权限、禁用成员；拒绝后仍访问上游、共享模型仍可见或个人服务受损会失败 |
| `hot_update_switches_workers_and_delivers_each_usage_event_once` | 放行真实上报回执后热更新，检查新版激活、旧进程回收，以及 Core 和另一设备保持运行；更新前后的事件按 ID 检查交付次数，重复上报会使测试失败 |

用量和权限通过 HTTP 观察；本地转发夹具记录真实服务端接受的上报，在 Team 通道只控制回执时机。假上游的模型目录请求单独响应，业务响应按字节转发，门控流可在上游完成前到达 EMP。每个测试结束调用进程回收断言。取消同步权限场景先执行同步以刷新本地权限；禁用成员场景直接检查下一次请求被拒。共享调用在服务端离线时受预算核对约束，因此离线补报使用停机前已获准的流。

更新下载发生在 Team 更新锁之后，状态 API 只提供激活版本快照。验证更新在锁上等待在途上报，以及旧进程退出与新版激活的严格顺序，需要能在黑盒测试中观察对应的生命周期事件。上报任务由 Core 执行，桌面模组提供策略。

```sh
cargo build --locked -p emp-app -p emp-mod-decision -p emp-mod-gateway -p emp-mod-aggregate -p emp-mod-team
python3 -B scripts/shadow_frontend/e2e_official_modules.py \
  --binaries target/debug --codex "$(command -v codex)" --output /tmp/emp-module-evidence
```

日常只改一个模组时，运行 `cargo build --locked -p emp-mod-decision`，或用 `cargo xtask package-module decision --output /tmp/emp-decision` 生成该平台的安装资产和校验文件。`decision` 可换成 `gateway`、`aggregate` 或 `team`。完整发行包继续使用 `cargo xtask package`，Aero 使用独立的包。

该验收启动三套隔离的 EMP、一个团队服务和模拟上游，使用真实 Codex 选择器以及三个工作空间账号。需要 Firefox 和 geckodriver。仅验收 HTTP、账本和真实 Codex 时可以加 `--no-browser`。使用 `--manual-https` 验收手填 HTTPS 地址，使用 `--lan` 验收同网发现。两种模式均使用 OpenSSL 生成仅供夹具信任的证书，`--lan` 还需要一块启用广播的私有 IPv4 网卡。使用 `--bundled-core /path/to/v0.14.3/EMP` 和 `--transition-core /path/to/packaged/EMP-linux-x86_64` 验证正式旧版更新程序。把本次打包的模组产物放在新 Core 旁，夹具会验证离线启动与损坏回滚。

使用另行构建、版本更高的 Decision 二进制运行 `--decision-update /path/to/empmod-decision`，可以验收独立热更新、损坏包拒绝及更新前后 Core 进程和其他三份模组文件保持一致。

在验收命令后加 `--manual-https --no-browser --hold-demo`，可保留十人演示。命令输出成员、团队管理员、VPS 维护者与未入组页面的本机地址，以及入组表单使用的团队 HTTPS 地址。演示使用三个 Pro、一个 Pro Lite 和六个 Free 的合成账号。完成检阅后按 Ctrl+C 退出演示。
