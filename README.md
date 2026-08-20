# EasyMultiProvider

Local, browser-configured model routing for Codex. EasyMultiProvider keeps
Codex as the client and exposes one local Responses endpoint that can route to
Codex subscriptions, OpenAI-compatible APIs, Gemini AI Studio, and Anthropic
Messages providers.

The stable public baseline is `v0.1.0`; the current development line targets
`v0.4.0`. The v0.3.0 subscription-forwarding changes are merged and locally
validated; other platforms and the ChatGPT App path still require separate
manual acceptance.

## Install a Release

Download the wheel from the GitHub Release page and install it in an isolated
environment:

```bash
uv venv
uv pip install ./easy_multi_provider-0.4.0-py3-none-any.whl
```

The wheel includes the Web UI, so it does not require a source checkout at
runtime. The source checkout remains useful for development and tests.

## Features

- Prefix-based model routing, such as `glm-provider/glm` and
  `subscription/gpt-5.6-luna`.
- Multiple encrypted Codex subscription credentials.
- Duplicate subscription detection without deleting encrypted credentials.
- Per-subscription model visibility without exposing native hidden models such
  as Codex Auto Review.
- Chat Completions, Responses, and Anthropic Messages upstream protocols.
- Browser model discovery, manual model editing, visibility toggles, and
  connection tests, including a Provider-wide hide/show action.
- Codex quota, rate-limit, and credit snapshots when the local Codex app-server
  exposes them.
- Encrypted `.emp` migration bundles for moving configuration and credentials
  between machines.
- Loopback-only management by default.

## Quick Start

EasyMultiProvider uses `uv` for Python, the virtual environment, and locked
dependencies. No separate version manager is required.

```bash
# Run these commands from the project directory.
uv python install 3.11
uv sync
cp config.example.json config.json
```

Generate a Fernet key once and store it in a password manager or another
private local secret store:

```bash
uv run python -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())'
```

Export the same key whenever EasyMultiProvider runs:

```bash
export EASY_MULTI_PROVIDER_MASTER_KEY='<your-existing-fernet-key>'
uv run python -m easy_multi_provider --config config.json
```

For a local-only setup, the CLI also reads `state/master.key` when the
environment variable is absent. The file must be a private `0600` file and is
ignored by Git. The environment variable takes precedence.

The server prints a one-use browser URL. Open that URL to enter the Web UI.
The default listener is `http://127.0.0.1:4200`; use `--port` if that port is
already occupied.

EMP automatically prefers `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` from its
launch environment. When they are absent, it reads the operating system proxy
settings; on Linux this includes a GNOME manual proxy. It does not guess by
scanning common local proxy ports. Startup prints `Network proxy: environment`,
`system`, or `direct` so the active path is visible.

If startup reports an environment proxy whose local service is not running,
external Providers such as Gemini will fail while local Codex routing still
works. Fix or start that proxy, or test direct access with:

```bash
env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY \
    -u http_proxy -u https_proxy -u all_proxy \
    uv run python -m easy_multi_provider --config config.json
```

The Web UI writes subscription credentials and provider API keys to encrypted
files under `state/`. `config.json`, `state/`, and generated catalogs are
ignored by Git. Never put the master key, `auth.json`, or an API key in the
repository.

## Web UI

The dashboard provides:

- **Accounts**: import `auth.json` and backup names such as `auth.json.bk1`
  through a file picker, enter only the account ID, refresh quota, edit the
  combined display-name/model-prefix value, choose which native Coding Agent
  models that subscription exposes, and remove local encrypted credentials.
  Native entries already hidden by Codex, such as Auto Review, are never
  offered as subscription models. Export or import an encrypted `.emp` bundle
  for migration.
- **Providers**: choose an official preset with fixed endpoint/protocol data,
  including **ChatGPT Subscription**, which forwards the Codex login carried by
  the EMP profile, or create a custom Provider with a Base URL, protocol, and
  API key without exposing the stored key back to the browser.
- **Models**: preview the upstream model list, select which models to import,
  edit context windows, test a model, and hide unused entries without deleting
  them. Models are grouped by Provider; visible and newer models appear first.
  A Provider row can hide or show all of its imported models at once.
