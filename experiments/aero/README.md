# aero experiment

[中文](README.zh-CN.md)

EMP **999.14.2-exp** adds **Lead & Execute** beside Refresh quotas and Aggregate routing. Configure a source-qualified orchestrator/executor pairing, then select `aero/pairing name` in Codex.

The orchestrator plans, reads and reviews; the executor implements through Codex tools. EMP owns routing, authentication and accounting. Role handoffs stay in the independent aero process, launched when needed and stopped with EMP.

```text
Codex → EMP gateway → aero → EMP gateway → role model
         configuration       routing, authentication and usage
```

## Start from the WebUI

1. Download a complete experimental native archive or macOS DMG from [prereleases](https://github.com/Killow1998/EasyMultiProvider/releases). Experimental tags start with `aero-999.14.2-exp-`. These downloads remain available; CI artifacts are only temporary staging and are removed after publication. This edition does not check for or install updates.
2. Extract the complete archive, preserving the `aero` folder beside EMP. macOS DMGs include it inside the app. No system Python installation is required. Run the experimental EMP after exiting your current instance.
3. Open **Lead & Execute → Add pairing**. The two cards select the orchestrator and executor. Model choices show their source, including aggregate routes, and each role has its own effort setting. You can save several pairings to compare them.
4. In your Codex session, select `aero/pairing name`. If an already-running client has not reloaded the model catalog, reopen its model picker or restart that client.

Settings are saved in `aero_profiles` in the normal EMP configuration. The generated sidecar configuration and SQLite conversation state live in a private `aero` directory beside that configuration. Named gateway device Keys retain their caller attribution on the inner model requests. The outer relay does not add a second usage charge. Role usage, errors and handoffs are recorded in rotating logs, capped at 100 MB.

## History, tools and media

- **Encrypted compaction** preserves the current plan, role and pending review. Both roles continue from a readable shared summary; EMP's existing history reader restores Native encrypted checkpoints.
- **Code Mode** and deferred tool discovery stay enabled. Planner file reads translate into Codex Code Mode calls; the executor uses the client's available tools.
- **Images** use EMP's image routing through the native tool or the lightweight Free-account MCP, including generation, editing and files saved by Codex.
- **Audio** input and output are retained. Voice multipart requests, SDP responses and real-time WebSocket connections pass through EMP.
- **Live text** reaches Codex as the model generates it, across role handoffs. Tool calls are checked against the current phase before Codex executes them.

## Standalone developer entry

The original CLI entry remains available with Python 3.10+, Codex CLI and ripgrep:

```sh
cp profiles.json profiles.local.json
python3 -B codex.py --config profiles.local.json --model aero/astra-sol --cwd /your/project
```

Set `upstream`, `planner.model` and `executor.model` in that private configuration to your EMP gateway and available model names. Supply the gateway Key through `EMP_GATEWAY_KEY`, or a private text file referenced by `key_file`. Relative paths resolve beside the configuration file. This launcher uses process-local Provider/catalog overrides and does not read or replace `auth.json`. State defaults to `$XDG_STATE_HOME/aero` or `~/.local/state/aero`; `--runtime` overrides it. The optional catalog template defaults to `$CODEX_HOME/models_cache.json` or `~/.codex/models_cache.json`.

For API-only testing, `python3 -B gateway.py --config profiles.local.json` serves `http://127.0.0.1:4219/v1/responses`. Use the same gateway Key and a stable `session-id` header. `--port` changes the listener. Managed GUI usage does not need this command or a second service entry.

## What happens during a task

- The planner answers simple questions directly. For implementation, it reads
  files through Codex, writes a plan and hands control to the executor.
- The executor receives Codex's tools. It hands back after completing work or
  encountering a condition requiring a revised plan. The planner checks the
  evidence and either finishes or revises the plan.
- A review is requested after 30 executor model calls and when a new user
  message arrives between requests. A second failed verification must trigger
  a handback according to the executor instructions. This last rule relies on
  the model following those instructions; the gateway enforces phase tool gates.
- Role efforts come from the profile. The Codex effort selector is disabled for
  these virtual models. There is no task-duration or handoff-count cutoff.

## Acceptance

Checks use Codex 0.169.0, an isolated Codex home and synthetic upstreams. They cover direct and Code Mode file operations, deferred MCP image generation/editing, continuation after compaction, image/audio roundtrips, first-token delivery before completion, authentication, replay, disconnect cancellation and shutdown. Extracted Windows, Linux and both macOS architecture packages also verify their bundled aero runtime without system Python.

Codex retains tool execution and approvals. Responses use HTTP/SSE; real-time media sidebands use WebSocket. EMP's individual call records retain role-model and device attribution.

## Reproduce the checks

```bash
python3 -B verify.py
python3 -B verify_lifecycle.py
python3 -B verify_features.py
python3 -B verify.py --native-codex
python3 -m pip install ruff==0.17.5
python3 -m ruff check --no-cache .
python3 -m ruff format --check .
```

Results are written to the ignored `evidence/` directory. Native verification
uses the synthetic catalog in `fixtures/`, without reading a login or private
model cache. Keep project acceptance results, user interventions, token/cache
usage and elapsed time when reporting a test; redact private code and keys.
