# Luna Max 整夜推进与 EMP CLI 闭环测试方案

状态：待用户审阅；审阅通过前不得执行

版本：2026-08-19-review-1

范围：EasyMultiProvider 的 Codex CLI 接入；暂不处理 Gemini；ChatGPT 桌面端只保留人工验收关卡

## 1. 结论

今晚的实施应采用“原生控制通道 + EMP 被测通道”，而不是让负责修复 EMP 的
Luna Max 也依赖正在被修改的 EMP：

```text
原生 Codex subscription
        |
        v
Luna Max 控制/修复进程  ----->  修改 EMP 代码、运行确定性验证器
        |                                  |
        | 启动有限、只读或临时目录内的子任务  | 启动/停止候选 EMP
        v                                  v
codex --profile emp  ---------------->  EMP SUT  ---> ChatGPT subscription
       被测 CLI                              被测服务       被测转发链路
```

这样可以完整验证“在 EMP Provider 下继续使用原 Codex 登录 subscription”，同时
避免 EMP 被重启、改坏或流中断时连修复者也一起掉线。

闭环的判定权属于确定性验证器，不属于模型。Luna 可以诊断和修改代码，但不能用
“我认为已经修好”代替测试结果。

## 2. 当前事实与待保护基线

实施者必须先重新核对，不能直接假设以下状态仍未变化：

- 仓库：当前 EasyMultiProvider checkout 根目录。
- Codex CLI：当前检测为 `0.148.0`。
- 基础 Codex 配置当前使用 `gpt-5.6-luna` 和 `max` reasoning。
- `emp` profile 当前指向 `http://127.0.0.1:4200/v1`，但默认模型仍是 Gemini；
  今晚所有 Luna 测试必须显式覆盖为 `gpt-5.6-luna` + `max`，不修改用户 profile。
- 当前 EMP 配置有且只有一个启用的 `forward` Provider：
  `chatgpt-subscription`。因此无前缀 `gpt-5.6-luna` 应由它接管，并转发调用方
  已有 Codex 登录态。
- 工作区已有未提交改动，包含 subscription forward、GLM/Chat Completions
  工具调用和流式错误处理等修复。它们是待验证基线，不得 reset、checkout、clean
  或被 HEAD 覆盖。
- 已有 `tests/test_codex_cli_demo.py` 可作为真实 Codex CLI + 本地假上游的基础，
  不应另造一套互相冲突的协议模拟器。

启动前必须把以下只读证据写入本次 artifact：

1. `git status --porcelain=v2`；
2. `git diff --binary`；
3. `git diff --check`；
4. `codex --version`、`codex exec --help` 和 `codex exec resume --help` 摘要；
5. 仅包含字段名和非秘密值的 EMP/profile 摘要。

禁止把 `config.json` 全文、`state/`、`~/.codex/auth.json`、Bearer token、API key、
ChatGPT account ID 的值写入 artifact。

## 3. 今晚目标与非目标

### 3.1 必须完成

1. 把 CLI 自动化做成可重复运行、可恢复、可审计的闭环。
2. 验证无前缀 `gpt-5.6-luna` 和此前报错的 `gpt-5.6-sol` 在 `--profile emp`
   下都通过唯一 forward Provider 使用原 Codex subscription，不再出现
   `unknown model` 404；整夜控制/修复模型仍固定为 Luna Max。
3. 验证普通回答、JSONL 终止事件、只读工具、临时工作区写入、会话 resume、EMP
   重启后的 resume。
4. 对 401、404、429、5xx、空流、半截流、非 SSE JSON、伪
   `<think>/<tool_call>` 文本做确定性故障注入，确保明确失败且不挂死。
5. 固化 GLM/通用 Chat Completions 适配器的编码和工具调用回归测试；直播 GLM
   请求只作可选 canary，不作为今晚核心完成条件。
6. 生成晨报、机器可读结果、完整复现命令和未解决问题列表。