- **Codex integration**: generate the merged catalog and write one EMP profile
  to `$CODEX_HOME/emp.config.toml` (or `~/.codex/emp.config.toml` by default),
  then show native Codex start and resume commands.

Only the account ID is required during import. Display name and model prefix
default to that ID.

## Data Migration (`.emp`)

Use **Export EMP Data** to download an encrypted `.emp` bundle. The bundle is
protected by a migration password and contains configuration, model routes,
Provider keys, and Codex subscription credentials.

On the destination machine:

1. Create a new local `state/master.key` or set
   `EASY_MULTI_PROVIDER_MASTER_KEY` to a newly generated key.
2. Start EasyMultiProvider and choose **Import EMP Data**.
3. Select the `.emp` file and enter the migration password.

The source and destination `state/master.key` values may be different. The
source master key is never included in the bundle; imported credentials are
decrypted in memory and re-encrypted with the destination key. Import uses
merge semantics: matching account, Provider, and model IDs are replaced, and
other destination entries are kept. The destination machine's host, port,
Codex endpoint, catalog path, and local vault paths are kept as-is.

Do not copy the source machine's `state/master.key` to the destination. Create
the destination key locally, then use the `.emp` file and migration password to
move the encrypted data.

## Migration on Another Machine

For a fresh destination that will receive an EMP migration bundle, install the
same source line with Python and `uv`:

```bash
git clone git@github.com:Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
uv sync
cp config.example.json config.json
uv run python -c 'from cryptography.fernet import Fernet; print(Fernet.generate_key().decode())'
```

Save the generated value as the destination's private `state/master.key`, or
use `EASY_MULTI_PROVIDER_MASTER_KEY`. Then start EMP, open its one-use browser
URL, and choose **Import EMP Data**. The source `config.json`, `state/`, and
planning files are not required on the destination.

## Codex Profile and Shared Session History

Click **Generate EMP Config** in the Web UI. EMP writes one profile for the
whole router and prints the exact command to use. Start EMP in one terminal
and the opt-in profile in another:

```bash
uv run python -m easy_multi_provider --config config.json
codex --profile emp
```

All accounts and Providers managed by EMP are one Provider from Codex's point
of view. Use `/model` to switch between entries such as
`subscription/gpt-5.6-luna`, `glm-provider/glm`, and
`gemini/gemini-2.5-flash` within the same EMP session group.

The plain Codex command remains the normal subscription client:

```bash
codex
codex resume --all
```

The EMP profile keeps Codex's native `openai` session identity and changes only
its `openai_base_url`. This lets the native resume picker show the same history
as the default profile:

```bash
codex resume --profile emp
codex resume --all --profile emp
```

No helper command or session database migration is required. Without `--all`,
Codex keeps its normal current-directory filter. Because the history is shared,
plain `codex resume` can also display sessions created through the EMP profile;
the profile-specific Base URL does not affect plain `codex` startup.

The generated profile does not disable Codex request-body compression, remote
compaction, WebSocket, or other native features. EMP accepts compressed HTTP
requests and the Responses WebSocket transport. It passes native Responses
compaction through and supplies compatible compaction for translated Chat
Completions and Anthropic models.

An unprefixed native model such as `gpt-5.6-luna` requires exactly one enabled
`forward` provider. Otherwise use the account prefix or the provider prefix.

Custom Providers can use `auto` protocol mode without first importing models.
The first real model request prefers Responses and falls back to Chat
Completions only when the Responses endpoint explicitly rejects that protocol;
EMP then saves the working protocol. This negotiation does not send a separate
generation request. Clicking **Pull Models** checks `/models` and lets you
select models to import, but a model-list response alone is not treated as
proof of a generation protocol. Select Anthropic Messages explicitly, or use
Anthropic authentication, for Anthropic-compatible endpoints.

Once a streamed response has started, its HTTP or WebSocket handshake has
already succeeded. A later upstream timeout is therefore reported as a terminal
`response.failed` event whose message includes the upstream HTTP status (for
example `HTTP 504`), rather than as a second HTTP response or a generic missing
terminal-event error. Codex may still apply its own native retry policy to a
reported 504; EMP does not disable or override that client behavior.

