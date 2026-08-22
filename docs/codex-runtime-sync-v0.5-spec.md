# Codex Runtime Reload Specification

Status: normative for the v0.5 development line
Scope: default Codex integration when an App Server may already be running

## 1. Problem

Changing `openai_base_url` and `model_catalog_json` does not update an existing
Codex App Server. A persistent Remote Control process can therefore continue to
serve the old `/model` catalog even though `config.toml` is correct.

EMP must make the running process release the stale configuration without
assuming how that process is supervised on a particular computer.

## 2. Core decision

> EMP writes the target configuration, closes every stale Codex background
> host that can retain that configuration, and never starts a replacement.

After the stop request:

- a systemd, launchd, Windows, Desktop, container, or third-party supervisor may
  restart Codex automatically;
- a non-persistent installation remains stopped until the user next starts
  Codex normally;
- both outcomes are valid;
- EMP may observe and verify an automatic restart, but does not depend on one.

The development host happens to use a user systemd service with
`Restart=always`. That is useful acceptance evidence, not an EMP product
assumption. No systemd unit name, launcher path, restart delay, or monitor
script belongs in production routing logic.

## 3. Product invariants

- Codex owns App Server startup, persistence, Remote Control, threads, history,
  resume, compaction, WebSockets, compression, and authentication.
- EMP owns only `openai_base_url`, `model_catalog_json`, its generated catalog,
  and bounded recovery accounting.
- File state and observed runtime state are reported separately.
- A running stale App Server is never called synchronized.
- Absence of a running App Server is not an error after a successful stop; the
  next normal Codex start will load the target configuration.

## 4. Prohibited behavior

- Do not call `systemctl`, launchd, Windows Service Manager, or a host-specific
  monitor/launcher from production EMP code.
- Do not execute `codex remote-control start`, `codex app-server daemon start`,
  or any equivalent start/restart command.
- Do not terminate arbitrary Codex processes. In particular, never terminate a
  normal TUI, `codex exec`, `codex resume`, a tool subprocess, EMP itself, or a
  process whose identity/ownership cannot be proved.
- Do not use force-kill escalation. A background host that does not exit after
  a bounded graceful termination produces `stop_failed`.
- Do not assume every computer has persistent Remote Control.
- Do not disable Remote Control, WebSockets, request compression, remote
  compaction, MCP, or native authentication.
- Do not modify Codex sessions, rollout files, or history databases.
- Do not require a terminal command, wrapper executable, or separate profile.
- Do not expose raw subprocess output, credentials, local socket paths, or
  private Provider/account data.

## 5. State model

Configuration and runtime are independent.

### 5.1 Configuration state

- `native`: original Codex fields are present.
- `emp_applied`: both EMP-owned fields contain the leased EMP values.
- `conflict`: an owned field was modified outside the lease.

### 5.2 Runtime state

- `not_checked`: no live observation exists in this EMP process.
- `reload_required`: configuration or catalog changed while Codex may still be
  running.
- `stopping`: the user-confirmed graceful stop request is in progress.
- `emp_loaded`: a newly observed runtime exposes every expected EMP model.
- `native_loaded`: a newly observed runtime exposes none of the recorded EMP
  models.
- `stopped_waiting_for_start`: no runtime returned during the bounded
  observation period; the next normal Codex start will load the target.
- `stop_failed`: EMP could not prove that the stale runtime stopped.
- `verification_failed`: a runtime returned but loaded the wrong catalog.
- `unsupported`: EMP has neither a supported lifecycle response nor a safely
  identifiable background-host fallback for the installed Codex runtime.

`emp_loaded` and `native_loaded` are observations, not permanent facts. EMP
restart resets live confidence to `not_checked` or `reload_required`.

## 6. Portable background-host stop adapter

The adapter does not interact with the operating-system supervisor. It first
uses Codex's public lifecycle command, then falls back to a narrowly scoped,
cross-platform process termination only when the running host is not managed by
that lifecycle command.

For Codex 0.149, the first close surface is:

```text
codex remote-control stop --json
```

The machine-readable lifecycle result uses Codex's real camelCase schema. At
minimum, `stopped` means the pid-managed daemon stopped and `notRunning` means
that daemon was absent. Neither status proves that no foreground Remote Control
or Desktop-owned App Server exists. EMP must not invent snake_case status
values in fixtures.

