<p align="center">
  <img src="assets/branding/easy-multi-provider-icon.svg" alt="EMP logo" width="112">
</p>

<h1 align="center">EMP — EasyMultiProvider</h1>

<p align="center">
  <strong>One Codex model picker for multiple ChatGPT accounts and external API models.</strong>
</p>

<p align="center">
  <a href="https://github.com/Killow1998/EasyMultiProvider/releases/latest"><img alt="GitHub release" src="https://img.shields.io/github/v/release/Killow1998/EasyMultiProvider"></a>
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/Killow1998/EasyMultiProvider"></a>
  <img alt="Codex CLI 0.149.x–0.156.x" src="https://img.shields.io/badge/Codex%20CLI-0.149.x--0.156.x-blue">
  <img alt="Windows Linux macOS" src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey">
</p>

<p align="center">
  <a href="https://github.com/Killow1998/EasyMultiProvider/releases/latest"><strong>Download</strong></a>
  · <a href="#quick-start">Quick Start</a>
  · <a href="#what-emp-does">Features</a>
  · <a href="#docs">Docs</a>
  · <a href="README.zh-CN.md">中文</a>
</p>

EMP runs locally beside Codex and does two things:

1. **Switch accounts in one model picker.** Import additional ChatGPT subscriptions, then choose their models in the same `/model` list or Codex App menu as your current login. You do not have to sign out to use another account.
2. **Use external models like Codex models.** Add an API provider such as DeepSeek and import its models into that picker. EMP adapts supported protocols so sessions, tool calls, and model switching feel close to native Codex use when the upstream model supports them.

Configure accounts and providers in EMP's local Web UI, then apply the catalog to Codex.

For example, one model picker can show:

| Model shown in Codex | Where the request goes |
| --- | --- |
| `gpt-5.6-luna` | Your current Codex login |
| `team/gpt-5.6-luna` | An imported ChatGPT subscription |
| `deepseek/deepseek-v4-pro` | A model you imported from the DeepSeek API |

The prefixed names are examples; you choose the account, provider, and models. Selecting `team/gpt-5.6-luna` uses the imported account, while `deepseek/deepseek-v4-pro` uses the DeepSeek API key. Codex still owns the coding session, permissions, and tools.

## Quick Start

### 1. Download EMP