For API-key Responses Providers, EMP keeps the complete model-visible input
and tool definitions but removes Codex-only `client_metadata` before forwarding
the request. Native subscription passthrough keeps that metadata. If an
upstream returns an HTML error page, EMP omits the page body and reports a
concise status such as `HTTP 403 (text/html)`; this normally indicates that the
upstream gateway or WAF rejected a coding-agent request rather than a protocol
negotiation failure.

Chat Completions providers preserve standard structured tool calls, including
tool-call history on the next turn. If an upstream emits `<think>` or
`<tool_call>` as ordinary text, EMP stops that response with a clear upstream
error instead of exposing or executing the markup. The Provider editor also
offers **仅文本**, which omits tool definitions for endpoints that should only
answer conversationally.

## Provider Routing

| Protocol | Typical upstream | Authentication |
|---|---|---|
| `responses` | Codex or Responses-compatible API | forwarded Codex auth or API key |
| `chat_completions` | OpenAI-compatible APIs and Gemini AI Studio | Bearer API key |
| `anthropic_messages` | Anthropic-compatible API | Anthropic API key |

For a GLM provider, configure an ID such as `glm-provider`, choose **Chat
Completions**, save the API key, and add or discover the upstream model `glm`.
Use the resulting Codex model ID:

```text
glm-provider/glm
```

If the upstream requires `/v1`, include `/v1` in the Base URL. EasyMultiProvider
appends the protocol endpoint, for example `/chat/completions`.

## ChatGPT App Compatibility Check

EMP is a separate local process and does not write the default Codex config or
change the ChatGPT App by itself. The supported workflow is to use the `emp`
profile explicitly for EMP and leave the normal profile unchanged.

Before relying on the setup, verify both paths:

1. Test a normal ChatGPT App conversation with EMP stopped.
2. Start EMP and repeat a normal ChatGPT App conversation.
3. Run `codex --profile emp`, choose `glm-provider/glm` with `/model`, and
   confirm the request reaches EMP.
4. Resume a normal session with plain `codex resume --all`.
5. Stop EMP and confirm the normal App/session path still works.

The ChatGPT App path is a manual acceptance test because its provider/config
loading behavior depends on the installed App version.

## API

| Endpoint | Method | Description |
|---|---|---|
| `/healthz` | GET | Process health check |
| `/api/config` | GET/POST | Read or update safe configuration metadata |
| `/api/accounts/import` | POST | Import and encrypt a Codex credential |
| `/api/accounts/<id>/quota` | POST | Refresh one account's quota |
| `/api/providers/discover` | POST | Discover provider models |
| `/api/catalog/refresh` | POST | Regenerate the Codex model catalog |
| `/api/integration` | GET | Return the Codex integration snippet |
| `/api/integration/generate` | POST | Regenerate the catalog and write the EMP Codex profile |
| `/api/migration/export` | POST | Download an encrypted `.emp` migration bundle |
| `/api/migration/import` | POST | Decrypt and merge an `.emp` migration bundle |
| `/v1/models` | GET | List enabled routed models |
| `/v1/responses` | POST | Codex-facing Responses endpoint |

The service binds to loopback by default. Do not expose it to a LAN or public
network without adding an appropriate access boundary.

## Testing

```bash
uv run python -m unittest discover -s tests -v
```

The optional real Codex CLI demo is disabled by default:

```bash
EASY_MP_RUN_CODEX_CLI=1 PYTHONPATH=. \
  uv run python -m unittest tests.test_codex_cli_demo -v
```

Useful local checks after starting the server:

```bash
curl http://127.0.0.1:4200/healthz
curl http://127.0.0.1:4200/v1/models
```

## Development Notes

- Python code uses the standard library HTTP server and `uv`-managed
  dependencies.
- Credentials are decrypted only when needed and are never returned by the
  configuration API.
- Migration bundles are encrypted with a user-supplied password; the target
  machine re-encrypts imported credentials with its own local master key.
- External API providers do not claim to have Codex subscription balances.
- Run a real text request before relying on tools, reasoning, images, search,
  or provider-specific features; protocol support varies by upstream.
- The v0.3.0 release candidate has not been audited with Codex Security.
