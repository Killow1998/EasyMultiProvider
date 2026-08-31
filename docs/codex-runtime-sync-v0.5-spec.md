# Shared Codex Runtime Observation Contract

Status: current normative contract (supersedes the former stop-only v0.5
runtime contract)

Scope: default Codex integration when a persistent App Server may already be
serving the App, TUI, Remote Control, or other clients.

## 1. Ownership boundary

The Codex App Server is owned by its launcher, service manager, or user. EMP
does not own its lifecycle.

Enable, restore, catalog refresh, runtime check, startup reconciliation, and
offline recovery must not:

- invoke `remote-control stop`, start, restart, daemon, or equivalent lifecycle
  commands;
- terminate or signal Codex, TUI, launcher, or App Server processes;
- start a second App Server when the shared listener is unavailable;
- modify Codex state databases, lock files, enrollment, credentials, sessions,
  or rollout history.

EMP owns only its generated model catalog, its integration lease, and the
`openai_base_url` / `model_catalog_json` fields while the lease is active.

## 2. File state and runtime observation

Saved integration files and the live backend are independent states.

- `configuration.state` reports what EMP has durably applied or restored.
- `runtime.state` reports only the latest bounded observation of the shared
  backend.
- A saved file change never implies that a running backend hot-loaded it.
- A prior-process observation is stale accounting until a new live probe
  succeeds.

The UI must not call either state “synchronized” merely because the operation
returned successfully.

## 3. Read-only protocol

The default control socket is:

```text
$CODEX_HOME/app-server-control/app-server-control.sock
```

On a supported platform, EMP:

1. connects to the existing Unix socket;
2. performs an HTTP WebSocket Upgrade for `ws://localhost/` without WebSocket
   compression;
3. sends `initialize` as a JSON-RPC text frame;
4. waits for the matching response;
5. sends `initialized`;
6. sends paginated `model/list` requests and waits for each matching response;
7. closes only its own probe connection.

`codex app-server proxy` is a raw stdio byte tunnel, not a JSONL RPC endpoint,
and is not used for catalog observation.

The probe is bounded by message size, response count, page count, cursor
validation, model count, and timeout. It ignores unrelated notifications while
waiting for the matching request ID.

## 4. Runtime states

- `not_checked`: no live observation in the current EMP process.
- `reload_required`: integration files are saved, but the observed model IDs do
  not match the target. The backend owner must restart Codex in a safe
  maintenance window before checking again.
- `emp_loaded`: the shared backend exposes every expected EMP model ID.
- `native_loaded`: the shared backend exposes none of the recorded EMP model
  IDs.
- `stopped_waiting_for_start`: the shared listener is absent or unavailable;
  the backend owner must start it.
- `verification_failed`: the listener answered, but the read-only query failed
  or returned malformed data.
- `unsupported`: the platform or installed runtime cannot perform the supported
  read-only probe.

`stopping` and `stop_failed` are legacy recovery values only. Current
enable/restore/reload flows do not produce them by controlling a process.

`emp_loaded` and `native_loaded` verify only the observed model ID set. They do
not prove that endpoint changes, model rename metadata, authentication,
enrollment, or any other startup setting were hot-loaded.

## 5. Operations

### Enable

1. Validate that the generated EMP catalog has visible models.
2. Write the catalog.
3. Apply the two leased Codex config fields.
4. Mark the runtime target as `reload_required`.
5. Perform at most one read-only live observation.
6. Return configuration and runtime objects separately.

### Restore

1. Restore only the fields still owned by the EMP lease.
2. Restore the optional search integration files.
3. Mark the native runtime target as `reload_required`.
4. Perform at most one read-only live observation.
5. Return configuration and runtime objects separately.

### Catalog refresh

Write the generated catalog and mark `reload_required`. Do not touch the shared
backend. A later “Check loaded catalog” action may perform a read-only probe.

### Reload API / Check loaded catalog

The existing reload API is retained for compatibility, but “reload” now means
only “observe the catalog currently loaded by the shared backend.” It never
reloads or controls the process.

## 6. Failure behavior

- Missing/refused listener: report unavailable and wait for the owner; do not
  start a replacement.
- Target model ID mismatch: report `reload_required`; do not claim success and
  do not repeatedly poll for a restart.
- Permission, Upgrade, malformed JSON-RPC, invalid pagination, timeout, or
  unsupported platform: fail closed with the corresponding runtime state.
- A failed runtime observation does not roll back an otherwise successful file
  transaction, and a successful file transaction does not hide the runtime
  failure.

No runtime observation logs request content, credentials, model responses, or
session history.

## 7. Verification boundary

Required regression coverage:

- enable, restore, catalog refresh, and reload do not invoke lifecycle commands
  or process termination;
- a real Unix WebSocket Upgrade accepts `initialize` / `initialized` /
  paginated `model/list` text frames;
- a missing listener reports waiting for its owner and does not start one;
- a model ID mismatch reports `reload_required` and a safe-maintenance restart
  instruction;
- configuration and runtime status remain separate in API and UI output.

The real shared-listener probe is verified on Linux for this change. macOS and
Windows behavior is preserved behind the platform socket capability boundary
but is not claimed as live-verified here.
