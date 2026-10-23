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

On Windows, normal startup connects Codex through `localhost` so local proxy
exceptions apply. EMP automatically upgrades its previous numeric loopback
setting on the same port and preserves the original settings for restoration.
No special launch command is needed. An already-open Codex client may need a
normal restart after its current work finishes to load changed settings.

You can use external models without a Codex subscription. Add an external service
and enable EMP; if Codex is signed out, EMP creates a key for the local connection.
Open **EMP local key** in the service list to remove it or **Sign in to a
subscription** with an official OpenAI device code. Successful sign-in replaces
the local key; restart Codex Runtime afterward. Removing the key restores the
native connection while EMP keeps running.

If saved settings or their encryption key cannot be opened, EMP shows a recovery
page with **Clear old settings and set up again** and **Quit EMP**.
Clearing requires confirmation and creates new settings and a new master key;
accounts and API keys must be imported again. **Keep a backup** is optional and
unchecked by default. Selecting it retains the old settings and key together;
otherwise the old local EMP data is removed after the reset succeeds. Native
Codex sign-in, conversations and custom external data directories are preserved.
An interrupted reset uses the same confirmed action; EMP handles the saved
transaction files automatically. Developer-supplied external key overrides are
reported as launch errors and do not enter this local-settings recovery flow.

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

Desktop launch opens an authenticated local WebUI and keeps an EMP icon in the
system tray (the menu bar on macOS). Left-click or right-click it for the same
three actions: **Open WebUI**, **Restore Native / Enable EMP**, and **Quit EMP**.
Closing the browser leaves EMP running. Restore Native keeps the tray available;
Quit EMP waits for active requests, restores native settings, and exits.
The menu follows the language selected in WebUI. Linux desktops need a
StatusNotifier-compatible tray or panel.

For foreground logs, run `EMP serve`. A successful start prints:

~~~text
EMP listening on ...
~~~

The service must remain running while EMP routing is enabled.

### 3. Add what you want to use

In the Web UI, either:

- import another Codex / ChatGPT subscription account,
- add an API Provider,
- or keep only the current native Codex login and use EMP for model visibility and display settings.

For an API Provider, pull the upstream model list, choose the models you want, and optionally edit their context windows.

For Mercury 3, choose **Inception / Mercury**, enter your Inception API key, then
open **Model settings → Update model list** and select `mercury-3`.

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

Click **Enable EMP**. The status line shows your Codex version and
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

Confirm the selected source appears in Codex's model picker, then check that a
request completes under **Statistics → Calls**. A loaded catalog and a successful
model call verify different parts of the connection.

### Quick tutorials

Select **Tutorials** in the WebUI header, then choose a task on the left to watch
its demonstration on the right. The ten GIFs are bundled for offline viewing.

[Watch the step-by-step GIF tutorials](docs/tutorials.md) for Native model settings,
imported subscriptions, external API services, and shared import/export controls.
The recordings use a Chinese interface with Chinese and English captions.

## What EMP does

### Accounts and model routing

- Use native Codex models, imported ChatGPT subscription models, and external API models from one catalog.
- Import multiple subscription accounts and refresh available quota data.
- Choose which Coding Agent models each subscription exposes.
- Sync native and subscription model catalogs and capabilities from authenticated
  Codex catalog responses. EMP refreshes automatically, keeps the last good cache
  when refresh fails, and preserves visibility, aliases, and context overrides.
  A newly exposed model needs no EMP release or manual model-list edit.
  Native refreshes write EMP's own snapshot; Codex's `models_cache.json` remains
  a read-only source for EMP, preserving Codex's cache metadata.
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
- Choose **Settings > Cross-service subagents** to allow external catalog models as children. The default **Off** keeps Native GPT subagents; **On-demand recovery** recovers encrypted tasks when needed, and **On** uses plaintext collaboration. Codex owns child tasks and permissions; the help compares speed and workflow time.
- Let external models use Codex standalone web search. EMP prefers the current `.codex` login and can fall back to an available imported account without exposing Provider credentials.

EMP refreshes the model list automatically. An already-open Codex model picker
may need reloading; the UI reports when the observed catalog still differs.

