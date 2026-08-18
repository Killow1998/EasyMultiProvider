# EasyMultiProvider 多账户与多协议预案

## 落地判断

可以做，但先做“显式前缀路由”，不做自动账户池。模型前缀和外部 Provider 路由较简单；真正需要先验证的是多份 Codex OAuth 凭据的隔离、刷新和额度查询。

## 目标形态

```text
Codex /model
  └─ EasyMultiProvider /v1/responses
       ├─ ship/gpt-*       → Codex subscription: ship
       ├─ plus258/gpt-*    → Codex subscription: plus258
       ├─ gq/glm-*         → gq / OpenAI Chat Completions
       ├─ gq/qwen-*        → gq / OpenAI Chat Completions
       ├─ gem/gemini-*     → Gemini OpenAI-compatible Chat Completions
       └─ ant/claude-*     → Anthropic Messages
```

Codex 入口始终是 OpenAI Responses。上游第一版支持三种 wire format：

1. `responses`：原生透传。
2. `chat_completions`：Responses 与 OpenAI Chat Completions 双向转换。
3. `anthropic_messages`：Responses 与 Anthropic Messages 双向转换。

Gemini 第一版走 Google 官方 OpenAI-compatible Chat Completions，不新增原生 Gemini adapter；需要 Gemini 专属能力时再增加 `gemini_generate_content`。

## 配置模型

### Subscription Account

- `id`：内部稳定 ID。
- `prefix`：Codex 模型前缀，如 `ship`、`plus258`，必须唯一。
- `display_name`：用户自定义名称。
- `credential_path`：服务端私有加密路径，不通过 API 返回。
- `enabled`：是否生成模型并参与路由。

每个账户独占 `state/accounts/<id>/`，其中 `auth.json.enc` 是由主密钥加密的 `0600` 文件、目录为 `0700`；catalog 生成器把原生模型复制为 `<prefix>/<native-slug>`；Router 仅在使用时解密并去掉前缀后发给对应账户。

### API Provider

- `id`、`prefix`、`display_name`。
- `base_url`、`protocol`。
- `auth_type`：第一版只做 `bearer` 和 `anthropic_api_key`。
- API Key 单独存放在主密钥加密的私有 secrets 文件，不进入公开配置响应。
- 模型使用显式映射：`<prefix>/<display-model>` → `upstream_model`。

## Web 页面

### Codex Accounts

- 上传 `auth.json`，填写账户名称与模型前缀。
- 显示别名、脱敏账户信息、套餐、认证健康状态、最后刷新时间。
- 显示后端实际返回的 5 小时、周、月等 quota 窗口；缺失窗口显示“未提供”，不猜测。
- 显示后端实际返回的 credit balance、月度 credit limit、earned reset credits 和 spend-control 状态；缺失字段显示“未提供”，不承诺精确剩余金额。
- 支持“刷新单个 / 刷新全部 / 删除本地凭据”。

### API Providers

- 配置前缀、Base URL、协议、认证类型、API Key 和模型映射。
- API Provider 第一版不查余额，只显示连接测试和最近一次错误。

### Catalog Preview

- 预览最终 `/model` 列表。
- 检查重复前缀、重复模型 ID 和未知 Provider。
- 保存后重新生成 catalog，并明确提示 Codex 需要重启才能刷新模型列表。

## 凭据与 quota 实现边界

1. 上传文件只在 loopback Web 页面接收，限制大小并先解析到内存。
2. 为账户创建隔离的临时 `CODEX_HOME`，调用官方 `codex app-server` 的 `account/read` 验证凭据。
3. 验证成功后原子写入账户目录；失败不落盘。
4. quota 和可用 credit 字段通过该账户隔离环境中的 `account/rateLimits/read` 获取。
5. Token 过期时先调用 `account/read { refreshToken: true }`，再读取官方更新后的凭据；每账户加进程内锁，避免并发刷新覆盖。
6. 推理转发只替换 `Authorization` 和 `chatgpt-account-id`，其他允许的 Codex 请求头按白名单透传。
7. 遇到 401/403 时只刷新并重试一次；仍失败则标记“需要重新导入/登录”，不无限重试。

首个技术验证必须确认：临时 `CODEX_HOME` 中的 app-server 能刷新上传凭据并原子更新文件，而且两个账户并发时不会串用 Token。验证失败就改为自己实现 OAuth 生命周期，不能继续堆 Router 功能。

## 安全底线

- 凭据管理 API 仅允许本机同源请求，并校验 `Origin`；不开放 CORS。
- 第一版强制监听 `127.0.0.1`，不支持 LAN/公网部署。
- 所有凭据目录 `0700`、加密文件 `0600`；日志、异常、Web API 永不返回 Token、完整 account ID 或原始 `auth.json`。
- 主密钥只从 `EASY_MULTI_PROVIDER_MASTER_KEY` 读取；quota 刷新期间的明文 `auth.json` 只存在于自动删除的临时 `CODEX_HOME`。
- 删除只删除本地副本，不宣称会撤销远端 Token；UI 必须明确说明。
- 不在 `config.json`、catalog、测试 fixture 或 Git 中保存真实凭据。

## 分阶段实施

### Phase A：账户隔离 Spike

- 用两份脱敏/测试凭据目录验证 app-server 的 `account/read`、Token refresh、`account/rateLimits/read`。
- 用假上游断言 `ship/*` 与 `plus258/*` 发出了不同认证头。
- 通过后再进入正式开发。

### Phase B：账户 Vault 与 quota 页面

- 增加上传、验证、原子保存、删除和脱敏账户列表 API。
- Web 展示多账户 quota；不改推理路由。

### Phase C：Subscription 前缀路由

- 自动生成每个账户的原生模型别名。
- 按前缀选择账户并转发 Responses；保留现有固定响应 CLI 测试，再增加双账户隔离测试。

### Phase D：第三种协议

- 保留现有 Responses 与 Chat Completions。
- 新增 Anthropic Messages 的文本、工具调用、工具结果和 SSE 转换测试。
- Gemini 先配置到 OpenAI-compatible endpoint。

### Phase E：Web 收口与真实 CLI 验收

- 完成 Accounts、Providers、Catalog 三块页面。
- 用临时 Codex 配置验证 `/model` 中同时出现 subscription alias 与 API model。
- 真实执行各协议至少一个文本任务和一个 tool-call 任务。

## 验收标准

- Web 能导入至少两个账户，刷新结果互不串号，API/日志不泄漏凭据。
- `/model` 同时出现 `ship/gpt-*`、`plus258/gpt-*`、`gq/glm-*` 等配置项。
- 选择不同 subscription 前缀时，假上游或受控验证能证明认证账户不同。
- Responses、Chat Completions、Anthropic Messages 均通过流式文本和工具调用回归测试。
- 外部 API Provider 不显示伪造余额；没有 quota 数据就明确显示未知。

## 明确暂缓

- 不自动按余量轮换账户，不在 429 后静默切账户。
- 不跨账户保持或迁移 thread affinity。
- 不支持公网管理、多用户权限、凭据云同步或数据库。
- 不做原生 Gemini `generateContent` adapter。
- 不承诺 Codex 原生 `/status` 显示当前前缀账户：它仍显示 Codex 自己当前登录账户；多账户 quota 以 Web 页面为准。

## 止损规则

如果 Phase A 无法在隔离 `CODEX_HOME` 中可靠刷新凭据，或出现任意一次账户 A 请求携带账户 B Token，停止后续功能开发并重做认证边界。自动账户池只有在显式前缀路由稳定、thread/account 亲和机制有独立测试后才讨论。