The public command only owns Codex's pid-managed daemon. It can reject a
foreground `codex remote-control` or a Desktop-owned `codex app-server` with an
"app server is running but is not managed" error. That result activates the
targeted host fallback; it is not treated as either success or an unknown
failure.

After either successful lifecycle status, EMP always performs one residual host
scan. `notRunning` becomes a successful no-op only when that scan completed and
found no eligible host. `stopped` proceeds to observation only after all
eligible residual hosts have also exited. The documented unmanaged-host error
also activates this scan. Permission and malformed-output failures do not.

The residual/fallback scan uses a maintained cross-platform process API rather
than shell commands. It may inspect only processes owned by the current user,
whose effective `CODEX_HOME` matches the integration config being changed, and
may select only these semantic Codex roles:

- foreground `codex remote-control` without the `start`, `stop`, or `pair`
  lifecycle subcommands;
- `codex app-server` whose command line contains an actual listening mode such
  as `--listen`.

Codex root-level options may precede the semantic subcommand, for example
`codex -c key=value app-server --listen unix://`. Classification must parse and
skip only known root options with their required values (`-c`/`--config`,
`--enable`, `--disable`, and their `--option=value` forms) before locating
`remote-control` or `app-server`. An unknown option, a missing option value, or
an unrelated positional token before the subcommand makes the process
ambiguous and therefore ineligible.

Both a native Codex binary and its official Node launcher may appear in one
process family. A Node launcher's argv may name either `codex.js` directly or a
`bin/codex` symlink; symlink acceptance requires canonical resolution to an
installed `@openai/codex` launcher. The adapter must de-duplicate that family
and terminate the innermost runtime first, then allow the launcher to exit
naturally. A still matching launcher may receive the same graceful termination
after the bounded child wait. It must not terminate:

- `codex app-server proxy`;
- `codex app-server daemon ...` helper commands;
- schema generation or diagnostic commands;
- bare interactive Codex, `exec`, `resume`, `review`, or any other client;
- a process owned by another user;
- a process using another `CODEX_HOME`;
- a process with missing, unreadable, ambiguous, or changed identity.

The process adapter reads only the candidate's `CODEX_HOME` environment value;
it must not retain or expose the rest of the environment. An absent value means
that user's platform-default Codex home. The target home comes from the active
integration config path, not from an unrelated shell default. Paths are
normalized with platform-appropriate case semantics. Effective home identity
is revalidated with the other process fields immediately before termination.
If the effective home cannot be determined safely, the process is ineligible
and the adapter must not terminate it.

The fallback sends only the platform's normal terminate request (for example,
`SIGTERM` on POSIX or the corresponding process terminate operation on
Windows), waits for a bounded grace period, and never escalates to a hard kill.
Process identity must be revalidated immediately before termination to reduce
PID-reuse risk. Browser responses and logs may contain only normalized role,
count, and result codes, never full command lines, environment values, PIDs, or
private paths.

The adapter still exposes one operation to the rest of EMP:

```text
stop_stale_codex_hosts()
```

It never exposes `start`, `restart`, hard `kill`, or a supervisor operation.

Every subprocess invocation:

- uses an argument array without a shell;
- inherits the current `CODEX_HOME` and required environment;
- has a bounded timeout and bounded captured output;
- returns normalized result codes;
- never forwards raw output to the browser or logs.

"No running App Server" is a successful no-op only after the lifecycle result
and residual scan agree. When the lifecycle command is unsupported but no
eligible same-user background host exists, the result is `unsupported`, not a
guessed success. Permission, malformed output, ownership ambiguity, identity
races, surviving hosts, or unknown failures are not converted into success.

## 7. Apply EMP transaction

The Web action is `Enable EMP for Codex`. One confirmation performs the whole
operation. The modal warns that current Codex connections may disconnect and
active requests may be interrupted.

Order:

1. Prove that the EMP listener is ready.
2. Build and atomically write the target catalog.
3. Compute the complete visible EMP-only model slug set; reject an empty set.
4. Acquire the integration operation lock.
5. Prepare the lease and bounded runtime accounting.
6. Atomically apply `openai_base_url` and `model_catalog_json`.
7. Mark runtime `reload_required`.
8. Invoke the portable background-host stop adapter.
9. If no App Server was running, finish as
   `emp_applied + stopped_waiting_for_start`.
10. If a process was stopped, observe for at most 20 seconds without starting
    anything.
11. If a runtime returns automatically, query `model/list` and verify every
    expected visible EMP slug.