### Codex continuity

EMP is designed to keep provider changes from turning into a different coding client.

EMP preserves native Codex sessions, `resume`, WebSockets, compression, and MCP where supported. Shared HTTP upstream requests negotiate gzip, Brotli, Zstandard and deflate responses with streaming decoding and decoded-body limits. During live provider switching, it reconstructs Codex-owned visible history instead of forwarding provider-private opaque state.

While EMP is active, it converts EMP-owned checkpoints in each request when
switching to a native model. **Restore Native**, quitting EMP, and `EMP restore`
restore configuration only; they do not scan or rewrite saved conversations.
**Restore Native** keeps EMP and its management page running; **Enable EMP**
can apply integration again in the same process. **Quit EMP** restores native
settings and then ends the service.
The Web UI waits for active requests before restoring or quitting. Idle
WebSocket connections close normally. Closing the terminal or receiving an OS
exit notification restores EMP-owned settings before waiting for background
work; an active turn can be interrupted. Windows logoff and shutdown are handled
inside EMP before termination, while canceling shutdown leaves EMP running.
Force-killing a process or losing power gives it no opportunity to run cleanup.

To resume an old conversation containing EMP-owned compaction data directly in
native Codex after EMP is disabled, close that conversation and run:

```sh
EMP migrate-history --thread-id CONVERSATION_UUID
```

This migrates only the selected conversation, backs up changed history, and
preserves its parent and other conversations. It rejects migration while the
selected conversation is being written. Reopen Codex to load native settings,
then resume the same conversation. `CODEX_HOME` selects the Codex home as usual.
This tool converts EMP-owned checkpoints; it cannot decrypt native encrypted
agent messages. For those messages, the existing Cross-service subagents >
On-demand recovery setting can transcribe them for the forwarded request using
the original observed account, adding GPT usage without rewriting the saved
conversation. No separate history-recovery control is needed.

For details about external subagent delegation, follow-up tasks, and tool calls, see [external collaboration compatibility](docs/external-collaboration.md).

### Usage, quota, and cost

EMP records local operational metrics so you can see where your coding-agent usage is going.

- Historical and live token usage by time, account, and Provider.
- API-equivalent cost estimates with daily price updates.
- **Statistics > Session bills**: search by session name or ID, sort by recent
  activity, tokens or estimated USD, and expand model usage and cache reads/writes.
  Auto Review, Sidechat and subagent usage belongs to the main conversation, with
  model and task breakdowns for the selected period. Unknown prices, missing usage
  and known zero rates stay distinct.
- Subscription quota snapshots and local trends.
- Usage grouped by service type and model, with account and service breakdowns.
- Call details with the session name, selected model, model sent upstream and
  upstream-reported model, tokens, cache reads/writes, duration, TTFT and TPS.
- Performance summaries and time charts for a selected period, with per-model
  averages and sample counts. TTFT measures time to the first text or tool
  arguments. TPS measures visible output over the first-to-last content interval,
  subtracting reported reasoning tokens. Call details retain end-to-end TPS,
  including waiting and retries. Buffered responses have no estimated generation TPS.
- Interrupted, failed and delivered outcomes, dispatch counts and retries.
- Pricing references: price a model with no public price as another model (for example as `gpt-5.5`). Records without a known price are marked separately; amounts sum only usage that can be priced.

Performance history survives EMP restarts. Missing upstream cache data is shown as unavailable rather than estimated.

See [usage accounting](docs/usage-accounting.md) and [usage verification](docs/usage-verification.md).

### Model display and context windows

Subscription editing supports per-model context token counts. Leave a field blank to use the model default.

Confirmed copies of the same ChatGPT account share one service, including Native
and imported credentials. Conflicting model visibility and context settings are
resolved in the import preview before committing the import. An `auth.json`
import without model settings inherits the existing account's settings.
Existing credential and
history references are retained. Removing a merged imported service removes its
confirmed imported aliases together; the local Native sign-in is preserved.