Download the latest reviewed build from [GitHub Releases](https://github.com/Killow1998/EasyMultiProvider/releases/latest).

| Platform | Package | Install and launch |
| --- | --- | --- |
| Windows x64 | `EMP.exe` | Double-click `EMP.exe` |
| Ubuntu 22.04+ x64 | `EMP-linux-x86_64.tar.gz` | Use the user-install commands below, then open **EMP** from the application menu |
| Ubuntu 22.04+ x64 | `EMP-linux-x86_64.deb` | Run `sudo apt install ./EMP-linux-x86_64.deb`, then open **EMP** |
| macOS Apple Silicon | `.dmg` | Drag **EMP** to Applications |
| macOS Intel | `.dmg` | Drag **EMP** to Applications |

For the Linux `.tar.gz`, run these commands in the download directory:

~~~bash
tar -xzf EMP-linux-x86_64.tar.gz
cd EMP
./install-user.sh
~~~

The `.deb` is a separate system-managed installation; it does not contain `install-user.sh`.

The [package workflow](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/package.yml) builds and smoke-tests the native artifacts before a release is published.

> macOS release artifacts are currently unsigned development builds. Public distribution still requires Developer ID signing and notarization.

### 2. Start EMP

EMP opens an authenticated local Web UI. A successful start prints:

~~~text
EMP listening on ...
~~~

Keep the EMP process running while using it.

### 3. Add what you want to use

In the Web UI, either:

- import another Codex / ChatGPT subscription account,
- add an API Provider,
- or keep only the current native Codex login and use EMP for model visibility and display settings.

For an API Provider, pull the upstream model list, choose the models you want, and optionally edit their context windows.

### 4. Apply EMP to Codex

Click **Apply EMP to Codex**.

EMP scans known Codex runtimes, shows their versions, and lets you select compatible clients. Multiple selected clients and workspaces can run concurrently.

### 5. Select a model normally

Open Codex and choose a model from `/model` or the App model menu.

Readable route prefixes make the source explicit, for example:

~~~text
team/gpt-5.6-luna
provider/model
~~~

With a ChatGPT login, catalog changes can refresh while Codex is running. Codex 0.155.0 also refreshes periodically; reopen the model picker if a newly added model is not visible immediately.

## What EMP does

### Accounts and model routing

- Use native Codex models, imported ChatGPT subscription models, and external API models from one catalog.
- Import multiple subscription accounts and refresh available quota data.
- Choose which Coding Agent models each subscription exposes.
- Add official or custom Providers through the Web UI.
- Discover Provider models, import only the ones you want, test them, edit context limits, hide them, or remove them.
- Preserve text, image, reasoning, and structured tool capabilities when the destination reports or supports them.
- Let Codex delegate a native child task to an external catalog model by its existing model slug while Codex continues to own the child task and permissions.
- Let external models use Codex standalone web search. EMP prefers the current `.codex` login and can fall back to an available imported account without exposing Provider credentials.

### Codex continuity

EMP is designed to keep provider changes from turning into a different coding client.

It preserves native Codex sessions, `resume`, WebSockets, compression, and MCP where supported. For compacted tasks that switch between the current login, imported subscriptions, and external models, EMP reconstructs only Codex-owned visible history instead of forwarding provider-private opaque state.

External subagent delegation, follow-up tasks, and tool calls have been verified with Gemini 3.7 Flash and 3.8 Flash on Codex runtime `0.153.4`. See [external collaboration compatibility](docs/external-collaboration.md).

### Usage, quota, and cost

EMP records local operational metrics so you can see where your coding-agent usage is going.

- Historical and live token usage by time, account, and Provider.
- API-equivalent cost estimates with daily price updates.
- Subscription quota snapshots and local trends.
- Upstream-reported prompt cache hit rates in token-weighted 10-minute periods.
- Rolling median TTFT and TPS from the latest 20 valid calls per recently used model, compared with the preceding window.
- Observed success, 429, 502, 503, and 504 rates.

Performance history survives EMP restarts. Missing upstream cache data is shown as unavailable rather than estimated.

See [usage accounting](docs/usage-accounting.md) and [usage verification](docs/usage-verification.md).

### Model display and context windows

Subscription editing supports per-model context token counts. Leave a field blank to use the model default.

**Refresh model limits** retrieves the subscription catalog with that account's credentials. Configured values cannot exceed the upstream-advertised maximum. Codex's default 95% effective percentage is preserved, so an advertised 872,000-token window becomes 828,400 usable tokens.

The catalog display and EMP request checks use the same effective window.

## Codex compatibility

The current source version is **v0.11.10**.

EMP supports Codex CLI **0.149.x through 0.156.x**; **0.156.1 is recommended**.

On the first integration-status load, EMP performs a bounded scan of known locations for:

- the Codex App runtime,
- the active `.codex` managed runtime,
- OpenAI's VS Code / Cursor extension runtime,
- a standalone `codex` on `PATH`,
- and, on Linux, `$CODEX_HOME/plugins/.plugin-appserver/codex`.

Detected runtimes are deduplicated. Unsupported or unreadable runtimes remain visible but cannot be selected; eligible pre-release or newer versions are shown as unverified.

EMP treats a persistent Codex App Server as externally owned. Enabling, restoring, refreshing, or checking integration files does **not** stop, start, or restart Codex.

EMP reads `model/list` from the existing local control socket on Windows, macOS, and Linux and compares the visible model catalog with its saved state.

## Web UI

The local Web UI is organized around four main areas:

- **Accounts** — import subscription credentials, edit display names and prefixes, refresh quota, view quota history, and control model visibility.
- **Providers** — configure supported services or a custom Provider.
- **Models** — discover, import, test, edit, hide, or remove Provider models.
- **Codex integration** — apply the EMP catalog to Codex or restore native Codex routing.

EMP automatically detects proxy settings from its launch environment or operating system.

## Local security and diagnostics

EMP binds the management UI to the local machine by default.

- Subscription credentials and Provider API keys are encrypted locally.
- Saved credentials are not returned to the browser after being stored.
- Local configuration, encrypted state, and generated catalogs are excluded from Git.
- EMP creates a private local encryption key automatically on first start.
- No manual key-generation environment variable is required.

Each start also prints a `Diagnostic log: ...` path. The diagnostic journal stores bounded structured runtime metadata for troubleshooting.

It does **not** record prompts, responses, tool payloads, HTTP bodies, headers, cookies, or credentials.

Managed logs are kept under `state/logs/`; the oldest data is removed automatically when the journal exceeds 10 MiB in total.

See [diagnostic journal specification](docs/diagnostic-journal-spec.md).

## Migration

EMP can export and import password-protected `.emp` migration files.

An export can include:

- Native Codex settings and credentials,
- additional subscription accounts,
- external Providers and API keys,
- imported models,
- model-family display settings.

Native credentials imported on another machine become an additional subscription instead of replacing the destination machine's current login.

EMP v0.9.0 through v0.9.9 use the same encrypted migration format, and a current EMP can import files exported by those versions. Use the same or a newer EMP version when moving settings forward so newer fields are not lost.

## Command-line mode

Packaged builds can also run explicitly as a service.

Windows:

~~~powershell
.\EMP.exe --version
.\EMP.exe serve --config config.json
~~~

Linux archive, from its extracted directory:

~~~bash
./EMP --version
./EMP serve --config config.json
~~~

With the `.deb`, use `EMP` instead of `./EMP` after installation.

EMP listens on `http://127.0.0.1:4200` by default. Use `--port` only when that port is already occupied.

## Install from source

Install Git and [`uv`](https://docs.astral.sh/uv/getting-started/installation/), then:

~~~bash
git clone https://github.com/Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
uv sync
uv run python -m easy_multi_provider serve --config config.json
~~~

`uv` manages Python, the virtual environment, and locked dependencies. No separate Python version manager is required.

## Configuration locations

Desktop launch stores configuration in the normal per-user location:

| Platform | Config |
| --- | --- |
| Windows | `%LOCALAPPDATA%\EasyMultiProvider\config.json` |
| macOS | `~/Library/Application Support/EasyMultiProvider/config.json` |
| Linux | `$XDG_CONFIG_HOME/easy-multi-provider/config.json` or `~/.config/easy-multi-provider/config.json` |

The Linux user installer places the binary at `$XDG_DATA_HOME/easy-multi-provider/EMP` (default `~/.local/share/easy-multi-provider/EMP`) and the launcher at `~/.local/bin/EMP`.

The Linux user installer and Web UI updates do not require sudo or an administrator password. Installing the system `.deb` does. Configuration and account data stay in the user configuration directory and are not replaced by binary updates.

## Docs

Useful technical references:

- [Usage accounting](docs/usage-accounting.md)
- [Usage verification](docs/usage-verification.md)
- [External collaboration compatibility](docs/external-collaboration.md)
- [HTTP forwarding](docs/http-forwarding.md)
- [Request limits](docs/request-limits.md)
- [Self-update behavior](docs/self-update.md)
- [Packaging](docs/packaging.md)
- [Diagnostic journal specification](docs/diagnostic-journal-spec.md)
- [Sidechat history handling](docs/sidechat-history.md)
- [Changelog](CHANGELOG.md)

## Notes

- Existing tasks may need a catalog refresh after context-window or model-display changes; verify the effective window in a new task when it matters.
- Restart Codex once after upgrading from an older EMP static catalog or after changing Codex's Base URL.
- Restore Native Codex before rolling back to EMP 0.9.91 or earlier.
- Existing system `.deb` installations are not removed automatically when moving to the user installer; stop the old EMP and back up its configuration first.

## License

EMP is released under the [MIT License](LICENSE).
