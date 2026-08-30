# EasyMultiProvider

[English](README.md) | 中文

EasyMultiProvider（EMP）是一个通过浏览器配置的 Codex 本地模型路由器。它在
保留 Codex 原生使用体验的同时，把多个 ChatGPT Subscription、API Provider
和外部模型加入同一个模型列表。

当前源码版本为 `v0.9.2`。

## 功能

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
- 通过密码保护的 `.emp` 文件导入和导出数据。
- 保留 Codex 原生会话、`resume`、WebSocket、压缩和 MCP 功能。
- 在当前登录、其他 Subscription 和外部模型之间切换时，使用 Codex 自己保存的
  可见历史继续已经压缩过的任务。
- 外部模型可以通过用户明确选择的 ChatGPT Subscription 使用 Codex 独立联网搜索，
  无需向外部 Provider 暴露凭据。

## 安装

EMP 需要系统已经安装 Codex CLI，并且可以通过 `codex` 命令调用。EMP 不会捆绑或
替代 Codex CLI。

EMP 支持 Codex CLI `0.149.x` 至 `0.151.x`，推荐使用 `0.151.x`。Web UI 会显示
当前安装版本；更高版本会标记为“尚未验证”，更旧版本会标记为“不再支持”。

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

- **Windows：**双击 `easy-multi-provider.exe`。
- **Linux：**安装 `.deb` 后，从应用菜单打开 **EasyMultiProvider**。
- **macOS：**打开 DMG，把 **EasyMultiProvider** 拖入“应用程序”，然后双击。

EMP 会自动打开已认证的 Web UI，并保留一个显示状态和日志的终端窗口。看到
`EasyMultiProvider listening on ...` 就表示启动成功。使用 EMP 时请保持该终端
开启；按 `Ctrl+C` 可以干净退出，也可以关闭终端结束进程。正常退出后会显示
`EasyMultiProvider stopped.`。

桌面启动会把配置保存到各系统标准的用户目录：

- Windows：`%LOCALAPPDATA%\EasyMultiProvider\config.json`
- macOS：`~/Library/Application Support/EasyMultiProvider/config.json`
- Linux：`$XDG_CONFIG_HOME/easy-multi-provider/config.json`，未设置时使用
  `~/.config/easy-multi-provider/config.json`

需要命令行控制时仍可显式启动服务。下载 Windows 可执行文件后，在 PowerShell 中运行：

```powershell
.\easy-multi-provider.exe --version
.\easy-multi-provider.exe serve --config config.json
```

解压 Linux `.tar.gz` 或安装 `.deb` 后运行：

```bash
./easy-multi-provider --version
./easy-multi-provider serve --config config.json
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

终端会输出一个一次性浏览器地址。打开后：

1. 导入 Codex Subscription 账户，或者添加 API Provider。
2. 拉取 Provider 模型并选择需要导入的模型。
3. 按需调整模型显示状态或上下文窗口。
4. 点击 **启用默认 Codex**。
5. 正常启动 Codex，通过 `/model` 或 App 模型菜单选择模型。

EMP 默认监听 `http://127.0.0.1:4200`。只有端口被占用时才需要使用
`--port` 修改端口。

每次启动还会输出 `Diagnostic log: ...`。EMP 会在该文件中保存结构化运行元数据，
后续遇到问题时无需再依赖用户复述整个过程。日志位于 `state/logs/`；总量超过
10 MiB 后会自动删除最老的分片。日志不会保存提示词、模型回复、工具参数或结果、
HTTP 正文、请求头、Cookie 和凭据。

## Web UI

- **账户**：导入 `auth.json` 或 `auth.json.bk1` 等备份文件。导入时只需要填写
  账户 ID，显示名称和模型前缀可以稍后修改。点击**刷新**会实时查询额度并保存新的
  快照。每个 Subscription 都能控制哪些 Coding Agent 模型显示在 Codex 中。
- **Provider**：选择支持的官方预设，或者通过 Base URL 和 API Key 添加自建
  Provider。
- **模型**：拉取上游模型，并进行导入、测试、编辑、隐藏或删除。模型按照
  Provider 分组显示。
- **Codex 集成**：把当前 EMP 模型目录应用到默认 Codex，也可以在同一页面恢复
  Codex 原生路由。

EMP 会自动检测启动环境或操作系统中的代理设置。

## 本地安全

EMP 默认只允许本机访问管理界面。Subscription 凭据和 Provider API Key 会在
本机加密保存，保存后不会重新返回浏览器。本地配置、加密状态和生成的模型目录
均已排除在 Git 提交之外。