EMP updates its catalog when services change. Native subscription integration uses
Codex's online catalog: response version notifications refresh a running engine
without restarting it. This is verified with Codex 0.162.1. That engine also
refreshes its online catalog in the background every 4 minutes 30 seconds.
An already-open picker must query the updated list; an immediate idle-picker
update is not guaranteed. API-only integration retains the static-file fallback,
and an engine already started with that static catalog needs a restart to change
catalog sources. `model/list` has no supported `forceReload` parameter.

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
External Provider models. Manage each service’s models from its own Model settings window. The
control changes catalog labels, not context windows or request limits.


Developers can verify frontend changes with [Shadow](scripts/shadow_frontend/README.md), which defaults to read-only management data and candidate accounting over isolated snapshots of real usage. Synthetic demos require an explicit selection. Review controls and simulated failures are not included in the normal management page.

## Codex compatibility

The current source version is **v0.13.14**.

Maintenance targets the latest stable Codex minor series and the previous nine;
patch releases share a series. See the [maintenance window and acceptance results](docs/codex-compatibility.md).

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

- **Services** — add subscriptions and API Providers. Click a service badge for
  its settings, lifetime model usage, alias, quota resets or removal.
- **Models** — reuse saved model lists and import, test, edit, hide or remove models.
- **Model display** — arrange the names and context labels shown in Codex.
- **Header controls** — enable EMP or restore native settings, and open
  Performance, Usage and Settings.

The header **Settings** button groups automatic activation, Codex web search for
external models, and **Routing settings**. Auto Review can use another subscription
when Native runs out of quota. Image generation and editing can follow the
conversation (the default), use a specific subscription, or automatically select
an available account. Automatic image selection considers shared and image limits;
server-side image references stay with their original account. Saved choices are retained.
Click an activity dot to view recent request targets, returned models, timings,
actual dispatch counts, retries and error sources, including image generation/edit
records and any token usage reported upstream.

EMP automatically detects proxy settings from its launch environment or operating system.
Explicit proxy environment variables take priority; otherwise EMP reads the system proxy.
Windows users can launch EMP normally without special commands. Clients should bypass their
proxy for EMP's `localhost` address; EMP can still use a proxy for upstream services such as
`chatgpt.com`. Setting only `NO_PROXY` does not disable EMP's system proxy.
If using TUN mode, confirm that the proxy software started its TUN interface successfully
with the permissions it requires. System proxy exceptions and TUN routing rules are separate.
For failed HTTP calls, diagnostics distinguish unavailable proxies, DNS failures, TLS
failures and connection timeouts, with guidance for the corresponding settings.

### Troubleshooting

| Symptom | Check first | Next step |
| --- | --- | --- |
| A model is missing from Codex | EMP model settings and integration status | Select and save the model, enable EMP, then reopen the model picker. If EMP asks you to reopen Codex, finish its current work first. |
| The client errors, but EMP has no matching call | Client base URL, port and local proxy rules | Confirm the client points to this EMP instance and allows direct local connections. Missing records alone do not identify the cause. |
| A call is rate limited or interrupted | Statistics call details | Read the upstream retry advice and actual attempts. After output or tool activity, check the task result before deciding to retry. |
| You want to stop using EMP routing | Tray menu or WebUI | Restore Native keeps EMP available; Quit EMP restores settings and ends the service. |

For a bug report, include EMP/Codex versions, OS, error text, the matching request
ID and the redacted report downloaded from **Statistics → System diagnostics**.
Keep API keys, sign-in credentials and full configuration files out of the report.

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

Data migration offers **Import**, **Export** and **Tunnel** from one chooser.
Import and export share the service selection view; Back keeps your entries.
For two computers on the same LAN, choose **Tunnel**. Select
services on the sender and create a 49-character code, valid for a fixed five
minutes. Paste it on the receiver, approve that receiver on the sender, then preview
and import. EMP selects the local network automatically. Sending and receiving
share one page. Credentials are encrypted in transit and each code works once.

Choose the `.emp` file, enter its export password, then click **Open migration
package**. Review the services before clicking **Import**. The preview and demo
have a Back button that retains the chosen file and password; closing discards
the dialog.

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

For the daily edit/check/E2E loop, cache retention and CI handoff, see the
[Rust development SOP](docs/development-sop.md) (Simplified Chinese).

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
