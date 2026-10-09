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
  <img alt="Codex CLI · App · IDE" src="https://img.shields.io/badge/Codex-CLI%20%C2%B7%20App%20%C2%B7%20IDE-blue">
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
| macOS Apple Silicon | `.dmg` | Drag **EMP** to Applications |
| macOS Intel | `.dmg` | Drag **EMP** to Applications |

For the Linux `.tar.gz`, run these commands in the download directory:

~~~bash
tar -xzf EMP-linux-x86_64.tar.gz
cd EMP
./install-user.sh
~~~

The current release does not publish a Linux `.deb`. Existing system-managed
installations remain owned by their package manager; the user installer does not
remove them automatically.

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

For Claude, open **Add service** and choose **Local subscription** or **CPA**
in the **Claude Code** section. Both routes require the installed Claude Code CLI.

- **Local Claude subscription** reuses the Claude Code subscription sign-in for
  the current OS user. Before each request, EMP checks for a Claude.ai subscription
  sign-in; CLI API-key and Console auth do not qualify for this mode. If needed,
  sign in through Claude Code. Open **Model settings**, click **Update model list**,
  select models and save to add them to Codex's model picker. **Add model manually**
  remains available for additional aliases or full IDs.
- **CPA** uses a Claude-Code-compatible CPA Base URL and API key. For direct
  Anthropic API access, add the separate **Anthropic API** Provider.

### 4. Apply EMP to Codex

Click **Apply EMP to Codex**. The status line shows your Codex version and
**EMP models loaded** once the model list is ready. If it asks you to reopen
Codex, finish your current work and open Codex again.

EMP writes its settings to the shared `~/.codex/config.toml`, so every Codex client (CLI, App, IDE extensions) picks them up. Multiple clients and workspaces can run concurrently.

### 5. Select a model normally

Open Codex and choose a model from `/model` or the App model menu.

Readable route prefixes make the source explicit, for example:

~~~text
team/gpt-5.6-luna
provider/model
~~~

With a ChatGPT login, supported Codex clients can pick up catalog changes while
running. Changes become available on a later request; reopen the model picker if
a new model is not visible yet.

## What EMP does

### Accounts and model routing

- Use native Codex models, imported ChatGPT subscription models, and external API models from one catalog.
- Import multiple subscription accounts and refresh available quota data.
- Choose which Coding Agent models each subscription exposes.
- Sync native and subscription model catalogs and capabilities from authenticated
  Codex catalog responses. EMP refreshes automatically, keeps the last good cache
  when refresh fails, and preserves visibility, aliases, and context overrides.
  A newly exposed model needs no EMP release or manual model-list edit.
- Add official or custom Providers through the Web UI.
- Route Claude requests through the installed Claude Code CLI using either the
  current OS user's Claude Code sign-in or a compatible CPA Base URL and API key.
  For local sign-in, open **Model settings**, update the list from Claude Code,
  and select models to add to Codex. Saved lists can be reused; **Add model**
  remains available for manual entries. Codex executes tool calls, and replies
  are buffered until generation finishes.
- Forward image inputs and supported inline document content only for Claude
  models whose configured capabilities and selected upstream support them.
- Discover Provider models, import only the ones you want, test them, edit context limits, hide them, or remove them.
- See dispatched request activity beside model, Provider, and subscription rows.
  Active dots mark in-flight requests, with the count in their tooltips. A
  distinct recent state marks completed requests and expires automatically. A
  lost event stream clears active confidence.
- Preserve text, image, reasoning, and structured tool capabilities when the destination reports or supports them.
- Let Codex delegate a native child task to an external catalog model by its existing model slug while Codex continues to own the child task and permissions.
- Let external models use Codex standalone web search. EMP prefers the current `.codex` login and can fall back to an available imported account without exposing Provider credentials.

EMP refreshes the model list automatically. An already-open Codex model picker
may need reloading; the UI reports when the observed catalog still differs.

### Codex continuity

EMP is designed to keep provider changes from turning into a different coding client.

EMP preserves native Codex sessions, `resume`, WebSockets, compression, and MCP where supported. During live provider switching, it reconstructs Codex-owned visible history instead of forwarding provider-private opaque state.

To return a conversation with EMP-owned compaction data to native Codex, finish
active work and close Codex, then choose **Restore Native** in the Codex
integration area of the Web UI. EMP converts only affected saved histories and
backs up each affected history file before rewriting it, then restores native
settings. The original conversation can then be resumed on supported Codex.
When restoration finishes, EMP exits; reopen Codex to continue. You can also
run `EMP restore` from the command line.

For details about external subagent delegation, follow-up tasks, and tool calls, see [external collaboration compatibility](docs/external-collaboration.md).

### Usage, quota, and cost

EMP records local operational metrics so you can see where your coding-agent usage is going.

- Historical and live token usage by time, account, and Provider.
- API-equivalent cost estimates with daily price updates.
- Subscription quota snapshots and local trends.
- Upstream-reported prompt cache hit rates in token-weighted 10-minute periods.
- Rolling median TTFT and TPS from the latest 20 valid calls per recently used model, compared with the preceding window.
- Observed success, 429, 502, 503, and 504 rates.
- Pricing references: price a model with no public price as another model (for example as `gpt-5.5`). Requests that still have no price count as 0 and are listed as such.

Performance history survives EMP restarts. Missing upstream cache data is shown as unavailable rather than estimated.

See [usage accounting](docs/usage-accounting.md) and [usage verification](docs/usage-verification.md).

### Model display and context windows

Subscription editing supports per-model context token counts. Leave a field blank to use the model default.