### 3.2 今晚不做

- 不测试或修复 Gemini。
- 不操作 ChatGPT 桌面端 UI；到桌面端关卡即记录 `WAITING_FOR_USER`。
- 不修改系统代理、Clash、DNS、证书、hosts、系统钥匙串或 ChatGPT App 包内容。
- 不读取、复制、导出或改写 `~/.codex/auth.json`。
- 不安装新依赖；优先使用 Python 标准库和项目现有依赖。
- 不 commit、push、开 PR、发消息或做任何外部写入。
- 不使用 `--dangerously-bypass-approvals-and-sandbox`、`--yolo` 或
  `danger-full-access`。
- 不以无限重试、无限模型调用或无限运行时间换取“最终成功”。

## 4. 交付物设计

Luna 在审阅批准后可实现以下文件；文件名如需调整，必须在晨报说明原因：

| 路径 | 作用 |
|---|---|
| `tools/overnight_cli.py` | 确定性状态机、进程管理、超时、重试、artifact 和汇总 |
| `tests/test_codex_cli_demo.py` | 扩展现有真实 CLI + 假 Responses 上游测试 |
| `tests/test_cli_contract.py` | EMP profile、JSONL、resume、故障分类等 CLI 契约测试 |
| `tests/fixtures/` | 固定 Responses/Chat Completions 流及故障脚本；不得含凭据 |
| `docs/overnight-cli-runbook.md` | 实施后形成的实际运行说明和恢复方法 |
| `artifacts/overnight/<run-id>/` | 本地运行证据；加入 `.gitignore`，不进入提交 |

若现有测试文件足以清晰承载契约测试，可以不创建
`tests/test_cli_contract.py`；不得为了匹配表格而制造空文件。

每次运行的 artifact 至少包含：

```text
artifacts/overnight/<run-id>/
  baseline/
    git-status.txt
    working-tree.patch
    environment-redacted.json
  controller/
    events.jsonl
    stderr.log
    checkpoint.json
  cases/<case-id>/<attempt>/
    command-redacted.json
    stdout.jsonl
    stderr.log
    last-message.json
    verifier.json
  junit.xml
  result.json
  summary.md
```

所有文件原子写入；`checkpoint.json` 必须足以在进程中断后判断最后一个已完成阶段，
但不得包含认证信息。

## 5. 闭环架构

### 5.1 Bootstrap Luna

第一次 Luna Max 使用原生 Codex subscription 启动，不带 `--profile emp`。职责是：

1. 阅读本方案和 handoff；
2. 保存基线；
3. 实现或补齐监督器与测试；
4. 先运行 10 分钟 dry-run；
5. dry-run 通过后冻结监督器、测试清单和 oracle 的哈希；
6. 启动整夜状态机并等待其终态，不得仅启动后台进程后宣称完成。

### 5.2 确定性监督器

`tools/overnight_cli.py` 只能做机械决策：

- 启停 loopback 上的候选 EMP 和假上游；
- 运行固定 case manifest；
- 解析 Codex JSONL；
- 执行测试、文件哈希、diff、进程和日志检查；
- 根据退出码和 oracle 输出进入下一状态；
- 在允许次数内请求 Luna 进行一次有证据的修复；
- 达到终止条件时写报告并退出。

它不能自行修改产品代码，也不能把 Luna 的自然语言结论当作 PASS。

### 5.3 Luna 修复 worker

出现产品失败时，由监督器启动或 resume 原生 Luna Max worker。每个 prompt 只包含：

- 失败 case ID；
- 已脱敏的最小复现；
- 相关日志和预期；
- 允许修改的产品/测试文件；
- 禁止修改的监督器、manifest、schema 和已冻结 oracle 文件。

worker 修改后，必须由监督器重新运行最小失败集，再运行受影响的回归集。

### 5.4 EMP 被测 CLI

