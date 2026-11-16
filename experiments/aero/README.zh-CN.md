# aero 实验版

aero 是一个可选的 Codex CLI 启动器，让两个模型在同一个任务中分工规划、执行和复核。
它独立运行，通过 EMP 现有网关调用模型。

```text
Codex CLI → aero → EMP → 规划模型／执行模型
```

Codex 负责文件、命令工具与审批；aero 负责交接与复核；EMP 负责账号、鉴权、转发和
用量统计。实验分支交付这个独立工具，搭配现有正式版 EMP 使用。

## 开始测试

需要 Python 3.10+、Codex CLI、ripgrep，以及运行中的 EMP。目前已验收 Linux 和
Codex 0.169.0 的协议及原生工具往返；其他平台尚未验收。本轮入口是文本任务与直接
文件／命令工具。

可以检出 `dev/aero-experiment` 后进入本目录，或在成功的
[实验 CI](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/aero-experiment.yml) 中下载 `aero-experiment`
产物，解开下载文件后，再解开其中的 `aero-experiment.zip`。CI 产物保留一天，源码分支
持续保留。

1. 在 EMP 的 **设置 → 通用 → 网关连接** 中为 aero 创建独立的设备 Key，便于在统计
   中识别调用。
2. 将 `profiles.json` 复制为 `profiles.local.json`。`upstream` 填 EMP API 地址；
   `planner.model` 与 `executor.model` 填自己 EMP 中可用的模型名称。
   默认 `aero/astra-sol` 由 Astra 规划和复核、Sol 执行。`aero/sol-dsk` 是另一个示例，
   其中 `deepseek/deepseek-v4-pro` 需要相应服务与模型，也可改成自己的模型。
3. 通过环境变量 `EMP_GATEWAY_KEY` 提供网关 Key，或把 Key 存入私有的纯文本文件，
   在本地配置增加 `"key_file": "caller.key"`。相对路径以该配置文件所在目录为准，
   环境变量优先。aero 不读取或替换 Codex 的 `auth.json`。
4. 在本目录启动一个新会话：

```bash
python3 -B codex.py --config profiles.local.json --model aero/astra-sol --cwd /你的项目路径
```

启动器临时监听本机随机端口，只为这个 Codex 进程设置 Provider 和模型目录；Codex
退出后关闭 aero。现有 Codex 全局配置和 EMP 模型目录保持原样。本轮使用这个 CLI
入口，aero 模型不进入 ChatGPT App 的全局选择器。

模型元数据模板默认读取 `$CODEX_HOME/models_cache.json`，未设置时读取
`~/.codex/models_cache.json`。正常启动一次 Codex 可生成缓存，也可以通过
`--catalog-template /路径/models.json` 指定 Codex 模型目录。

运行状态默认保存在 `$XDG_STATE_HOME/aero`，未设置时为
`~/.local/state/aero`；可用 `--runtime /私有目录` 修改。`sessions.sqlite3` 保存
对话与交接历史；日志记录角色、模型、用量、耗时和错误，轮转总上限为 100 MB。请使用
私有目录。底层模型调用携带稳定的会话标识和 `X-EMP-Client: aero`，供 EMP 归属统计。

也可独立执行 `python3 -B gateway.py --config profiles.local.json`，在
`http://127.0.0.1:4219/v1/responses` 为 API 客户端提供实验服务；使用同一个网关 Key
鉴权，并提供稳定的 `session_id` 请求头。`--port` 可修改端口。它是另一种测试入口，
无需把 aero 再添加成 EMP 的上游服务。

## 一个任务怎样推进

- 规划模型可以直接回答问题；需要实施时，先通过 Codex 读文件，再给出计划并交接。
- 执行模型使用 Codex 的工具完成工作、验证结果，遇到需要重新规划的情况则交回。
  规划模型检查实际证据，再决定结束或继续。
- 每 30 次执行模型调用，以及两次请求之间收到新用户消息时，会要求交回复核。
  「同一验证第二次失败即交回」由执行指令约束，需要模型遵从；网关负责限制各阶段
  可调用的工具。
- 各角色的推理强度由配置指定，这些虚拟模型没有 Codex effort 选择器。没有任务
  总时长或交接次数上限。

## 已验证与尚未覆盖

CI 使用合成后端和隔离的 Codex Home，验证角色交接、原生工具读写与校验、鉴权、重试
重放、持久化、断连取消和退出。CI 不调用付费模型。这证明协议行为，不能代替真实任务
的能力、速度和费用对照。

当前限制：

- 仅接收完整、可见的 HTTP Responses 历史。WebSocket、`previous_response_id`、
  item reference 和加密 compaction 会明确失败。历史前缀被改写时也会拒绝，保留原交接
  历史，避免静默丢失状态。
- 长会话压缩与真实推理中的即时插话尚未验收，本轮适合新建、有明确范围的项目会话。
- 使用文本和直接工具；Code Mode、内置搜索、生图、音频、嵌套子代理不属于本版。
- 内容在模型／交接片段完成后返回；SSE 心跳只保活，尚未实时转发中间文本。
- 汇总计数只相加已报告的用量，部分角色未报告时不构成完整总量；计量以 EMP 的逐次
  调用记录为准。

## 复现验收

```bash
python3 -B verify.py
python3 -B verify_lifecycle.py
python3 -B verify.py --native-codex
python3 -m pip install ruff==0.17.5
python3 -m ruff check --no-cache .
python3 -m ruff format --check .
```

结果写入忽略提交的 `evidence/`。原生 Codex 验证使用 `fixtures/` 内的合成目录，
不读取私人登录或模型缓存。反馈请记录任务验收结果、人工介入、token／缓存用量和耗时，
并去除私人代码及 Key。
