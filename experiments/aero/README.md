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
4. Start a new Codex session and select `aero/pairing name`. If an already-running client has not reloaded the model catalog, reopen its model picker or restart that client.

Settings are saved in `aero_profiles` in the normal EMP configuration. The generated sidecar configuration and SQLite conversation state live in a private `aero` directory beside that configuration. Named gateway device Keys retain their caller attribution on the inner model requests. The outer relay does not add a second usage charge. Role usage, errors and handoffs are recorded in rotating logs, capped at 100 MB.

Linux native tool execution is tested with Codex 0.169.0 and synthetic upstreams. The package workflow verifies extracted Windows, Linux and both macOS architecture archives without system Python on the backend PATH. Desktop client behavior on other platforms still needs user acceptance. This experiment uses text and direct file/command tools.

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

## Test scope and known gaps

The CI checks use synthetic upstreams and an isolated Codex home. They cover
role handoffs, read/write/verify through native Codex tools, authentication,
retry replay, persistence, client disconnects and shutdown. They make no paid
model calls. Passing proves protocol behavior, not that two models are faster,
cheaper or more capable than one.

Current limits:

- Full visible HTTP Responses history only. WebSocket, `previous_response_id`,
  item references and encrypted compaction are rejected. Changes to the saved
  conversation prefix are rejected rather than discarding handoff history.
- Long-session compaction and interruption during a live model call are not
  accepted yet. Use a new, bounded project session for this experiment.
- The launcher advertises text input and direct tools. Code Mode, built-in
  search, image/audio generation and nested subagents are outside this version.
- Responses are buffered until a model/handback segment finishes. SSE heartbeats
  keep the connection alive; intermediate tokens are not streamed to Codex.
- Returned aggregate counters sum reported usage; missing role usage is not a
  complete total. Use EMP's individual call records for accounting.

## Reproduce the checks

```bash
python3 -B verify.py
python3 -B verify_lifecycle.py
python3 -B verify.py --native-codex
python3 -m pip install ruff==0.17.5
python3 -m ruff check --no-cache .
python3 -m ruff format --check .
```

Results are written to the ignored `evidence/` directory. Native verification
uses the synthetic catalog in `fixtures/`, without reading a login or private
model cache. Keep project acceptance results, user interventions, token/cache
usage and elapsed time when reporting a test; redact private code and keys.