真正验证用户路径的子进程使用：

```bash
codex --profile emp exec \
  -m gpt-5.6-luna \
  -c 'model_reasoning_effort="max"' \
  --sandbox read-only \
  --json \
  --output-schema <schema> \
  -o <last-message> \
  -C <disposable-git-fixture> \
  <fixed-prompt>
```

候选 EMP 使用随机空闲 loopback 端口时，用一次性 `-c` 覆盖 profile 中 Provider 的
`base_url`；这仍然经过 `emp` profile，但不会争抢用户正在使用的 4200 端口。另设
一个单独的 `PROFILE-REAL-4200` canary，验证用户日常命令的真实路径。

需要 resume 的 case 不加 `--ephemeral`。监督器从 `thread.started.thread_id` 取 ID，
随后运行：

```bash
codex --profile emp exec resume <THREAD_ID> \
  -m gpt-5.6-luna \
  -c 'model_reasoning_effort="max"' \
  --json \
  --output-schema <schema> \
  -o <last-message> \
  <fixed-follow-up>
```

禁止依赖 `--last` 做自动化关联；并行或失败重试时它可能选错会话。只使用明确的
`thread_id`。

## 6. 状态机与推进顺序

```text
PREFLIGHT
  -> BASELINE
  -> HARNESS_BUILD
  -> UNIT
  -> MOCK_CLI
  -> LIVE_SUBSCRIPTION_CANARY
  -> RESUME_AND_RESTART
  -> FAULT_INJECTION
  -> SOAK
  -> REPORT
```

任何阶段失败后只能进入以下一个分支：

- `PRODUCT_FAIL -> DIAGNOSE -> PATCH -> 最小重测 -> 受影响回归`；
- `UPSTREAM_FAIL -> 有上限退避重试，不改产品代码`；
- `HARNESS_FAIL -> 仅在冻结前修复 harness；冻结后停止并报告`；
- `ENV_BLOCKED -> 继续其他不依赖该环境的 case，最终标记 PARTIAL`；
- `BUDGET_STOP -> 停止 live/model case，完成本地验证和报告`；
- `WAITING_FOR_USER -> 桌面端任务结束，不循环等待`。

同一根因连续三次、总修复轮次达到上限、或 oracle 文件被改动时，进入
`STOP_BLOCKED`，不得继续“换个 prompt 再试”。

## 7. 分阶段施工计划

### Phase 0：Preflight 与基线，预计 10–20 分钟

通过条件：

- 工作区和已有 dirty diff 已保存，且未被修改；
- `codex`、`uv`、Python 和 loopback 可用；
- `emp` profile 可解析，敏感文件未被读取；
- 4200 端口占用情况已记录，未知进程不会被 kill；
- `gpt-5.6-luna` 在生成 catalog 中存在且支持 `max`；
- 配置中恰有一个启用的 forward Provider，或明确失败并停止 live canary；
- Gemini case 数为 0。

### Phase 1：复建当前回归基线，预计 20–40 分钟

按从小到大顺序：

1. router/config/catalog 的定向单元测试；
2. server 和 migration 测试；
3. 全套 `unittest discover`；
4. `git diff --check`。

测试失败必须区分产品失败与 sandbox/loopback 环境失败。不能因全套测试中的环境性
bind/path 问题而掩盖定向产品测试结果。

### Phase 2：构建确定性 harness，预计 45–90 分钟

要求：

- Python 标准库实现；
- 所有子进程使用参数数组，不用拼接 shell 字符串；
- 每个子进程独立 process group，有软终止和硬终止；
- 每个 case 有 timeout、重试上限和唯一目录；
- loopback 端口由 OS 分配；
- stdout/stderr 分离保存；
- 任何命令写 artifact 前先脱敏；
- 运行结束检查并清理自己启动的进程，绝不按模糊名称 `pkill`；
- 支持 `--dry-run`、`--phase`、`--resume <run-id>`、`--max-hours`、
  `--live-canaries` 和 `--seed`；
