# ChatGPT 客户端接入 EMP 分阶段方案

状态：CLI Track A 待按审阅后的整夜方案实施；桌面端 Track B 等待用户人工验收。

详细施工、闭环、预算和启动指令见：

- [`lunamax-overnight-cli-plan.md`](lunamax-overnight-cli-plan.md)
- [`lunamax-overnight-handoff.md`](lunamax-overnight-handoff.md)

## 目标

让 ChatGPT 桌面客户端中的 Codex/Agent 使用 EasyMultiProvider 的模型路由，
而不是必须在终端执行：

```bash
codex --profile emp resume --all
```

第一目标是当前 Codex subscription 和 GLM 兼容链路；Gemini 暂时排除。同时保留
原生 ChatGPT 客户端和普通 Codex CLI 的可回退路径。

## 当前边界

EMP 目前提供的是本地 `Responses` 兼容服务，并通过 `emp.config.toml` 把
Codex CLI 指向该服务。ChatGPT 客户端是否读取同一份 Codex profile、是否允许
自定义 Provider/Base URL、以及是否向本地服务转发 Codex 登录态，需要在本机
版本上验证；不能直接假设它和 CLI 的配置加载路径相同。

因此，第一阶段不修改 ChatGPT 客户端的网络流量、不注入证书、不拦截账号请求，
也不复制或持久化 ChatGPT 登录凭据。

## 方案比较

| 方案 | 做法 | 风险 | 建议 |
|---|---|---:|---|
| 客户端原生配置 | 确认 ChatGPT 客户端的 Codex 实现是否支持同一 profile 或自定义 Provider，然后接入 EMP | 低 | 首选 |
| 本地兼容启动器 | 用一个本地启动器/包装层启动客户端或 Codex Agent，并把请求指向 EMP | 中 | 原生配置不可用时的 PoC |
| 系统代理/HTTPS 拦截 | 拦截官方客户端请求并改写到 EMP | 高 | 不建议，涉及 TLS、登录态、隐私和升级兼容性 |
| 独立本地客户端 | 保留 ChatGPT 作为账号/对话工具，另建一个使用 EMP 的 Agent UI | 中 | 适合作为长期可控替代方案 |

## 两条实施轨道

### Track A：Codex CLI，今晚可自动推进

先证明 `gpt-5.6-luna` 在 `--profile emp` 下通过唯一 forward Provider 使用原
Codex 登录 subscription，再验证 JSONL、工具、resume、重启、故障注入和 soak。

负责修复的 Luna Max 走原生 subscription；被测子 CLI 才走 EMP。这样 EMP 故障不会
切断修复进程。完整执行规格见整夜方案。

### Track B：ChatGPT 桌面端，必须用户介入

只在 Track A 通过后开始。用户需要启动/聚焦 App、确认是 Codex/Agent 入口并发出
固定测试消息。Agent 可以读取脱敏日志协助判断，但不得自行修改 App 登录态、系统
代理或证书。

## 桌面端推荐实施阶段

### Phase 0：确认客户端能力

1. 区分“ChatGPT 桌面端里的 Codex/Agent”与普通 ChatGPT 模型聊天入口。
2. 在 EMP 停止时确认客户端的正常路径不受影响。
3. 记录客户端版本、启动参数、配置目录和请求目标；只做读取，不修改账号文件。
4. 验证客户端是否读取 `CODEX_HOME`、`emp.config.toml` 或其他 Provider 配置。

如果客户端完全不读取自定义 Provider，立即停止原生接入路线，转入本地兼容启动器，
不做透明 HTTPS 拦截。

### Phase 1：先把 EMP 做成可诊断的本地网关

- `/v1/models` 只列出实际可路由模型；
- 启动时检查 profile、catalog、Provider 和模型路由的一致性；
- 对外部 Provider 显示 DNS、代理、认证、HTTP 状态和流式协议错误；
- Responses、Chat Completions 的普通响应和流式响应都必须返回明确完成或明确错误；
- 保持 ChatGPT subscription 的 forward auth 只使用调用方携带的登录态。

### Phase 2：最小 PoC

先接入已由 CLI Track A 证明可用的 subscription Luna，只做只读文本验收：

- 能列出模型；
- 能完成一次普通回答和一次流式回答；
- 上游错误不会变成空消息；
- EMP 停止后客户端可以恢复原生路径；
- 不保存客户端登录态，不影响普通 ChatGPT 会话。

### Phase 3：工具和会话

在文本链路稳定后，再验证工具调用、工具结果续接、取消、超时、重连和会话恢复。
任何不兼容的 `<think>`/`<tool_call>` 文本都必须被拒绝或隔离，不能作为真实工具调用
执行。

## 验收标准

1. ChatGPT 客户端无需手工执行 CLI resume 命令即可选择 EMP 模型。
2. `gpt-5.6-sol` 仍可通过当前 Codex subscription 使用。
3. subscription 和 GLM 兼容链路的流式响应有明确的 `response.completed` 或可读错误。
4. EMP 停止、升级或配置损坏时，原生 ChatGPT/CLI 路径仍可回退。
5. 不需要复制 API key、ChatGPT access token 或修改系统证书。

## 决策点

在实施前只需要确认一件事：你说的“ChatGPT 客户端”是 ChatGPT 桌面端里的
Codex/Agent 入口，还是普通 ChatGPT 对话窗口。前者可能存在可配置的 Codex
集成点；后者通常不是 EMP 这种本地 OpenAI-compatible Provider 可以直接接管的
入口，应该走独立本地客户端或官方允许的扩展能力。
