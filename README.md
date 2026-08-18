# EasyMultiProvider

通过浏览器配置本地 Provider 和模型路由，让 Codex 的 `/model` 同时看到原生订阅模型和外部 API 模型。

## 工作方式

```text
Codex /model
    ↓ model_catalog_json
Codex → http://127.0.0.1:4200/v1
          ├─ ship/gpt-*            → 选择 ship 的 Codex subscription
          ├─ plus258/gpt-*         → 选择 plus258 的 Codex subscription
          ├─ gq/glm-*              → OpenAI-compatible Chat Completions
          └─ ant/claude-*          → Anthropic Messages
```

Web 页面只管理本地 Router，不控制桌面端，也不修改 Codex Desktop。

## 环境与启动

项目用 `mise` 管理 Python/uv 版本，用 `uv` 管理虚拟环境和依赖。当前 `mise.toml` 选择 Python 3.11 与 uv 0.11.1。

```bash
cd /home/nuc/NA2H/EasyMultiProvider
mise install
uv sync
```

加密凭据必须使用一个长期保存的 Fernet 主密钥。只在 shell 的 secret manager 或本机环境中保存，不要写入 `mise.toml`、`config.json` 或 Git：

```bash
export EASY_MULTI_PROVIDER_MASTER_KEY="$(uv run python -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())')"
uv run python -m easy_multi_provider --config config.json
```

上面的生成命令只执行一次；以后启动时重新导出同一个主密钥。

打开 <http://127.0.0.1:4200>，在页面中添加 Provider 和模型。

Web 页面不修改监听端口；默认端口是 `4200`。如需避开端口冲突，在启动时使用 `--port 4300`（或修改启动配置）后，重新打开 <http://127.0.0.1:4300>。

可以从示例复制配置：

```bash
cp config.example.json config.json
```

`config.json` 和生成的 catalog 会使用私有文件权限；不要提交它们，也不要把 API Key 写进日志。
Web 导入的 Codex `auth.json` 会先在内存解析，再写入 `state/accounts/<id>/auth.json.enc`；API Key 写入 `state/secrets/*.enc`，由主密钥加密，`config.json` 只保存非敏感元数据和文件引用。运行 quota 刷新时，程序才会在操作系统临时目录创建 `0600` 的短生命周期 `auth.json` 供 `codex app-server` 使用，完成后自动删除。缺少主密钥时凭据读写会失败关闭。`.gitignore` 已忽略 `state/`、`config.json`、生成目录和密钥文件。

目标环境包括 Windows 10/11、macOS Intel 和 Ubuntu 20/22/24。Unix 使用 `0700/0600` 文件权限；Windows 的权限模型由用户目录 ACL 负责，因此不要把项目目录放在共享目录中。

## Codex 接入

在 Web 页面点击“生成合并 catalog”，然后把页面显示的配置片段加入 `~/.codex/config.toml`：

```toml
model_provider = "easy-multi-provider"
model_catalog_json = "/absolute/path/to/EasyMultiProvider/generated/codex-models.json"

[model_providers.easy-multi-provider]
name = "EasyMultiProvider"
base_url = "http://127.0.0.1:4200/v1"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
```

重启 Codex 后，`/model` 会使用合并后的目录。专用 provider 禁用 EasyMultiProvider 尚未实现的 Responses WebSocket，并避免内置 OpenAI provider 对 ChatGPT 请求启用 zstd body 压缩。原生模型会匹配唯一的 `auth_mode = "forward"` Provider；外部模型通过显式的 `provider/model` ID 匹配配置中的模型。

导入账户后，catalog 会把原生模型复制为账户前缀，例如 `ship/gpt-5`、`plus258/gpt-5`。Router 只读取选中账户私有目录中的 Token，并覆写对应的 `Authorization` 与 `chatgpt-account-id`；不会把上传账户 Token 与当前 Codex 请求的认证头混用。

Web 导入账户时只需填写账户 ID；显示名称和模型前缀留空会默认使用账户 ID，之后可在账户列表点击“编辑”修改。

账户导入后会用 `tokens.account_id`（没有时再用 access token 的内存值）与当前 `CODEX_HOME/auth.json` 比对。重复账户仍保留为加密副本，但页面会标记“已过滤”，不会为它生成重复的 subscription 模型别名；两个导入副本互相重复时也只保留第一个别名。

Provider 保存后可在列表点击“拉取模型”：通用 API Key Provider 请求 `GET /models`，Gemini AI Studio 自动请求原生 `GET /models` 并分页读取可生成内容的模型，同时带入可返回的上下文上限和已知 reasoning levels。模型会自动加入并刷新 catalog；在模型列表取消“显示”只会把它标记为 disabled，不删除配置，且不会出现在 Codex 的 `/model` 列表中。Anthropic Messages 没有统一的模型列表接口，仍需手动添加。

`requires_openai_auth = true` 会保留现有 ChatGPT 登录态，因此切到外部模型后 `/status` 仍能显示当前 subscription 的套餐和 rate-limit 窗口。Web 账户刷新还会尽量显示 Codex 返回的 credit balance、月度 credit limit、earned reset credits 和 spend-control 状态；后端未返回字段时显示“未提供”，不推算精确剩余金额。这里显示的是 ChatGPT subscription 数据，不是外部 API Provider 的余额。

## Provider 类型

- `responses`：直接转发 Codex Responses 请求。
- `chat_completions`：将基础文本、工具定义和流式文本转换到 Chat Completions。
- `anthropic_messages`：转换到 Anthropic `/v1/messages`，支持文本、基础工具调用和流式文本。
- `forward`：只转发来自本地 Codex 请求的必要认证头；不在 EasyMultiProvider 中保存 ChatGPT Token。
- `api_key`：使用 Web 配置保存的本地 API Key，保存后写入加密 secret 文件。
- `anthropic_api_key`：使用 `x-api-key` 与 `anthropic-version` 调用 Anthropic Messages。

第一版覆盖通用文本、工具定义转换和流式文本链路；流式 function-call 增量拼装、reasoning、图片、搜索和 Codex 专用工具仍取决于上游协议，接入后应先用一个真实的工具调用任务验证。

## API

```text
GET  /api/config
POST /api/config
POST /api/providers/discover
GET  /api/accounts
POST /api/accounts/import
POST /api/accounts/<id>/quota
DELETE /api/accounts/<id>
POST /api/catalog/refresh
GET  /api/integration
GET  /v1/models
POST /v1/responses
GET  /healthz
```

默认只监听 `127.0.0.1`，Web 管理和账户操作仅通过本机同源请求提供，不开放 LAN/公网。

## 测试

```bash
EASY_MULTI_PROVIDER_MASTER_KEY="$(uv run python -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())')" \
  uv run python -m unittest discover -s tests -v
```

真实 Codex CLI 固定响应测试默认跳过，显式运行：

```bash
EASY_MP_RUN_CODEX_CLI=1 PYTHONPATH=. \
  uv run python -m unittest tests.test_codex_cli_demo -v
```

它会在临时目录生成 `demo/fixed`，启动本地固定 Responses 上游和 EasyMultiProvider，再执行 `codex exec --ignore-user-config --ephemeral ... -m demo/fixed`。预期最终响应严格等于 `EASY_MULTIPROVIDER_DEMO_OK`；测试不会修改 `~/.codex/config.toml`，也不会保存 Codex 会话。