- case 顺序随机化只来自保存的固定 seed，可复现。

冻结前必须用纯假上游跑一次 10 分钟 dry-run。冻结后记录 harness、manifest、schema
和 oracle 文件 SHA-256；修复 worker 改动任一文件即停止。

### Phase 3：Mock CLI 契约，预计 30–60 分钟

这里使用真实 `codex exec`，但模型响应由固定本地 Responses/Chat Completions 假上游
提供，所以不消耗外部模型配额。目标是证明 Codex CLI、EMP 和协议事件之间的契约。

必须覆盖普通文本、结构化 function call、tool output 续接、非 SSE JSON、空流、
半截流、重复/乱序片段、HTTP 错误和 UTF-8 中文文本。

### Phase 4：Live subscription canary，预计 20–40 分钟

这是今晚唯一必须访问真实 ChatGPT subscription 的被测阶段。顺序固定：

1. 原生控制通道返回固定 nonce，证明 Luna 控制面健康；
2. `--profile emp` + 无前缀 `gpt-5.6-luna` 返回结构化 nonce；
3. 用最短只读 prompt 对此前报错的无前缀 `gpt-5.6-sol` 做一次 canary；
4. 只读工具在临时 Git fixture 中读取指定文件；
5. workspace-write 工具只修改 fixture 中一个指定文件；
6. 检查 EMP 日志只有脱敏路由证据，不能出现 token 值；
7. 确认 Luna/Sol 均没有 `unknown model`、空输出或缺失终止事件。

Live canary 不允许修改真实仓库。第二步失败时先判断 401/403/404/429/5xx：

- 401/403：认证转发或登录态问题；停止 live 重试，保留本地测试；
- 404 unknown model：路由/catalog 产品失败，允许修复；
- 429：预算/限流停止，不改路由代码；
- 5xx/断流：按 upstream 分类，只重试一次。

### Phase 5：Resume、重启和取消，预计 40–80 分钟

使用不可预测 nonce 证明上下文连续性，不能只检查“命令退出 0”：

1. 第一 turn 写入 nonce A，输出 thread ID；
2. 明确 ID resume，要求同时返回 A 和新 nonce B；
3. 在没有活动请求时停止并重启候选 EMP；
4. 再次明确 ID resume，要求返回 A、B 和 C；
5. 启动一个可控慢流，取消客户端；确认 EMP 关闭上游、无孤儿请求；
6. 新会话仍可成功，取消不会污染后续会话。

### Phase 6：故障注入与 GLM 适配器回归，预计 40–80 分钟

必须在本地假上游完成：

- 结构化 Chat Completions `tool_calls` 被转换为 Responses function call；
- 下一 turn 的 function call/output 历史被正确还原为 assistant/tool messages；
- `tool_call_mode=disabled` 时不把工具定义发给上游；
- 普通中文、emoji、分片 UTF-8 不乱码；
- 文本 `<think>`、`<tool_call>`、`<|tool_calls|>` 不被显示或执行，而是明确 502；
- `stream=true` 但上游返回普通 JSON 时仍完成；
- `[DONE]` 前没有内容时明确报空响应；
- JSON error body 被保留为可读上游错误；
- 断流没有 `response.completed`，并由 CLI 标记失败而非静默成功。

如果本地回归全部通过，可对 `chen/glm` 做最多一次纯文本和一次受控工具 canary；
失败只记录为可选上游结果，不得阻断 subscription 主线，也不得切换到 Gemini。

### Phase 7：Soak，目标 4–6 小时，总运行上限 8 小时

Soak 不是连续调用真实模型：

