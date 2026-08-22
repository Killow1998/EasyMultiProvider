# EasyMultiProvider

Local, browser-configured model routing for Codex. EasyMultiProvider keeps
Codex as the client and exposes one local Responses endpoint that can route to
Codex subscriptions, OpenAI-compatible APIs, Gemini AI Studio, and Anthropic
Messages providers.

The current source version is `v0.5.0`. It is covered by offline tests and has
been accepted on the current Linux host with Codex CLI/TUI, Desktop App,
native and prefixed subscriptions, external models, image input, and native
resume. Cross-platform packaging and runtime checks remain future hardening.

## Install

Clone the repository and let `uv` create the isolated Python environment:

```bash
git clone git@github.com:Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
uv sync
```

The package includes the Web UI. A local wheel can be produced with `uv build`
when a source checkout is not desired on the target machine.

## Features

- Prefix-based model routing, such as `glm-provider/glm` and
  `subscription/gpt-5.6-luna`.
- Multiple encrypted Codex subscription credentials.
- Duplicate subscription detection without deleting encrypted credentials.
- Per-subscription model visibility without exposing native hidden models such
  as Codex Auto Review. Hidden native service models remain routable for Codex
  internal features even though users cannot select them in `/model`. When an
  imported account matches the current Codex login, its visibility choices
  control the unprefixed native model list instead of creating duplicate aliases.
- Chat Completions, Responses, and Anthropic Messages upstream protocols.
- Provider-advertised text/image model capabilities in the Codex catalog, with
  image-preserving Responses forwarding and Chat Completions conversion.
- Browser model discovery, manual model editing, visibility toggles, and
  connection tests, including a Provider-wide hide/show action.
- Codex quota, rate-limit, and credit snapshots when the local Codex app-server
  exposes them.
- Recoverable default-Codex integration that preserves native session identity,
  `codex resume`, WebSockets, request compression, remote compaction, and MCP.
- Capability provenance, bounded privacy-safe diagnostics, and a Context Guard
  that checks the translated upstream payload without owning conversation history.
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
uv run python -m easy_multi_provider serve --config config.json
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
    uv run python -m easy_multi_provider serve --config config.json
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
  offered as subscription models. A duplicate of the current Codex login stays
  visible for management, does not create prefixed duplicates, and controls
  which unprefixed native models appear in `/model`. Export or import an
  encrypted `.emp` bundle for migration.
- **Providers**: choose an official preset with fixed endpoint/protocol data,
  including **ChatGPT Subscription**, which forwards the Codex login carried by
  the incoming Codex request, or create a custom Provider with a Base URL,
  protocol, and API key without exposing the stored key back to the browser.
- **Models**: preview the upstream model list, select which models to import,
  edit context windows, test a model, and hide unused entries without deleting
  them. Models are grouped by Provider; visible and newer models appear first.
  A Provider row can hide or show all of its imported models at once.
- **Codex integration**: explicitly enable EMP for the default Codex
  configuration after the listener is ready, inspect recovery state, and
  restore native Codex without a wrapper command or separate profile.

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

## Default Codex Integration and Shared Session History

Start EMP, open its one-use browser URL, and click **Enable Default Codex**.
Activation is explicit and is available only after the listener is ready:

```bash
uv run python -m easy_multi_provider serve --config config.json
```

EMP temporarily manages only the root `openai_base_url` and
`model_catalog_json` fields in `$CODEX_HOME/config.toml`. It records a private
integration lease so a normal shutdown, a later EMP start, or the offline
recovery command can compare and restore those fields without rewriting
unrelated TOML settings or comments.

**Enable EMP for Codex** and **Restore native Codex** are each one confirmed
transaction. EMP writes the target catalog and leased configuration first,
then asks Codex Remote Control to stop gracefully. Because `stopped` and
`notRunning` cover only its pid-managed daemon, EMP always follows either result
with one narrowly scoped residual psutil scan. The same scan is used for the
documented unmanaged App Server error, but not for permission or malformed
responses. It gracefully terminates only verified same-user foreground Remote
Control or listening App Server hosts whose effective `CODEX_HOME` matches the
active integration manager. EMP classifies owner, executable, and semantic
argv before reading only that candidate's `CODEX_HOME`; an absent value means
the current user's platform-default Codex home. Different, unreadable,
ambiguous, or changed homes are excluded and revalidated before termination.
Official Node `bin/codex` shims must
resolve to `@openai/codex/bin/codex.js`; Codex clients, helpers, lookalikes,
ambiguous identities, and other users are excluded. EMP never escalates to a
hard kill. Known value-taking Codex root options may precede the host
subcommand; unknown options, missing values, and unrelated positional prefixes
make the process ineligible instead of triggering a loose argv search.
EMP never starts or restarts Codex. If an external owner brings Codex back, EMP observes
`model/list` for at most 20 seconds and verifies the complete expected model
set. If nothing returns, the operation succeeds as
`stopped_waiting_for_start`; the next normal Codex start loads the target.