12. Finish as `emp_applied + emp_loaded`, or
    `emp_applied + stopped_waiting_for_start` when nothing returns.

Configuration is written before the stop request. This guarantees that any
supervisor restart reads the new values and avoids racing a restart delay.

## 8. Restore native transaction

The Web action is `Restore native Codex`. One confirmation performs both
durable restoration and stale-runtime release.

Order:

1. Retain the visible EMP-only slugs from the active recovery record.
2. Acquire the integration operation lock.
3. Restore only the two leased fields with compare-and-restore.
4. Mark runtime `reload_required`.
5. Invoke the same portable background-host stop adapter.
6. If no App Server was running, finish as
   `native + stopped_waiting_for_start`.
7. If a runtime returns automatically within 20 seconds, query `model/list` and
   verify that none of the retained EMP-only slugs remain.
8. Finish as `native + native_loaded`, or
   `native + stopped_waiting_for_start` when nothing returns.

Native fields are not changed back to EMP merely because a live verification
could not be performed. The next normal Codex start must converge toward the
durable native target.

## 9. Stop and verification failures

- If catalog/config mutation fails, do not issue a stop request.
- If the stop adapter proves that no runtime exists, keep the target
  configuration and report `stopped_waiting_for_start`.
- If stop fails for an unknown reason, keep the target configuration, report
  `stop_failed`, and do not pretend the old runtime reloaded it.
- EMP may make one bounded retry only for a documented transient control-socket
  failure.
- EMP never starts another process as rollback.
- If an automatically restarted runtime has only some expected EMP models,
  report `verification_failed`; partial overlap is not success.
- The user can retry the same one-click `Reload Codex` action. They are never
  told to copy a terminal restart command.

## 10. Observing an automatic or later start

After a successful stop, EMP polls the Codex App Server control surface for at
most 20 seconds. Polling does not start Codex.

If Codex reappears:

- connect through `codex app-server proxy`;
- perform the documented JSONL initialization;
- call `model/list` with bounded pagination;
- derive response parsing from generated App Server schema or a sanitized real
  fixture, not only a hand-written FakeRunner object;
- verify the complete target model condition.

If Codex does not reappear, the operation remains successful with
`stopped_waiting_for_start`. On later Web status refresh, EMP may perform a
read-only observation and upgrade the state to `emp_loaded` or `native_loaded`.

## 11. Catalog changes while EMP is applied

Model import, visibility changes, account changes, and catalog refresh:

- atomically regenerate the catalog;
- persist the new expected visible EMP slug set;
- mark runtime `reload_required`;
- show a top-level `Apply model changes to Codex` action;
- never stop Codex automatically during an ordinary save.

The user-confirmed action writes no new configuration values; it only asks the
current stale App Server to stop and observes any automatic restart.

## 12. Recovery accounting

EMP may persist only the bounded data needed to explain and finish the target:

- schema/version;
- target (`emp` or `native`);
- lease/config relation;
- expected visible EMP model slugs;
- runtime state and last observation timestamp;
- bounded error code.

It must not persist prompts, responses, tool results, threads, credentials,
tokens, command output, process lists, or environment dumps. The record is
private and atomically written.

## 13. Offline doctor and restore

Default `doctor` remains read-only and offline. It does not run Codex commands,
access the App Server socket, or open a listener.

It reports durable configuration and last-known runtime separately. A last-known
runtime value is labelled as stale/offline and is never refreshed implicitly.

Offline `restore` restores native fields but cannot prove the current runtime
reloaded them. It reports `reload_required` or
`stopped_waiting_for_start`, never `native_loaded` without a live observation.

## 14. HTTP and Web behavior

`GET /api/integration` returns separate `configuration` and `runtime` objects.
Raw subprocess text is not part of the response.

Mutations:

- `POST /api/integration/enable` with `{ "confirm_reload": true }` performs the
  complete apply-and-stop transaction.
- `POST /api/integration/restore` with `{ "confirm_reload": true }` performs the
  complete restore-and-stop transaction.
- `POST /api/integration/reload` with `{ "confirm_reload": true }` handles an
  already-applied catalog change.

Missing confirmation returns `409 confirmation_required` before mutation.

The Web UI:

- uses one custom modal for each operation;
- displays preparing, saving, closing Codex, observing, and done states;
- shows success only after the target config was written and stale runtime was
  stopped or proven absent;
- distinguishes “loaded now” from “will load next time Codex starts”;
- never displays a terminal restart command as the normal solution;
- keeps errors next to the operation and in a top-level toast.

