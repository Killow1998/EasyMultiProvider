# aero 实验版

[English](README.md)

EMP **999.14.2-exp** 在刷新额度与聚合路由之间增加带图标的 **上令下效**。选择带来源前缀的统筹模型与执行模型，保存后直接在 Codex 选择 `aero/配对名称`。

统筹模型负责规划、只读检查和复核；执行模型使用 Codex 工具落实计划。EMP 负责连接、认证与计量，角色交接由独立 aero 进程管理，首次调用时启动，随 EMP 退出。

```text
Codex → EMP 网关 → aero → EMP 网关 → 对应角色的模型
          配置配对          路由、认证与计量
```

## 从界面开始

1. 在 [预发布下载](https://github.com/Killow1998/EasyMultiProvider/releases) 中下载完整系统压缩包或 macOS DMG。实验标签以 `aero-999.14.2-exp-` 开头，下载文件持续保留；CI 产物只用于构建中转，发布后清理。本实验版关闭更新检查与自动安装。
2. 完整解压，保留 EMP 旁的 `aero` 文件夹；macOS DMG 已将它放进应用包。无需安装 Python。先退出当前 EMP，再运行实验版。
3. 点击 **上令下效 → 添加配对**。左卡片选择统筹模型，右卡片选择执行模型；选项区分 Native、其他订阅、外部服务和聚合路由，每个角色可分别设置推理强度。可以保存多组配对进行比较。
4. 在 Codex 会话中选择 `aero/配对名称`。已运行的客户端若尚未重新加载模型目录，重新打开模型选择器，或重启该客户端。

配对保存在 EMP 配置的 `aero_profiles` 中。独立运行程序的生成配置和 SQLite 对话状态保存在该配置旁的私有 `aero` 目录。网关设备 Key 的归属继续传到实际模型调用，外层转发不重复计量。角色、交接、错误与用量日志轮转总上限为 100 MB。

## 对话、工具与媒体

- **加密压缩**：调用 Codex 压缩后继续原会话，保留当前计划、角色和待复核状态。两种角色共享可读摘要；Native 加密检查点通过 EMP 既有历史读取链路恢复。
- **Code Mode**：保留 Codex 的 Code Mode 与延迟工具发现。统筹阶段的读文件工具转换为 Code Mode 调用，执行阶段直接使用 Codex 提供的工具。
- **生图**：使用 EMP 既有图片路由，支持原生生图工具和 Free 账号的轻量 MCP，包含生成、编辑和图片落盘。
- **音频**：保留音频输入与输出，Voice 的 multipart、SDP 和实时 WebSocket 连接经 EMP 转发。
- **实时文字**：模型生成文字时立即返回，跨角色交接连续显示；工具调用经当前阶段检查后交给 Codex 执行。

## 独立开发入口

原有 CLI 入口仍可使用，需要 Python 3.10+、Codex CLI 和 ripgrep：

```sh
cp profiles.json profiles.local.json
python3 -B codex.py --config profiles.local.json --model aero/astra-sol --cwd /你的项目路径
```

在私有配置中填写 EMP 的 `upstream` 地址和可用的 `planner.model`、`executor.model`；通过 `EMP_GATEWAY_KEY` 提供网关 Key，或用 `key_file` 指向私有纯文本文件，相对路径以配置目录为准。启动器仅为该 Codex 进程设置 Provider 和目录，不读取或替换 `auth.json`。状态默认位于 `$XDG_STATE_HOME/aero`，否则为 `~/.local/state/aero`，可通过 `--runtime` 修改。目录模板默认读取 `$CODEX_HOME/models_cache.json` 或 `~/.codex/models_cache.json`。

API 单独测试可运行 `python3 -B gateway.py --config profiles.local.json`，默认监听 `http://127.0.0.1:4219/v1/responses`；使用同一个网关 Key 和稳定的 `session-id` 请求头，`--port` 可修改端口。界面使用时不需要这条命令，也不需要再增加一个服务。

## 一个任务怎样推进

- 规划模型可以直接回答问题；需要实施时，先通过 Codex 读文件，再给出计划并交接。
- 执行模型使用 Codex 的工具完成工作、验证结果，遇到需要重新规划的情况则交回。
  规划模型检查实际证据，再决定结束或继续。
- 每 30 次执行模型调用，以及两次请求之间收到新用户消息时，会要求交回复核。
  「同一验证第二次失败即交回」由执行指令约束，需要模型遵从；网关负责限制各阶段
  可调用的工具。
- 各角色的推理强度由配置指定，这些虚拟模型没有 Codex effort 选择器。没有任务
  总时长或交接次数上限。

## 验收方式

验收使用 Codex 0.169.0、隔离的 Codex Home 和合成上游，覆盖直接工具与 Code Mode 的读写校验、延迟 MCP 生图、加密检查点继续、图片／音频往返、首段文字实时送达、鉴权、重放、断连取消和退出。Windows、Linux、Intel Mac 与 Apple Silicon Mac 的解压安装包另外验证内置 aero，无需系统 Python。

Codex 工具执行与审批沿用客户端；Responses 使用 HTTP/SSE，媒体的实时侧信道使用 WebSocket。计量查看 EMP 的逐次调用记录，可按统筹／执行模型和设备归属查询。

## 复现验收

```bash
python3 -B verify.py
python3 -B verify_lifecycle.py
python3 -B verify_features.py
python3 -B verify.py --native-codex
python3 -m pip install ruff==0.17.5
python3 -m ruff check --no-cache .
python3 -m ruff format --check .
```

结果写入忽略提交的 `evidence/`。原生 Codex 验证使用 `fixtures/` 内的合成目录，
不读取私人登录或模型缓存。反馈请记录任务验收结果、人工介入、token／缓存用量和耗时，
并去除私人代码及 Key。