A later catalog refresh only records `reload_required`. Use the visible
**Sync and reconnect Codex** action once to apply that already-written catalog.
Every interrupting action uses a custom confirmation modal and is never
triggered by page loading, ordinary saving, `doctor`, or offline `restore`.
Process identifiers, command lines, executable paths, and environment values
are not returned by the API, displayed in the Web UI, logged, or persisted.

After activation, use the ordinary native commands:

```bash
codex
codex resume
codex resume --all
```

Use `/model` to switch between native subscription aliases and external models
such as `subscription/gpt-5.6-luna`, `glm-provider/glm`, or
`gemini/gemini-2.5-flash`. When a model has a known context limit, its display
name includes the usable size, such as `GPT-5.6-Sol [258K]`; unknown limits are
left unlabeled. The Desktop App uses this display name, while the TUI receives
the same value as `Context 258K` in the model description because its picker
renders the model slug as the primary label. No `emp-resume`, `--profile emp`, alternate session directory,
or session database migration is involved; Codex remains the only owner of
threads, history, resume, fork, and native compaction.

EMP does not disable Codex request-body compression, remote compaction,
WebSockets, MCP, or other native features. It accepts compressed HTTP requests
and the Responses WebSocket transport, passes native Responses compaction
through, and supplies compatible compaction for translated Chat Completions and
Anthropic models.

If EMP was interrupted or Codex still points at a stopped listener, inspect and
repair the leased integration without starting the server:

```bash
uv run python -m easy_multi_provider doctor
uv run python -m easy_multi_provider restore
```

Both commands support `--json`. `doctor` is read-only and reports durable
configuration separately from stale/offline runtime accounting; it never
probes Codex. Offline `restore` changes only EMP-owned fields that still match
the lease and records `reload_required`; it never claims `native_loaded`
without a live observation. Conflicting user edits are reported and preserved.

Known unprefixed native models such as `gpt-5.6-luna` automatically use the
current validated Codex login. No visible `forward` Provider is required.
Imported subscriptions and external Providers continue to use their configured
prefixes.

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
error instead of exposing or executing the markup. EMP always preserves Codex
tool definitions; conversational-only endpoints are not suitable as coding-agent
Providers.

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

EMP is a separate local process. It changes the default Codex configuration
only after **Enable Default Codex** is clicked and restores the two leased fields
on a normal shutdown. Desktop App support uses the same public Codex
configuration path; it does not use browser automation or private App state.

Before relying on the setup, verify both paths:

1. Test a normal Desktop App conversation with EMP stopped and native state
   restored.
2. Start EMP and click **Enable Default Codex**. Confirm the one operation that
   writes the target configuration and gracefully releases any stale runtime.
   No second synchronization action is required for initial enable.
3. Choose one native subscription model and one external model and complete a
   fresh request with each.
4. Confirm plain `codex resume --all` still shows the same native history.
5. Stop EMP normally, or run `restore`, and confirm the App and CLI return to
   native routing.

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
| `/api/integration` | GET | Read safe leased-integration status |
| `/api/integration/enable` | POST | Enable EMP for the default Codex configuration |
| `/api/integration/restore` | POST | Compare and restore EMP-owned Codex fields |
| `/api/integration/reload` | POST | User-confirmed graceful stop and model-list verification after a later catalog change |
| `/api/capabilities` | GET | Read capability provenance and safe context status |
| `/api/diagnostics` | GET | Read the bounded privacy-safe request status ring |
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
- Missing modality metadata remains text-only. After importing a vision model,
  regenerate the catalog, fully restart Codex, and verify one real image request;
  protocol and vision support still depend on the upstream.
