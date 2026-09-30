<p align="center">
  <img src="assets/branding/easy-multi-provider-icon.svg" alt="EMP 标志" width="112">
</p>

<h1 align="center">EMP — EasyMultiProvider</h1>

<p align="center"><strong>一个 Codex 模型选择器，切换多个 ChatGPT 账号和外部 API 模型。</strong></p>

<p align="center"><a href="README.md">English</a> · <a href="#快速开始">快速开始</a> · <a href="https://github.com/Killow1998/EasyMultiProvider/releases/latest">下载</a></p>

EMP 在本机运行，主要解决两件事：

1. **多账号共用一个模型选择器。**导入其他 ChatGPT 订阅账号后，直接在同一个
   `/model` 列表或 Codex App 菜单里选择对应模型，不必退出当前账号再登录。
2. **外部模型进入模型选择器。**添加 DeepSeek 等 API Provider 并导入模型。
   EMP 适配受支持的协议，让会话、工具调用和模型切换在上游模型支持时尽量接近
   Codex 原生模型的使用体验。

在 EMP 的本地网页配置账号和 Provider，再将模型列表应用到 Codex。

例如，同一个模型列表可以同时出现：

| Codex 中显示的模型 | 请求发送到哪里 |
| --- | --- |
| `gpt-5.6-luna` | 当前 Codex 登录账号 |
| `team/gpt-5.6-luna` | 导入的 ChatGPT 订阅账号 |
| `deepseek/deepseek-v4-pro` | 从 DeepSeek 官方 API 导入的模型 |

带前缀的名称只是示例；账号、Provider 和模型由你选择。选中 `team/gpt-5.6-luna`
时使用导入的账号，选中 `deepseek/deepseek-v4-pro` 时使用 DeepSeek API Key。编码任务、
权限和工具仍由 Codex 管理。EMP 在本机统一管理模型列表、加密凭据和账号额度。

当前源码版本为 `v0.12.5`。

## 功能

- 增量扫描本机 Codex 历史，按时间段、账号和服务查看历史及实时 token 用量与 API 等价金额，价格每天后台更新。
  详见[用量统计说明](docs/usage-accounting.md)。
- 在“计价对照”中把没有公开价格的模型按另一个模型计价（例如按 `gpt-5.5`）；
  仍然没有价格的请求按 0 计，并会单独标出。

- 在同一个 Codex 模型选择器中使用原生模型、其他 ChatGPT Subscription 和
  外部 API 模型。
- 使用 `team/gpt-5.6-luna`、`provider/model` 等清晰前缀路由模型。
- 导入多个 Codex Subscription 账户并刷新可用额度信息。
- 从经过认证的 Codex 目录响应同步 Native 和 Subscription 模型及能力，并自动刷新；
  刷新失败时保留最近一次成功缓存，同时保留已有的可见性、别名和上下文覆盖值。
  新公开模型无需等待 EMP 发布或手动编辑模型列表。
- 在模型、Provider 和 Subscription 账户旁查看请求活动：活动点标记进行中的请求，
  提示文字显示数量；近期状态表示刚完成的请求并会自动过期；事件流断开时会清除活动状态。
- 通过 Web UI 添加官方或自建 Provider。
- 通过已安装的 Claude Code CLI 使用 Claude：可复用当前操作系统用户的 Claude Code
  登录，也可连接兼容 Claude Code 的 CPA Base URL 和 API Key。本地登录模型需通过
  “Add model”手动添加别名或完整模型 ID。工具调用由 Codex 执行，回复在生成完成后显示。
- 只有当 Claude 模型的已配置能力和所选上游都支持时，才会转发图片输入和受支持的内联文档内容。
- 拉取 Provider 模型，自由选择导入模型，修改上下文窗口，执行测试并隐藏
  不常用模型。
- Provider 报告或支持时，保留文本、图片、推理和结构化工具调用能力。
- Codex 可以使用现有模型 slug，把原生子任务委派给外部模型；子任务及其权限仍由
  Codex 管理。
- 凭据只在本机加密保存。
- 保存私有且有容量上限的诊断日志，便于后续排查问题。
- 在“性能与健康”中查看原生与外部模型的缓存命中率，按每 10 分钟的缓存输入
  token 总数与输入 token 总数计算；跳过空闲时段，缺少上游数据时显示“未提供”。
- 在“性能与健康”中查看各近期模型最近 20 次有效调用的 TTFT/TPS 中位数，并与
  前一个窗口比较；历史会跨 EMP 重启保留。实际采集到 Fast 请求时，再区分 OpenAI
  速度模式。同时显示成功、429、502、503 和 504 的观测比例；
  不保存提示词或回复内容。
