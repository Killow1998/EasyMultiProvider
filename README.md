# EasyMultiProvider

English | [中文](README.zh-CN.md)

EasyMultiProvider (EMP) is a local, browser-configured model router for Codex.
It keeps the native Codex experience while adding multiple ChatGPT
subscriptions, API providers, and external models to the same model list.

The current source version is `v0.9.3`.

## Features

- Use native Codex models, additional ChatGPT subscriptions, and external API
  models from the same Codex model picker.
- Route models with readable prefixes such as `team/gpt-5.6-luna` or
  `provider/model`.
- Import multiple Codex subscription accounts and refresh available quota data.
- Add official or custom providers through the Web UI.
- Discover provider models, select which ones to import, edit context windows,
  test them, and hide unused entries.
- Preserve text, image, reasoning, and structured tool capabilities when the
  provider reports or supports them.
- Let Codex delegate a native child task to an external catalog model by its
  existing model slug; Codex continues to own the child task and permissions.
- Keep credentials encrypted on the local machine.
- Keep a private, bounded diagnostic journal for later troubleshooting.
- Export and import password-protected `.emp` migration files.
- Preserve native Codex sessions, `resume`, WebSockets, compression, and MCP.
- Continue compacted tasks when switching between the current login, imported
  subscriptions, and external models, using Codex-owned visible history only.
- Let external models use Codex standalone web search through an explicitly
  selected ChatGPT Subscription without exposing Provider credentials.

## Install

EMP requires either the Codex-managed runtime in the active Codex home or a
standalone Codex CLI available as `codex`. EMP does not bundle
or replace either runtime. When both exist, App integration uses the managed
runtime and the Web UI reports a different standalone `PATH` CLI separately.

EMP supports Codex CLI `0.149.x` through `0.151.x`; `0.151.x` is recommended.
The Web UI shows the installed version and marks newer versions as not yet
verified or older versions as unsupported.

### Prebuilt packages

Download reviewed builds from
[GitHub Releases](https://github.com/Killow1998/EasyMultiProvider/releases). The
[Package workflow](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/package.yml)
builds and smoke-tests these native artifacts before a release is published:

| Platform | Artifact |
| --- | --- |
| Windows x64 | branded standalone `.exe` |
| Ubuntu 22.04+ x64 | `.tar.gz` and desktop-enabled `.deb` |
| macOS Intel | `.app` inside a `.dmg` |
| macOS Apple Silicon | `.app` inside a `.dmg` |

For the simplest desktop launch:

- **Windows:** double-click `easy-multi-provider.exe`.
- **Linux:** install the `.deb`, then open **EasyMultiProvider** from the
  application menu.
- **macOS:** open the DMG, drag **EasyMultiProvider** to Applications, then
  double-click it.

EMP opens the authenticated Web UI and keeps a visible terminal window for
status and logs. `EasyMultiProvider listening on ...` means startup succeeded.
Keep that terminal open while using EMP. Press `Ctrl+C` for a clean stop, or
close the terminal to terminate the process; after a clean stop it prints
`EasyMultiProvider stopped.`

Desktop launch stores configuration in the normal per-user directory:

- Windows: `%LOCALAPPDATA%\EasyMultiProvider\config.json`
- macOS: `~/Library/Application Support/EasyMultiProvider/config.json`
- Linux: `$XDG_CONFIG_HOME/easy-multi-provider/config.json`, or
  `~/.config/easy-multi-provider/config.json`

For command-line use, the explicit service command remains available. With the
downloaded Windows executable, use PowerShell:

```powershell
.\easy-multi-provider.exe --version
.\easy-multi-provider.exe serve --config config.json
```

After extracting the Linux `.tar.gz` or installing the `.deb`:

```bash
./easy-multi-provider --version
./easy-multi-provider serve --config config.json
```

The `.deb` installs the same command into `PATH`, so omit `./` after installing
it. The Windows executable and Linux archive command also enter the
browser-opening desktop mode when run without arguments.

The current macOS workflow artifacts are unsigned development builds. Public
distribution still requires Apple Developer ID signing and notarization.

### Install from source

Install Git and [`uv`](https://docs.astral.sh/uv/getting-started/installation/),
then clone EMP:

```bash
git clone https://github.com/Killow1998/EasyMultiProvider.git
cd EasyMultiProvider
uv sync
```

`uv` manages Python, the virtual environment, and locked dependencies. No
separate Python version manager is required.

## Quick Start

Start a packaged EMP executable explicitly on Linux or macOS with:

```bash
easy-multi-provider serve --config config.json
```

When running from a source checkout, use:

```bash
uv run python -m easy_multi_provider serve --config config.json
```

On first start, EMP automatically creates a private local encryption key. No
environment variable or manual key-generation command is required.

The terminal prints a one-use browser URL. Open it and:

1. Import a Codex subscription account or add an API Provider.
2. Pull the Provider model list and import the models you want.
3. Adjust model visibility or context windows when needed.
4. Click **Enable Default Codex**.
5. Start Codex normally and select a model from `/model` or the App model menu.

EMP listens on `http://127.0.0.1:4200` by default. Use `--port` only when that
port is already occupied.

Each start also prints `Diagnostic log: ...`. EMP stores structured runtime
metadata in that file so later bugs can be diagnosed without reconstructing
the session from memory. Managed logs are kept under `state/logs/`; the oldest
parts are removed automatically when they exceed 10 MiB in total. Prompts,
responses, tool payloads, HTTP bodies, headers, cookies, and credentials are
not recorded.

## Web UI

- **Accounts** imports `auth.json` and backup files such as `auth.json.bk1`.
  Only an account ID is required. The display name / display prefix can be edited
  later and supports emoji; the actual route prefix stays unchanged.
  **Refresh** performs a live quota query and saves the new snapshot.
  While EMP is running it samples quota every five minutes and shows local
  trends for one hour, one day, one week, or up to 15 days. This history stores
  quota metrics only, never credentials. Each subscription can control which
  Coding Agent models appear in Codex.
- **Providers** offers presets for supported official services and a custom
  Provider form for Base URL and API key endpoints.
- **Models** discovers upstream models and lets you import, test, edit, hide,
  or remove them. Models stay grouped by Provider.
- **Codex integration** applies the current EMP catalog to the default Codex
  configuration and can restore native Codex routing from the same page.

EMP automatically detects proxy settings from its launch environment or the
operating system.

## Local Security

EMP binds its management UI to the local machine by default. Subscription
credentials and Provider API keys are encrypted locally and are not returned
to the browser after saving. Local configuration, encrypted state, and
generated catalogs are excluded from Git.