**Refresh model limits** retrieves the subscription catalog with that account's credentials. Configured values cannot exceed the upstream-advertised maximum. Codex's default 95% effective percentage is preserved, so an advertised 872,000-token window becomes 828,400 usable tokens.

External Provider models use the same 95% rule, so a 256,000-token window shows as 243K in the Codex model picker. The catalog display and EMP request checks use the same effective window.

For Claude Code models, edit the context window to set Codex's history budget.
EMP checks and compacts that history; Claude Code's own compaction is disabled.
The configured budget can be smaller than the model's capacity. For models that
require the extended-context option, keep `[1m]` in the upstream model name;
models with native 1M context do not need it. Increasing the budget does not
increase the capacity offered by the model or account.

Use the global eye control in the right-side **Model display** to show or hide
context labels there and in the Codex model picker for Native, Subscription and
External Provider models. The left model list does not repeat these labels. The
control changes catalog labels, not context windows or request limits.

## Codex compatibility

The current source version is **v0.12.12**.

EMP supports Codex CLI, desktop App and IDE extension installations without a
universal minimum engine version. Each operation depends on the interfaces that
the installed engine actually provides. If an engine lacks quota reads, quota
reset or catalog inspection, EMP reports that operation as unavailable; it does
not reject all requests solely because of the version number. Authentication,
protocol and conversation-history checks still apply.

Diagnostics show the App or extension version separately from its Codex engine
version. An available engine means EMP could run it and read its version, not
that every feature has been verified. Model catalog refresh uses the observed
engine version; if it cannot be determined, EMP keeps the cached catalog and
reports that refresh is unavailable.

There is no client to choose: EMP edits the shared `config.toml` used by Codex clients.

To query account quota, EMP selects a trusted, executable Codex engine with an
observed version from these locations (a configured binary wins):

- the Codex App runtime,
- the active `.codex` managed runtime,
- OpenAI's VS Code / Cursor extension runtime,
- a standalone `codex` on `PATH`,
- on Unix, nvm installations under `NVM_DIR`, `$XDG_CONFIG_HOME/nvm`,
  `~/.config/nvm`, or `~/.nvm`,
- and, on Linux, `$CODEX_HOME/plugins/.plugin-appserver/codex`.

Discovery covers Windows x64, Linux x64, macOS Intel and macOS Apple Silicon.
EMP recognizes the known installation layouts of the CLI, IDE extension and
desktop app on each platform.

Known nvm locations also work when a desktop launcher has a minimal `PATH`.
An npm-installed Codex uses its installation's trusted Node interpreter, so
manual `codex` or `node` symlinks are unnecessary.

EMP does not stop or restart your running Codex backend. Quota checks use a
separate CLI helper; they do not launch the desktop GUI. Applying or restoring
changes the saved settings. When Codex next reaches EMP (its model list or a turn), EMP
reads `model/list` from Codex's local control socket and pushes the catalog
observation to the Web UI. This read-only check does not reload the backend or
verify its request routing. If the control interface is unavailable, EMP reports
that settings are saved and the catalog is unverified; this does not mean the
App has stopped. If a restart is needed, use the backend's usual
launcher when your active work permits it. There is no background polling.

## Web UI

The local Web UI is organized around four main areas:

- **Accounts** — use the compact account summary to view model/token usage and API-equivalent cost, save an alias, choose a reset or remove an imported account. Open Model settings from the account summary; Refresh and Trend stay visible in each row.
- **Providers** — configure supported services or a custom Provider.
- **Models** — discover, import, test, edit, hide, or remove Provider models.
- **Codex integration** — apply the EMP catalog to Codex or restore native Codex routing.

The header **Settings** button groups automatic activation, Codex web search for
external models, and automatic-review routing to another subscription when
Native runs out of quota. All three start enabled; saved choices are retained.
Click an activity dot to view recent request targets, returned models, timings,
actual dispatch counts and retries.

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

EMP listens on `http://127.0.0.1:4200` by default. Use `--port` only when that port is already occupied.

## Install from source

Install Git and a Rust toolchain ([`rustup`](https://rustup.rs/)), then:

~~~bash
git clone https://github.com/Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
cargo build --locked --release -p emp-app --bin EMP
./target/release/EMP serve --config config.json
~~~

Run the tests with `cargo test --locked --workspace --all-targets`. The Rust test suite is self-contained: it uses temporary directories and local fake upstreams, and never touches your real `~/.codex` or calls a real provider.

## Configuration locations

Desktop launch stores configuration in the normal per-user location:

| Platform | Config |
| --- | --- |
| Windows | `%LOCALAPPDATA%\EasyMultiProvider\config.json` |
| macOS | `~/Library/Application Support/EasyMultiProvider/config.json` |
| Linux | `$XDG_CONFIG_HOME/easy-multi-provider/config.json` or `~/.config/easy-multi-provider/config.json` |

The Linux user installer places the binary at `$XDG_DATA_HOME/easy-multi-provider/EMP` (default `~/.local/share/easy-multi-provider/EMP`) and the launcher at `~/.local/bin/EMP`.

The Linux user installer and Web UI updates do not require sudo or an administrator password. Configuration and account data stay in the user configuration directory and are not replaced by binary updates. Existing system `.deb` installations remain managed by their package manager.

A red dot on **Check updates** marks an available release. If an older Windows
version exits during an update without reopening, download `EMP.exe` from
[Releases](https://github.com/Killow1998/EasyMultiProvider/releases/latest),
replace the closed executable and open it. Your accounts and settings stay in
the user configuration directory. Version 0.12.5 fixes the exited-process wait
for subsequent updates.

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
