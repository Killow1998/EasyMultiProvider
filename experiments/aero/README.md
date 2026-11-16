# aero experiment

[中文](README.zh-CN.md)

aero is an optional Codex CLI launcher for two-model planning, execution and
review. It uses EMP's existing gateway API. It runs as a separate local process;
EMP owns provider routing, authentication and accounting, aero owns handoffs,
and Codex owns file tools and command approval.

```text
Codex CLI → aero → EMP → planner / executor models
```

This branch ships the standalone experiment, not a replacement EMP executable.
Use the current EMP release. Download `aero-experiment` from a successful
[experiment workflow](https://github.com/Killow1998/EasyMultiProvider/actions/workflows/aero-experiment.yml), unzip the
download and then `aero-experiment.zip`, or check out `dev/aero-experiment` and
open this directory. The workflow bundle is retained for one day; the branch
remains available as source.

## Start a test session

Requirements: Python 3.10+, Codex CLI, ripgrep and a running EMP. Protocol and
native tool execution are tested on Linux with Codex 0.169.0. Other platforms
are not yet accepted. The experiment uses text and direct command/file tools.

1. In EMP, open **Settings → General → Gateway connection**. Create a named
   device key for aero so its calls can be identified in usage records.
2. Copy `profiles.json` to `profiles.local.json`. Set `upstream` to EMP's API
   base URL and the role models to names available in your EMP catalog. The
   default `aero/astra-sol` uses Astra for planning/review and Sol for execution.
   `aero/sol-dsk` is a second example; its executor requires an imported DeepSeek
   service named `deepseek` with `deepseek-v4-pro` enabled, or a different model
   name entered in the profile.
3. Supply the gateway key through `EMP_GATEWAY_KEY` or save it as plain text in
   a private file and add `"key_file": "caller.key"` to the local profile.
   Relative key-file paths are resolved beside that profile. Environment
   credentials take precedence. No Codex `auth.json` is read or replaced.
4. From this directory, start a new session:

```bash
python3 -B codex.py --config profiles.local.json --model aero/astra-sol --cwd /path/to/project
```

The launcher uses a random loopback port and closes aero when Codex exits.
Provider/model overrides apply only to that Codex process. The existing global
Codex configuration and EMP model catalog stay intact. This launcher is the
test entry; aero models are not added to the ChatGPT App's global picker.

The model metadata template is read from `$CODEX_HOME/models_cache.json`, or
`~/.codex/models_cache.json`. Start your normal Codex once to create its cache,
or pass `--catalog-template /path/to/models.json` with a Codex model catalog.

State is kept in `$XDG_STATE_HOME/aero`, defaulting to
`~/.local/state/aero`. `--runtime /path/to/private-state` selects another
directory. Session history and handoffs are stored in `sessions.sqlite3`; logs
contain model/role, usage, timing and failures, with a 100 MB rotation limit.
Use a private state directory. Stable per-conversation IDs and `X-EMP-Client:
aero` attribute the underlying model calls in EMP.

For an API client, `python3 -B gateway.py --config profiles.local.json` exposes
`http://127.0.0.1:4219/v1/responses`; authenticate with the same gateway key and
send a stable `session_id` header. `--port` changes this listener. This server
is a separate test entry, not a second service to configure inside EMP.

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