- 本地 mock/故障矩阵按固定 seed 循环；
- 每 5 分钟写 heartbeat；
- 每 30 分钟记录 EMP RSS、文件描述符、子进程数和 artifact 大小；
- live subscription canary 最多每 60 分钟一次，整夜总数不超过配置上限；
- 每次 live canary 使用新 fixture，结束后检查无残留进程和端口；
- 首次发现产品失败即退出 soak，进入一次修复闭环，修复后从失败 seed 复测，
  通过后再继续剩余时长；
- 监督器自身异常、冻结文件变化或 artifact 超限立即停止。

### Phase 8：晨报

无论 PASS、PARTIAL、BLOCKED 或 BUDGET_STOP，都必须产出 `summary.md` 和
`result.json`，不能因为失败而没有报告。

## 8. 固定测试矩阵

| ID | 路径 | Oracle / 通过条件 |
|---|---|---|
| PRE-01 | 版本/profile | CLI 版本可识别，profile 可解析，无秘密输出 |
| PRE-02 | catalog/route | Luna 在 catalog 中；唯一 forward 能接管无前缀模型 |
| UNIT-01 | forward headers | 只转发允许的 Codex auth/account/originator 头，值不落盘 |
| UNIT-02 | unknown model | 真无效模型返回结构化 404；有效 Luna 不返回 404 |
| UNIT-03 | GLM tools | 结构化工具调用和历史双向转换一致 |
| UNIT-04 | pseudo markup | 伪工具/思考标签明确拒绝，绝不执行 |
| UNIT-05 | stream variants | SSE、非 SSE JSON、空流、断流均有唯一终态 |
| MOCK-01 | CLI text | 退出 0、JSONL 合法、固定 final sentinel |
| MOCK-02 | CLI function call | function call、output、第二 turn 均完整 |
| MOCK-03 | error matrix | 错误分类和退出状态匹配 manifest，无 hang |
| LIVE-01 | EMP Luna text | `--profile emp` 下无前缀 Luna 返回 nonce |
| LIVE-01B | EMP Sol route | 此前 404 的无前缀 Sol 完成一次最短 canary |
| LIVE-02 | read-only tool | 精确读取 fixture，仓库零 diff |
| LIVE-03 | write tool | 只改 fixture 指定文件，内容 hash 精确 |
| LIVE-04 | resume | 明确 thread ID，A/B nonce 连续 |
| LIVE-05 | EMP restart | 重启后同 ID 恢复并返回 A/B/C |
| LIVE-06 | cancel | 取消无孤儿；下一会话成功 |
| GLM-01 | 本地编码/工具回归 | Unicode 无损、伪标签隔离、结构化工具可续接 |
| GLM-02 | 可选 live canary | 最多两次；结果与主线分开报告 |
| SOAK-01 | 本地循环 | 所有 seed 通过，无 crash、hang、资源持续增长 |
| SEC-01 | artifact | 无 token/API key/account ID 值，无 auth/config 原文 |
| CLEAN-01 | 变更范围 | 无 reset/clean、无外部写入、无 forbidden path 变更 |

## 9. JSONL 和结果判定

一个 Codex turn 只有同时满足以下条件才是 PASS：

1. 进程在 timeout 内退出 0；
2. stdout 每个非空行都是合法 JSON；
3. 恰有一个 `thread.started`；
4. 恰有一个终态，且为 `turn.completed`；
5. 不存在 `turn.failed` 或 `error`；
6. `-o` 文件存在且符合 JSON Schema；
7. nonce/sentinel 和文件 hash 满足独立 oracle；
8. EMP 仍健康，或该 case 明确要求 EMP 停止；
9. 没有 forbidden diff、秘密泄漏或孤儿进程。

仅看到自然语言回答、HTTP 200 或 `response.completed` 中任意一个，都不足以单独判定
成功。

统一结果枚举：

- `PASS`：所有必需 case 和 soak 门槛通过；
- `PARTIAL`：核心本地与 subscription 主线通过，但有环境性/可选项未完成；
- `BLOCKED`：同一产品/环境阻塞达到终止条件；
- `BUDGET_STOP`：429、订阅限制或配置的 live/model 预算触发；
- `WAITING_FOR_USER`：仅供桌面端 Track B 使用。