- 通过密码保护的 `.emp` 文件导入和导出数据。
  导出时可多选 Native、其他 Subscription 和 External Provider，默认全选。
  Native 包含模型显示配置和本机 Codex 登录凭据；导入后作为额外 Subscription 账号，不替换当前登录。选中模型的共享分组显示设置会一并导出。
  导入时只有身份一致的账号才会更新；冲突账号保留双方，并为导入项分配新 ID/prefix，同步对应显示设置。导出结果按文件内容统计，缺少 Native 凭据时明确提示。
- 在客户端支持时，保留 Codex 原生会话、`resume`、WebSocket、压缩和 MCP 行为。
- 在不同 Provider 之间实时切换时，使用 Codex 可见历史，不转发 Provider 私有状态。
- 若会话含有 EMP 处理的压缩数据，想切回原生 Codex，先结束活动任务并关闭 Codex，
  再在 Web UI 的 Codex 集成中点击“恢复原生”（Restore Native）。EMP 只转换受影响的
  已保存历史，改写前备份历史文件，然后恢复原生设置。恢复完成后 EMP 会退出；重新打开
  Codex 即可在受支持版本上继续原对话。也可使用命令行 `EMP restore`。
- 外部模型可以使用 Codex 独立联网搜索；EMP 优先使用当前 `.codex` 登录，读取不到时
  自动回退到可用的导入账号，无需向外部 Provider 暴露凭据。

## 安装

EMP 不会捆绑或替代 Codex。EMP 只修改共用的 `~/.codex/config.toml`，CLI、App、
IDE 插件等所有 Codex 客户端都会读取这份设置，所以不需要选择客户端；多个客户端和
workspace 可以同时工作。查询账户余量时，EMP 会在已知位置（Codex App、`.codex`
托管 runtime、VS Code/Cursor 插件、`PATH` 中的 `codex`）选择受信任、可执行且已观测到
版本的 Codex 引擎来执行。
Unix 上还会查找 `NVM_DIR`、`$XDG_CONFIG_HOME/nvm`、`~/.config/nvm` 和 `~/.nvm`
中的安装。即使桌面启动器的 `PATH` 不含 nvm，npm 安装的 Codex 也可以使用同一安装中
受信任的 Node，无需手动创建 `codex` 或 `node` 链接。

路径检测覆盖 Windows x64、Linux x64、macOS Intel 和 macOS Apple Silicon，
支持这些平台上 CLI、IDE 插件和桌面 App 的已知安装布局。

EMP 不会停止或重启你正在使用的 Codex 后端。余量查询使用独立的 CLI 辅助进程，
不会打开桌面 App。点“将 EMP 应用于 Codex”后，集成卡片会分别显示
已保存的设置和正在运行的 Codex 加载的模型目录。如果目录仍是旧的，请在当前工作
允许时，通过平时使用的启动器重启 Codex，然后再次检查目录。

Codex 下次连到 EMP（拉取模型列表或发起对话）时，EMP 会通过本地控制通道读取
`model/list`，并直接推送结果到 Web UI，不做后台轮询；也可以手动检查。
目录匹配说明模型已可见，不代表已有会话采用了新的服务商设置。检查只读取状态，
不会重新加载后端，也不会把目录匹配当作请求路由已经切换的证明。控制接口不可用时，
EMP 会显示设置已保存、共享模型目录尚未验证，不据此判断 App 已停止。

Linux 同时支持扫描当前 `CODEX_HOME/plugins/.plugin-appserver/codex`，
其他 AppImage 或发行版的安装布局仍需单独验证。

EMP 支持 Codex CLI、桌面 App 和 IDE 扩展，不以统一的最低引擎版本拒绝接入。
余量读取、额度重置和模型目录检查分别取决于所用引擎提供的接口；缺少某个接口时，
只报告对应操作不可用。认证、协议和对话历史保护仍然生效。

诊断信息分别显示 App／扩展版本与 Codex 引擎版本。“引擎可用”表示 EMP
能够运行它并读到版本，不代表所有功能都已验证。模型目录刷新使用实际观测到的
引擎版本；无法确定时保留已有缓存，并提示该次刷新不可用。

EMP 会自动刷新模型列表。主界面显示 Codex 版本和“EMP 模型已加载”；如果提示
重新打开 Codex，完成当前操作后重新打开即可。

外部子任务委派、后续任务和工具调用的说明见[子任务兼容性](docs/external-collaboration.md)。