## 15. Large Provider model picker

- Search display name and upstream ID case-insensitively.
- `Select filtered` and `Clear filtered` affect only visible results.
- Filtering never changes hidden selections.
- Display selected/total and selected-filtered/filtered counts.
- Existing imported models start selected.
- Newly discovered models start unselected.
- First import of a large list therefore starts with none selected.
- The server preserves manual context, visibility, and capability overrides for
  retained models.
- Deselecting an imported model hides it from the generated catalog but does
  not delete its manual overrides or credentials.

## 16. Required tests

### 16.1 Stop adapter

- Parses real Codex camelCase lifecycle fixtures for `stopped` and
  `notRunning`.
- Uses the Codex-owned graceful-stop command first.
- Falls back only for the documented unmanaged-host result.
- Performs a residual scan after both `stopped` and `notRunning`; a foreground
  host found after `notRunning` must still be terminated.
- Classifies native and Node-launched foreground Remote Control and listening
  App Server process families.
- Covers both direct `codex.js` and canonically resolved `bin/codex` Node
  launcher argv shapes.
- Covers native and Node launcher forms with known root-level Codex options
  before the host subcommand, while rejecting malformed or unknown prefixes.
- Revalidates same-user identity before graceful termination.
- Selects and revalidates only the active integration `CODEX_HOME`; a controlled
  process using a different home is never terminated.
- Never selects TUI, exec, resume, proxy, daemon helper, schema generator,
  another user's process, or an ambiguous process.
- Never uses a hard-kill path when a host ignores graceful termination.
- Never invokes start, restart, systemctl, launchd, service manager, shell PID
  utilities, or a hard-kill API.
- No-running-runtime is a successful no-op.
- Permission/unknown failures remain failures.
- Missing confirmation executes no mutation or stop command.

### 16.2 Transaction tests

- Apply writes target config before requesting stop.
- Restore writes native config before requesting stop.
- No persistent supervisor results in `stopped_waiting_for_start`, not failure.
- Simulated automatic restart reaches target-specific loaded state.
- Partial model overlap fails.
- Stop failure keeps durable target config and reports stale runtime.
- Crash/restart across every durable phase gives deterministic doctor output.
- Concurrent apply/restore/reload operations serialize.

### 16.3 Process-level fake Codex

Use a temporary executable implementing real argument order and JSONL. It must
simulate:

- no running App Server;
- graceful stop;
- no restart;
- delayed external/supervisor restart;
- complete and partial `model/list` responses.

Use controlled child processes for the targeted-host fallback. Tests must cover
native and Node-wrapper-shaped families, same-user filtering, PID identity
revalidation, graceful timeout without hard kill, and every excluded client
role. Tests must not enumerate or terminate the user's real Codex processes.

No test may run real stop/start commands against the user's Codex.

### 16.4 Web behavior

A JavaScript DOM harness must exercise confirmation, progress, success/failure,
loaded-versus-next-start wording, search, filtered bulk selection, and counts.
String-presence assertions alone are insufficient.

## 17. Acceptance corrections from the first live default-Codex run

### 17.1 Configuration state and runtime state are independent

The browser must never label a transport, authentication, or status-fetch
failure as a configuration `conflict`. `Conflict` is reserved for a current,
proved mismatch between the two leased fields and both their original and
applied values.

An active lease whose current managed fields equal the applied values remains
`emp_applied`, including when runtime observation is
`verification_failed`, `stop_failed`, `unsupported`, or temporarily
unavailable. Those runtime results remain visible as a separate status and
action.

An enable or restore transaction that successfully writes its target
configuration returns a successful HTTP response even when the independent
runtime observation ends in `verification_failed`, `stop_failed`, or
`unsupported`. The response still carries `action_required` and the runtime
detail. HTTP `409` is reserved for an actual configuration/lease conflict or a
rejected transaction, not an incomplete runtime observation.

If `/api/integration` cannot be loaded, the browser renders `Unavailable` and
the request error. It must not invent a configuration state. A previously
rendered successful configuration state may be retained, but it must be marked
stale.

A startup target mismatch is ephemeral evidence. A later successful enable or
startup reconciliation that proves the active listener and catalog equal the
current lease clears that mismatch before the next integration summary.

Required regressions:

- active/applied configuration plus runtime verification failure renders
  `EMP applied`, not `Conflict`;
