# EasyMultiProvider

English | [中文](README.zh-CN.md)

EasyMultiProvider (EMP) is a local, browser-configured model router for Codex.
It keeps the native Codex experience while adding multiple ChatGPT
subscriptions, API providers, and external models to the same model list.

The current source version is `v0.8.1`.

## Features

- Use native Codex models, additional ChatGPT subscriptions, and external API
  models from the same Codex model picker.
- Route models with readable prefixes such as `team/gpt-5.6-luna` or
  `provider/model`.
- Import multiple Codex subscription accounts and view available quota data.
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

Start EMP from the project directory:

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
  Only an account ID is required; display name and model prefix can be edited
  later. Each subscription can control which Coding Agent models appear in
  Codex.
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
