# Overnight CLI Track A runbook

This runbook describes the bounded supervisor used for the 2026-08-19 Luna Max
run. It keeps the native Codex subscription as the control channel and uses
`--profile emp` only for the disposable or live SUT child CLI.

## Reproduce

From the repository root:

```bash
./.venv/bin/python tools/overnight_cli.py \
  --run-id 20260819-night1 \
  --resume 20260819-night1 \
  --live-canaries \
  --max-hours 8 \
  --soak-hours 4
```

The run is bounded by the supervisor. It writes `result.json`, `summary.md`,
JSONL controller events, a checkpoint, frozen hashes, and one redacted verifier
directory per case under `artifacts/overnight/<run-id>/`.

## Recovery

If the controller is interrupted, rerun with the same `--run-id` and
`--resume <run-id>`. The supervisor verifies the frozen supervisor/manifest/
oracle hashes before continuing. A hash mismatch is a hard stop; do not edit
the frozen files to make a case pass.

Only child PIDs recorded by the supervisor are terminated. The existing 4200
service is never killed or reconfigured. If its `/healthz` probe is unavailable,
live cases are recorded as environment-blocked and local mock/GLM coverage still
completes.

## Artifact safety

Runtime stdout/stderr and final messages are redacted before they are written.
The supervisor does not read or copy `~/.codex/auth.json`; live CLI invocations
read the user's profile through Codex itself. Disposable mock credentials are
created only in a temporary `CODEX_HOME` and are removed at stack teardown.

Do not commit `artifacts/`. Do not use `--last` to resume a case: use the
explicit `thread_id` captured from `thread.started`.

## Desktop Track B

The ChatGPT desktop client remains `WAITING_FOR_USER`. The supervisor does not
operate the UI, modify application packages, install certificates, or alter
system proxy/DNS settings.