- an integration-status request failure renders `Unavailable`, not `Conflict`;
- a stale startup mismatch is cleared after a successful matching enable or
  reconciliation;
- a real third-party edit to either leased field still renders `Conflict` and
  is never overwritten.

### 17.2 Native models use the current Codex login without Provider setup

Native model entries merged from `native_catalog_path` are not display-only.
When Codex requests one of those unprefixed native slugs, EMP routes it to
`codex_base_url` with the incoming, already validated Codex login headers even
when no explicit `forward` Provider exists.

This implicit route:

- exists only in memory and is never persisted or shown as a user Provider;
- is limited to visible, API-supported slugs present in the native catalog;
- uses the Responses protocol and native Responses compact endpoint;
- forwards only the existing allowlist of login headers, including
  `Authorization` and `ChatGPT-Account-ID`;
- never reads an imported account credential and never stores the incoming
  login;
- does not make an unknown unprefixed model routable;
- does not change explicit external routes or prefixed imported-account routes.

An explicitly configured unique forward Provider keeps its existing behavior.
The implicit route is the zero-configuration fallback for known native slugs,
so the default Codex subscription and EMP models coexist immediately after
`Enable Default Codex`.

Required regressions:

- a known native slug routes to `codex_base_url` with `auth_mode=forward` when
  the Provider list contains no forward Provider;
- the current request's login and account headers reach the native upstream;
- native compaction uses the native compact endpoint;
- an unknown unprefixed slug still returns 404;
- configured external routes and prefixed imported-account routes retain
  precedence;
- the Web Provider list does not gain a synthetic Provider row.

### 17.3 Model names expose known usable context

Generated catalog display names append a compact context label when EMP has a
known positive context limit. Native models apply
`effective_context_window_percent` before formatting, so a 272,000-token model
with a 95 percent effective window is displayed as `[258K]`. Imported account
aliases inherit that label. External models use their persisted
`context_window`; unknown limits have no label and must not inherit the native
catalog template's context fields.

Codex clients do not render the same catalog fields uniformly. The Desktop App
uses `display_name`, while the CLI TUI model picker uses the stable model slug
as its primary label. EMP therefore also appends `Context <size>` to the model
description so both clients expose the same information without changing the
routing slug.

Native models with `visibility = "hide"` remain absent from user-facing model
pickers and subscription aliases, but visibility does not disable routing.
Internal Codex service requests such as `codex-auto-review` must continue to
use the implicit current-login route when `supported_in_api` is true.

## 18. Live acceptance evidence

The current Linux host with a persistent Remote Control owner has verified:

1. One-click default-Codex enable closes the stale runtime and converges after
   the external owner starts Codex again.
2. `/model` contains the current native login, imported subscription aliases,
   and external Provider models.
3. Unprefixed current-login and prefixed imported-subscription requests both
   complete successfully.
4. The Desktop App loads the combined catalog and completes external-model
   text and image requests.
5. Native resume history remains visible through ordinary `codex resume`.
6. Runtime verification warnings no longer create a false configuration
   `Conflict`.

Non-persistent lifecycle behavior remains covered by controlled integration
tests because exercising it against the user's persistent runtime would not
represent that deployment. Windows and macOS remain cross-platform hardening,
not v0.5 architecture changes.

## 19. Definition of done

- EMP contains no supervisor-specific production path.
- EMP never starts or restarts Codex.
- EMP terminates only verified same-user Codex background hosts after explicit
  confirmation and never hard-kills them.
- The user performs no terminal command.
- Persistent and non-persistent machines both converge correctly.
- Initial enable and restore each require one confirmation, not a second Sync
  step.
- Complete-set model verification, focused tests, full regression, JavaScript
  behavior tests, compilation, lock consistency, and diff checks pass.
- The current Linux CLI/TUI/Desktop and persistent-RC acceptance evidence above
  is complete; cross-platform checks remain separately tracked.

## 20. Required revision of the first LunaMax draft

The first draft is not accepted because it:

- branches into managed daemon, unmanaged RC, and host supervisor start/restart
  behavior that EMP does not own;
- calls start/restart commands instead of stopping only;
- separates initial enable/restore from a mandatory second Sync action;
- stores runtime confidence only in process memory;
- performs one immediate readiness query;
- accepts any model overlap instead of the complete expected set;
- has incomplete stale-runtime failure semantics;
- tests Web text presence rather than behavior.

LunaMax must implement this document exactly. Any requested deviation must be
raised before code modification.
