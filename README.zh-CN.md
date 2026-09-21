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

当前源码版本为 `v0.11.4`。

## 功能

- 增量扫描本机 Codex 历史，按时间段、账号和服务查看历史及实时 token 用量与 API 等价金额，价格每天后台更新。
  详见[用量统计说明](docs/usage-accounting.md)。

- 在同一个 Codex 模型选择器中使用原生模型、其他 ChatGPT Subscription 和
  外部 API 模型。
- 使用 `team/gpt-5.6-luna`、`provider/model` 等清晰前缀路由模型。
- 导入多个 Codex Subscription 账户并刷新可用额度信息。
- 通过 Web UI 添加官方或自建 Provider。
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
- 保留 Codex 原生会话、`resume`、WebSocket、压缩和 MCP 功能。
- 在当前登录、其他 Subscription 和外部模型之间切换时，使用 Codex 自己保存的
  可见历史继续已经压缩过的任务。
- 外部模型可以使用 Codex 独立联网搜索；EMP 优先使用当前 `.codex` 登录，读取不到时
  自动回退到可用的导入账号，无需向外部 Provider 暴露凭据。

## 安装

EMP 不会捆绑或替代 Codex。首次读取集成状态时，它会在有限的已知位置扫描
Codex App runtime、当前 `.codex` 托管 runtime、OpenAI 的 VS Code/Cursor
插件 runtime，以及 `PATH` 中的独立 `codex`。Web UI 会列出版本、合并相同程序，
并允许用户多选计划使用 EMP 的兼容 Codex 客户端；多个客户端和 workspace 可以
同时工作。EMP 会独立自动选择一个兼容 helper 程序用于版本检测和账户余量查询，
这个内部选择不会路由模型请求或限制已选客户端。不兼容或无法读取的 runtime 仍会
显示，但不可选择；可用的预发布或更新版本会明确标记为“尚未验证”。

这些客户端通常共用同一用户级 `.codex` 目录。客户端选择不会创建新的 Codex
配置目录，也不会阻止其他客户端读取这份共享配置。

EMP 把持久运行的 Codex App Server 视为外部所有者管理的共享后端。启用、恢复、
刷新或检查集成时，EMP 都不会停止、启动或重启 Codex。EMP 通过现有本地控制通道
读取 `model/list`，核对可见模型、显示名称和描述；目录未刷新时保持待确认状态，
连接或权限失败则显示对应错误。这条只读探测路径已在 Windows、Intel macOS
和 Linux 的官方 Codex 0.154.0 后台验证。检查成功不代表 Base URL 或其他启动配置
已经热加载。Linux 同时支持扫描当前 `CODEX_HOME/plugins/.plugin-appserver/codex`，
其他 AppImage 或发行版的安装布局仍需单独验证。

EMP 支持 Codex CLI `0.149.x` 至 `0.155.x`，推荐使用 `0.155.0`。Web UI 会显示
当前安装版本；更高版本会标记为“尚未验证”，更旧版本会标记为“不再支持”。

已在 runtime `0.153.4` 上验证 Gemini 3.7 Flash、3.8 Flash 的子任务委派、
后续任务和工具调用。协议说明见 [子任务兼容性](docs/external-collaboration.md)。

Subscription 的编辑窗口可以逐模型设置上下文 token 数。留空使用模型默认值；
“刷新模型上限”会用该账号的登录凭据拉取订阅目录，输入不能超过目录中的
最大上下文。0.95 的默认预留系数保持不变，例如设置 872,000 后可用约 828,400。
设置同时影响 Codex 模型列表和 EMP 的请求上下文检查，不会修改 API 地址或
目标 Codex 的当前登录。已有任务能否立即采用新窗口取决于客户端是否刷新了
目录；新建任务后应确认有效上下文。Native 导出后，其上下文设置随导入的账号
迁移，不覆盖目标机器 Native 的设置。

### 预构建安装包

从 [GitHub Releases](https://github.com/Killow1998/EasyMultiProvider/releases)
下载已经审核的构建。
[Package workflow](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/package.yml)
会在发布前原生构建并实际启动检查以下产物：

| 平台 | 产物 |
| --- | --- |
| Windows x64 | 带图标的独立 `.exe` |
| Ubuntu 22.04+ x64 | `.tar.gz` 和带桌面入口的 `.deb` |
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

- **Linux `.deb`：**运行 `sudo apt install ./EMP-linux-x86_64.deb`，再从应用菜单打开 **EMP**。`.deb` 不包含 `install-user.sh`。
- **macOS：**打开 DMG，把 **EMP** 拖入“应用程序”，然后双击。

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
Linux 用户安装与网页更新不需要 `sudo` 或管理员密码；安装系统级 `.deb` 需要。
配置与账号数据保存在上述用户配置目录，更新程序不会替换它们。
已有系统 `.deb` 安装不会被自动卸载；停止
旧 EMP 后可安装用户版本，旧配置应先备份再迁入用户配置目录。

需要命令行控制时仍可显式启动服务。下载 Windows 可执行文件后，在 PowerShell 中运行：

```powershell
.\EMP.exe --version
.\EMP.exe serve --config config.json
```

解压 Linux `.tar.gz` 或安装 `.deb` 后运行：

```bash
./EMP --version
./EMP serve --config config.json
```

`.deb` 会把同一命令安装到 `PATH` 中，安装后不需要输入前面的 `./`。Windows
可执行文件和 Linux 压缩包中的程序在无参数运行时，也会进入自动打开浏览器的
桌面模式。

当前 macOS workflow 产物属于未签名的开发构建。公开分发仍需要 Apple Developer
ID 签名和公证。

### 从源码安装

安装 Git 和 [`uv`](https://docs.astral.sh/uv/getting-started/installation/)，然后拉取
EMP：

```bash
git clone https://github.com/Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
uv sync
```

`uv` 会管理 Python、虚拟环境和锁定依赖，不需要额外的 Python 版本管理器。

## 快速开始

在 Linux 或 macOS 中显式启动打包后的 EMP：

```bash
easy-multi-provider serve --config config.json
```

在源码目录中运行时使用：

```bash
uv run python -m easy_multi_provider serve --config config.json
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

只有当前原生账号时，可以跳过导入账号和 Provider：在“当前 Codex 登录 → 编辑”
中隐藏模型，在“模型显示”中修改显示名称，保存后点击“将 EMP 应用于
Codex”。至少保留一个可见模型，显示名称不会改变模型 ID。

使用 ChatGPT 登录时，名称、隐藏状态和新增模型可在 Codex 运行期间自动更新。
EMP 会通过 Responses HTTP 和 WebSocket 通知模型目录版本变化，Codex 可在后续
请求时拉取更新。Codex 0.155.0 也会约每 4.5 分钟定期刷新；空闲 App 菜单不保证
保存后立即更新，刷新后可重新打开模型菜单查看。
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