Subscription 的编辑窗口可以逐模型设置上下文 token 数。留空使用模型默认值；
“刷新模型上限”会用该账号的登录凭据拉取订阅目录，输入不能超过目录中的
最大上下文。0.95 的默认预留系数保持不变，例如设置 872,000 后可用约 828,400。
设置同时影响 Codex 模型列表和 EMP 的请求上下文检查，不会修改 API 地址或
目标 Codex 的当前登录。已有任务能否立即采用新窗口取决于客户端是否刷新了
目录；新建任务后应确认有效上下文。Native 导出后，其上下文设置随导入的账号
迁移，不覆盖目标机器 Native 的设置。外部 Provider 模型同样按 95% 计算，
例如 256,000 的窗口在 Codex 模型列表中显示为 243K。

在右侧“模型显示”区域使用全局眼睛开关，可统一显示或隐藏该区域及 Codex 模型选择器中的
Native、Subscription 和 External Provider 上下文标签。左侧模型列表不重复显示这些标签。
此操作只改变模型目录标签，不会修改上下文窗口或请求限制。

### 预构建安装包

从 [GitHub Releases](https://github.com/Killow1998/EasyMultiProvider/releases)
下载已经审核的构建。
[Package workflow](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/package.yml)
会在发布前原生构建并实际启动检查以下产物：

| 平台 | 产物 |
| --- | --- |
| Windows x64 | 独立 `.exe` |
| Ubuntu 22.04+ x64 | `.tar.gz` 和用户安装脚本 |
| macOS Intel | 包含 `.app` 的 `.dmg` |
| macOS Apple Silicon | 包含 `.app` 的 `.dmg` |

最简单的桌面启动方式是：

- **Windows：**双击 `EMP.exe`。
- **Linux `.tar.gz`：**在下载目录运行以下命令，再从应用菜单打开 **EMP**：

  ```bash
  tar -xzf EMP-linux-x86_64.tar.gz
  cd EMP
  ./install-user.sh
  ```

- **macOS：**打开 DMG，把 **EMP** 拖入“应用程序”，然后双击。

当前发行版不提供 Linux `.deb`。已经安装的系统管理版本仍由原有包管理器负责；用户安装脚本不会自动移除它。

EMP 会自动打开已认证的 Web UI，并保留一个显示状态和日志的终端窗口。看到
`EMP listening on ...` 就表示启动成功。使用 EMP 时请保持该终端
开启；按 `Ctrl+C` 可以干净退出，也可以关闭终端结束进程。正常退出后会显示
`EMP stopped.`。

桌面启动会把配置保存到各系统标准的用户目录：

- Windows：`%LOCALAPPDATA%\EasyMultiProvider\config.json`
- macOS：`~/Library/Application Support/EasyMultiProvider/config.json`
- Linux：`$XDG_CONFIG_HOME/easy-multi-provider/config.json`，未设置时使用
  `~/.config/easy-multi-provider/config.json`

Linux 用户安装把程序放在 `$XDG_DATA_HOME/easy-multi-provider/EMP`，默认是
`~/.local/share/easy-multi-provider/EMP`；启动入口是 `~/.local/bin/EMP`。
Linux 用户安装与网页更新不需要 `sudo` 或管理员密码。
配置与账号数据保存在上述用户配置目录，更新程序不会替换它们。

“检查更新”上的红点表示有可用版本。如果旧版 Windows EMP 更新时退出后没有重新打开，
从 [Releases](https://github.com/Killow1998/EasyMultiProvider/releases/latest) 下载
`EMP.exe`，替换已关闭的程序再打开即可，账号和设置仍保留在用户配置目录。
v0.12.5 修复了后续更新中误等已退出进程的问题。
已有系统 `.deb` 安装不会被自动卸载；停止
旧 EMP 后可安装用户版本，旧配置应先备份再迁入用户配置目录。

需要命令行控制时仍可显式启动服务。下载 Windows 可执行文件后，在 PowerShell 中运行：

```powershell
.\EMP.exe --version
.\EMP.exe serve --config config.json
```

解压 Linux `.tar.gz` 后运行：

```bash
./EMP --version
./EMP serve --config config.json
```

Windows 可执行文件和 Linux 压缩包中的程序在无参数运行时，也会进入自动打开浏览器的
桌面模式。

当前 macOS workflow 产物属于未签名的开发构建。公开分发仍需要 Apple Developer
ID 签名和公证。

### 从源码安装

安装 Git 和 Rust 工具链（[`rustup`](https://rustup.rs/)），然后构建 EMP：

```bash
git clone https://github.com/Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
cargo build --locked --release -p emp-app --bin EMP
```

运行测试：`cargo test --locked --workspace --all-targets`。测试全部使用临时目录和
本地假上游，不会读写真实的 `~/.codex`，也不会调用真实 Provider。

## 快速开始

在解压后的 Linux `.tar.gz` 目录中显式启动打包后的 EMP：

```bash
./EMP serve --config config.json
```

在源码目录中运行时使用：

```bash
./target/release/EMP serve --config config.json
```

首次启动时，EMP 会自动创建本机私有加密密钥，不需要设置环境变量，也不需要
手动生成密钥。

### `.emp` 版本兼容性

EMP v0.9.0 至 v0.9.9 使用同一种加密迁移格式。当前版本可以导入其中任意版本
导出的文件；完整的 v0.9.0-v0.9.7 交叉测试确认，账户与凭据、Provider 与 API Key、
模型和模型显示名均可保留。若要无损迁移所有设置，请使用相同或更新的 EMP 版本：
旧程序无法保留它发布后才新增的设置，例如 v0.9.3 加入的模型系列显示设置和原生
模型可见性。

终端会输出一个一次性浏览器地址。打开后：

1. 导入 Codex Subscription 账户，或者添加 API Provider。
2. 拉取 Provider 模型并选择需要导入的模型。
3. 按需调整模型显示状态或上下文窗口。
4. 点击 **将 EMP 应用于 Codex**。
5. 正常启动 Codex，通过 `/model` 或 App 模型菜单选择模型。

使用 Claude 时，在 Web UI 中选择 **Add Provider → Claude**，再选 **Local Claude subscription** 或 **CPA**。两种方式都需要安装 Claude Code CLI。

- **Local Claude subscription** 使用当前操作系统用户的 Claude Code 订阅登录。每次请求前，EMP 会检查 Claude.ai 订阅登录；CLI API Key 和 Console 登录不适用于本机模式。若需登录，请在 Claude Code 中登录。使用 **Add model** 手动添加别名或完整模型 ID，即可在 Codex 模型选择器中选择。
- **CPA** 使用兼容 Claude Code 的 CPA Base URL 和 API Key。直接连接 Anthropic API 时，请另加 **Anthropic API** Provider。

只有当前原生账号时，可以跳过导入账号和 Provider：在“当前 Codex 登录 → 编辑”
中隐藏模型，在“模型显示”中修改显示名称，保存后点击“将 EMP 应用于
Codex”。至少保留一个可见模型，显示名称不会改变模型 ID。

使用 ChatGPT 登录时，受支持的 Codex 客户端可在运行期间获取模型目录变更。
目录变更会在后续请求时生效；若新模型尚未显示，可重新打开模型菜单查看。
从旧版静态目录升级，或修改 Codex 的 Base URL 后，需要重启 Codex 一次。
没有 ChatGPT 模型发现能力的客户端继续使用静态目录。
回退至 EMP 0.9.91 或更早版本前，请先恢复原生 Codex。

EMP 默认监听 `http://127.0.0.1:4200`。只有端口被占用时才需要使用
`--port` 修改端口。

每次启动还会输出 `Diagnostic log: ...`。EMP 会在该文件中保存结构化运行元数据，
后续遇到问题时无需再依赖用户复述整个过程。日志位于 `state/logs/`；总量超过
10 MiB 后会自动删除最老的分片。日志不会保存提示词、模型回复、工具参数或结果、
HTTP 正文、请求头、Cookie 和凭据。

## Web UI

- **账户**：导入 `auth.json` 或 `auth.json.bk1` 等备份文件。导入时只需要填写
  账户 ID。显示名称 / 显示前缀可以稍后修改，支持 emoji；实际路由前缀保持不变。
  点击**刷新**会实时查询额度并保存新的
  快照。EMP 运行时每 5 分钟自动采样一次，并提供 1 小时、1 天、1 周和最多 15 天
  的本地余量趋势；历史只包含额度指标，不包含凭据。每个 Subscription 都能控制哪些
  Coding Agent 模型显示在 Codex 中。
- **Provider**：选择支持的官方预设，或者通过 Base URL 和 API Key 添加自建
  Provider。
- **模型**：拉取上游模型，并进行导入、测试、编辑、隐藏或删除。模型按照
  Provider 分组显示。
- **Codex 集成**：把当前 EMP 模型目录应用到默认 Codex，也可以在同一页面恢复
  Codex 原生路由。页面分别显示文件状态和共享后端当前暴露的模型 ID；EMP 不控制
  共享后端的进程生命周期。

EMP 会自动检测启动环境或操作系统中的代理设置。

## 本地安全

EMP 默认只允许本机访问管理界面。Subscription 凭据和 Provider API Key 会在
本机加密保存，保存后不会重新返回浏览器。本地配置、加密状态和生成的模型目录
均已排除在 Git 提交之外。