## 10. 预算、超时和熔断默认值

审阅时可改；未改则使用：

| 项目 | 默认值 |
|---|---:|
| 总墙钟时间 | 8 小时 |
| soak 目标 | 4–6 小时 |
| 单个本地 case timeout | 120 秒 |
| 单个 Luna/真实 CLI turn timeout | 15 分钟 |
| 同一 upstream/infra case 重试 | 1 次 |
| 同一产品根因修复轮次 | 2 次 |
| 全程 Luna 修复 worker turn | 最多 12 次 |
| live EMP canary | 最多 8 次，至少间隔 60 分钟 |
| 可选 live GLM | 最多 2 次 |
| 连续同根因失败熔断 | 3 次 |
| artifact 上限 | 1 GiB |
| 无 heartbeat 熔断 | 10 分钟 |

出现 429、订阅余额/周额度告警或 auth 失效时，立即停止新的 live/model 调用；本地
mock 验证和报告仍继续。不得通过切换账号、复制 token 或自动购买额度绕过。

## 11. 安全与变更边界

### 允许

- 读取仓库、文档、已脱敏配置元数据；
- 修改 EMP 源码、测试、README 和本方案列出的本地 harness/runbook；
- 在 workspace 和系统临时目录创建 disposable fixture；
- 启停监督器自己创建的 loopback 进程；
- 运行项目已有测试和受限 `codex exec` canary。

### 禁止

- `git reset --hard`、`git checkout --`、`git clean`、删除用户文件；
- 修改或提交 auth、API key、`state/`、真实 `config.json` 内容；
- kill 未由监督器记录 PID/启动时间/命令的进程；
- 修改 4200 上未知现存服务；
- 修改系统网络、Clash 7897、证书、hosts 或 ChatGPT App；
- commit、push、PR、发布、上传 artifact；
- 在测试 prompt 中授权被测子 Codex 修改真实仓库。

若 4200 已被占用，先探测 `/healthz` 和进程身份；身份不能确定时只使用随机端口，
`PROFILE-REAL-4200` 标记为环境阻塞，不能 kill 抢端口。

## 12. 防止测试“修 oracle”

dry-run 后冻结：

- case manifest；
- JSON Schema；
- sentinel/nonce verifier；
- artifact 脱敏器；
- supervisor 的状态转换和预算配置；
- 关键 mock upstream fixture。

之后 repair worker 只能改产品代码和明确允许的产品测试。若它修改冻结文件，监督器
不自动回滚，也不接受新结果，而是停止并在晨报列出 diff。这避免模型通过降低断言、
删测试或扩大重试来制造 PASS。

## 13. 晨报格式

`summary.md` 必须按以下顺序：

1. 最终状态和一句话结论；
2. 是否证明 `gpt-5.6-luna` 与 `gpt-5.6-sol` 经 EMP 使用原 subscription；
3. 修改文件和每项修改理由；
4. 必需测试矩阵结果；
5. soak 时长、循环数、live canary 数、重试数；
6. 首次失败、根因、修复和复测证据；
7. subscription/GLM/环境问题分栏；
8. 安全检查与秘密扫描结果；
9. 未解决问题和最小复现；
10. ChatGPT 桌面端下一步人工清单；
11. 精确复现命令；
12. `git status` 和最终 diff 摘要。

报告不得写“全部通过”而省略 case 级证据。

## 14. ChatGPT 桌面端 Track B（今晚不执行）

CLI Track A 完成后才进入桌面端。官方文档目前没有给出“ChatGPT 普通聊天窗口加载
Codex 自定义 Provider/profile”的稳定承诺，因此必须在本机版本做人工能力探测。

需要用户介入的关卡：

1. 用户确认目标是 ChatGPT 桌面端里的 Codex/Agent 入口，而非普通模型聊天；
2. EMP 停止时完成一轮原生基线对话；
3. 用户启动/聚焦 App 并进入 Codex 入口；
4. 只读确认 App 是否加载 `CODEX_HOME`/profile/custom Provider；
5. 若能选择 EMP，用户手动发一条固定 nonce，再由日志验证路由；
6. 用户停止 EMP，确认 App 原生路径可回退。

如果 App 不读取 custom Provider，停止原生接入路线，评估“受支持的本地启动器”或
“独立 EMP Agent UI”。不进入透明 HTTPS MITM，不注入证书，不接管普通 ChatGPT
会话。

## 15. 审阅通过后的启动方式

首次 bootstrap 必须走原生控制通道，不能带 `--profile emp`：

```bash
cd /path/to/EasyMultiProvider
export EMP_RUN_ID=20260819-night1
mkdir -p "artifacts/overnight/$EMP_RUN_ID"

/usr/bin/caffeinate -dimsu codex exec \
  -C /path/to/EasyMultiProvider \
  -m gpt-5.6-luna \
  -c 'model_reasoning_effort="max"' \
  --sandbox workspace-write \
  --approve-for-me \
  --json \
  -o "artifacts/overnight/$EMP_RUN_ID/bootstrap-final.md" \
  - < docs/lunamax-overnight-handoff.md \
  > "artifacts/overnight/$EMP_RUN_ID/bootstrap-events.jsonl" \
  2> "artifacts/overnight/$EMP_RUN_ID/bootstrap-stderr.log"
```

`caffeinate` 只负责防止 Mac 在前台进程运行时休眠。终端保持打开并接通电源。若命令
提前退出，不用 `--last` 猜会话；从 JSONL 的 `thread.started.thread_id` 恢复。

`--approve-for-me` 使用当前 CLI 的自动安全审查，并没有绕过 workspace-write
sandbox；它用于避免用户睡眠期间卡在可自动判断的本地审批。任何需要扩大到系统、
认证、网络设置或外部写入的动作仍应停止。

## 16. 用户审阅清单

批准今晚执行前，请确认：

- [ ] 同意原生 subscription 作为控制通道，EMP 只作为被测通道；
- [ ] 同意 Luna 修改仓库内 CLI/router/test/docs，但不 commit/push；
- [ ] 同意最多 8 小时、最多 12 个修复 turn、最多 8 个 live EMP canary；
- [ ] 同意 Gemini 完全排除；
- [ ] 同意 GLM 本地回归必做、live GLM 最多两次且可选；
- [ ] 同意不修改 `~/.codex/emp.config.toml`、auth、系统代理和 ChatGPT App；
- [ ] 同意桌面端到 `WAITING_FOR_USER` 即停止；
- [ ] 同意 PASS 必须由固定 oracle 判定，Luna 不能自行降低验收标准。

## 17. 设计依据

- [Codex 非交互模式](https://learn.chatgpt.com/docs/non-interactive-mode)：`codex exec`、
  JSONL、结构化输出、明确 session ID resume 和最小 sandbox。
- [Codex CLI 命令参考](https://learn.chatgpt.com/docs/developer-commands?surface=cli)：
  `--profile`、`--json`、`--output-schema`、`--output-last-message` 和 resume 语义。
- [GPT-5.6 Luna 模型页](https://developers.openai.com/api/docs/models/gpt-5.6-luna)：
  Luna 的 Responses、streaming、function calling 与 `max` reasoning 支持。
- [GPT-5.6 模型指导](https://developers.openai.com/api/docs/guides/latest-model)：
  应显式设置 reasoning effort，并为主动多步任务定义授权边界和成功标准。

命令还按本机 `codex-cli 0.148.0` 的 `--help` 做过只读核对；若今晚 CLI 版本变化，
Preflight 必须重新确认后再运行，不能盲用本页命令。
