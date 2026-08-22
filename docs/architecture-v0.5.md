# EasyMultiProvider v0.5 Architecture

Persistent App Server and Remote Control synchronization is specified
normatively in `docs/codex-runtime-sync-v0.5-spec.md`. Configuration lease state
and the running Codex catalog are separate axes; neither may stand in for the
other.

Status: implemented and accepted on the current Linux host
Scope: v0.5 development line

## Decision

EasyMultiProvider is a local **Codex Runtime Control Plane**. It manages how a
Codex request reaches a subscription, official API, compatible gateway, or
local model. It does not become a second Codex client and does not own
conversation history.

The central ownership rule is:

> Codex is the only source of truth for threads and history. EMP owns runtime
> integration, routing, capabilities, diagnostics, and context safety.

This rule lets EMP improve multi-provider behavior without duplicating resume,
fork, compaction, deletion, synchronization, or crash-recovery semantics for
conversation content.

## Product boundary

| Responsibility | Owner |
| --- | --- |
| Thread and history persistence | Codex |
| Resume, fork, archive, and native compaction | Codex |
| CLI, TUI, Desktop App, and SDK user experience | Codex |
| Temporary default-Codex integration and recovery | EMP |
| Subscription accounts and encrypted Provider credentials | EMP |
| Model catalog, prefixes, visibility, and routing | EMP |
| Protocol translation and transport adaptation | EMP |
| Provider/model capability knowledge | EMP |
| Context estimation, safe limits, and preflight decisions | EMP |
| Model execution and authoritative usage/errors | Upstream Provider |

EMP must preserve Codex's native behavior rather than globally disabling it.
In particular, integration must not turn off WebSockets, request-body
compression, remote compaction, MCP, or the native resume picker to make a
Provider easier to support.

## Non-goals

v0.5 does not include:

- an EMP-owned thread or canonical-history database;
- persistent per-model copies of conversation context;
- silent history trimming, summarization, or compaction;
- direct edits to Codex databases, rollout files, or other private persistence;
- browser automation or private Desktop App injection;
- a general LiteLLM replacement or an enterprise gateway administration plane;
- automatic cross-model Context Migration.

Context Migration remains a later, explicit experiment. It is not a reason to
expand v0.5 history ownership.

## Runtime request path

```text
Codex CLI / TUI / Desktop App / SDK
                  |
                  v
       Default Codex integration
                  |
                  v
        EMP Responses boundary
                  |
        +---------+----------+
        |                    |
        v                    v
 Capability and route   Request observation
        |                    |
        +---------+----------+
                  v
         Protocol translation
                  |
                  v
           Context Guard
                  |
                  v
   Subscription / API / Gateway / Local model
```

The Context Guard is deliberately placed after translation. Its decision must
use the payload that the target model will actually receive, including
translated tool definitions and schemas, rather than only the incoming Codex
shape.

## State model

EMP separates durable recovery data, durable capability data, connection-local
accounting, and forbidden history data.

### Durable local state

EMP may persist:

- encrypted subscription credentials and Provider API keys;
- user configuration, model visibility, prefixes, and manual capability
  overrides;
- an integration lease containing only the values needed to compare and restore
  EMP-owned Codex fields;
- capability sources, confidence, timestamps, and endpoint/model fingerprints;
- numeric calibration such as largest known successful input, smallest explicit
  context failure, and last reported usage;
- privacy-safe operational counters and error classes.

Capability and calibration keys must include the Provider endpoint fingerprint,
upstream model, protocol, and deployment identity. A result learned from one
gateway must not silently constrain another endpoint that happens to expose the
same model name.

### Connection-local volatile state

For an active Responses WebSocket connection, EMP may hold:

- the previous response identifier needed by that connection;
- the model and Provider fingerprint;
- estimated cumulative input usage;
- last successful upstream usage;
- bounded replay/accounting metadata needed to interpret later increments.

This state is numeric or opaque accounting state, not a reconstructed transcript.
It is memory-only, bounded, scoped to one connection, and discarded when the
connection closes.

### Forbidden persisted state

EMP must not persist:

- user prompts or system instructions;
- assistant content or reasoning;
- tool arguments, tool output, or full tool-call history;
- complete translated payloads;
- compacted conversation checkpoints as a substitute for Codex history;
- an EMP-owned resume or fork graph.

Opaque thread or response identifiers alone do not turn accounting into history,
but v0.5 should not persist them unless a concrete recovery requirement exists.

## Default Codex integration

The v0.5 product path uses the default Codex configuration. It must not require
an `emp-resume` wrapper, a separate session directory, or `--profile emp` for
ordinary use. This preserves the native session identity and therefore the
normal resume list.

Codex loads session-static model settings when its App Server starts, so file
state and runtime state are separate. EMP uses the runtime states defined in
`docs/codex-runtime-sync-v0.5-spec.md`, including `reload_required`,
`stopping`, `emp_loaded`, `native_loaded`, `stopped_waiting_for_start`, and
explicit failure boundaries. A loaded state from a prior EMP process is
last-known accounting, not fresh verification.

Integration is an explicit user action and occurs only after the local listener
is ready. First installation must not silently redirect Codex.

### Owned fields and paths

EMP owns only these root fields while integration is active:

- `openai_base_url`;
- `model_catalog_json`.

The path contract is:

- Codex home: non-empty `CODEX_HOME`, otherwise the user's `.codex` directory;
- Codex config: `$CODEX_HOME/config.toml`;
- integration state:
  `$CODEX_HOME/easy-multi-provider/integration/`;
- lease and lock: `lease.json` and `lease.lock` inside that state directory.

An explicit state-directory override may exist for tests and advanced recovery,
but normal `doctor` and `restore` behavior must not depend on the invocation
directory or a source checkout.

### Lease transaction

The lease is a small recovery record, not a second configuration file. It stores
the original and applied state of the two owned fields plus schema, lease,
instance, process, status, and timestamp metadata.

Activation follows this order:

1. Prove that the EMP listener accepts requests.
2. Acquire the integration lock.
3. Read and parse the current TOML without discarding unrelated formatting.
4. Write a `prepared` lease atomically.
5. Apply the two fields atomically.
6. Mark the lease `active`.

Normal restoration follows this order:

1. Acquire the same lock.
2. Compare current fields with the lease's applied values.
3. Mark the lease `restoring`.
4. Restore only values that EMP still owns.
5. Mark the lease `restored`.

### Portable stop-only runtime synchronization

Initial enable and Web restore are each one confirmed transaction: write the
target catalog/configuration, persist `reload_required`, request only
`codex remote-control stop --json`, then observe for at most 20 seconds. The
real lifecycle values are `stopped` and `notRunning`. Only the documented
"App Server is running but is not managed" error activates the fallback from
an error response; both successful values also require one residual psutil scan
because they describe only the pid-managed daemon. The scan revalidates creation
time, owner, executable, semantic argv, and the effective Codex home. The target
home is derived from the active integration manager's config path, never an
unrelated shell default. Owner/executable/argv classification happens before
environment access; only a supported candidate's `CODEX_HOME` is read, with an
absent value mapped to that user's platform default. Different, unreadable,
ambiguous, or changed homes are ineligible. The normalized effective home is
used only for ephemeral revalidation and is never logged, returned, or persisted.
The scan then gracefully terminates only same-user foreground Remote Control or
listening App Server process families, native child before official Node
launcher. Node `bin/codex` shims are accepted only when canonical resolution
reaches the installed `@openai/codex/bin/codex.js`. The scan excludes TUI, exec,
resume, review, proxy, daemon, schema helpers, other users, lookalikes, and
ambiguous identities. It has no hard-kill path and exposes no PID, argv, path,
or environment data.

Before identifying `remote-control` or `app-server`, the classifier consumes
only the known value-taking root options `-c`/`--config`, `--enable`, and
`--disable`, including supported long equals forms. Unknown options, missing
values, and unrelated positional prefixes reject the identity immediately;
classification never scans loosely for a later command token.

EMP never owns startup, restart, or service-manager lifecycle. If an external
owner starts Codex again, EMP queries paginated `model/list` and requires the
entire expected EMP set for enable, or the absence of every retained EMP slug
for native restore. Partial overlap is `verification_failed`.

No runtime is a successful `stopped_waiting_for_start` result. Permission,
protocol, malformed-output, and unsupported-command errors remain explicit.
An unmanaged fallback with no safely identifiable host is `unsupported`; an
identity race, access denial, or graceful timeout is `stop_failed`.
Catalog refresh records `reload_required`; only a later confirmed Reload action
requests another graceful stop. WebSockets, request compression, remote
compaction, and MCP remain native Codex capabilities and are not disabled.

`doctor` and offline `restore` do not run Codex commands. They report durable
configuration and stale/offline last-known runtime accounting separately;
offline restore never claims `native_loaded`.

If the original config did not exist, restoration removes the new file only
when nothing except EMP-owned fields remains. Comments, tables, or values added
by the user cause the file to be preserved.

### Crash reconciliation

On `doctor`, `restore`, or the next EMP startup, current managed fields are
classified against the lease:

- **original**: no active redirect remains;
- **applied**: EMP values remain and may be restored or re-adopted;
- **mixed**: only part of the expected state matches;
- **other**: the user or another process changed an owned value.

Re-adoption is allowed only after the new EMP listener is ready. `mixed` and
`other` are conflicts: EMP reports them and does not overwrite user state.
Normal exit uses the same compare-and-restore rule.

The integration core rejects a symlinked Codex config in v0.5. Replacing a
symlink could mutate the wrong file or destroy link semantics; the UI and CLI
must explain this limitation rather than claiming recovery succeeded.

### Offline control surface

The lifecycle CLI exposes:

```text
easy-multi-provider serve --config <path>
easy-multi-provider doctor [--json]
easy-multi-provider restore [--json]
```

`doctor` and `restore` operate without importing or starting the server. A
machine-readable result contains state, relation, conflict names, lease phase,
and whether the config exists. It does not need to expose original/applied
values, credentials, or full lease metadata.

Success and conflict/error exit codes must be stable. Parser usage errors may
retain the command-line parser's conventional exit code.

## Capability Engine

Provider support cannot be represented by one protocol dropdown or inferred
from a model name. The Capability Engine provides one normalized record for
routing, UI, diagnostics, and Context Guard decisions.

A capability record covers:

- endpoint and deployment fingerprint;
- upstream model identifier;
- request protocol and authentication shape;
- streaming and Responses WebSocket behavior;
- structured tool calls and parallel tool calls;
- accepted reasoning-effort values;
- advertised input modalities, independently from image-detail fidelity;
- advertised context and output limits;
- observed success and explicit context-failure boundaries;
- value source, confidence, and observation time.

Each value remains distinguishable as:

- **official/advertised**: returned by an authoritative model endpoint or
  maintained preset;
- **observed**: proven by a real routed request or explicit upstream error;
- **manual**: set by the user;
- **inferred**: a conservative heuristic;
- **unknown**: not safely available.

The UI may summarize these states, but it must not erase their provenance.
Manual values are allowed because many compatible gateways expose incomplete
metadata; unsafe overrides should be visibly identified rather than presented
as official limits.

### Protocol auto-detection

A successful `/models` response proves only model-list compatibility. It does
not prove Responses, Chat Completions, tool calls, or streaming support.

For `auto` protocol mode, EMP attempts negotiation on the first real routed
request and caches the result for the endpoint fingerprint. Fallback is allowed
only when the endpoint explicitly rejects the attempted protocol. Authentication
failures, WAF responses, rate limits, and timeouts are not protocol evidence.
Anthropic Messages remains explicit unless the endpoint or preset provides
authoritative evidence.

Capability probing that would consume generation quota requires an explicit
user test. Ordinary model metadata requests should be distinguished from
billable generation requests in the UI.

### Input-modality contract

Model discovery may persist bounded upstream modality identifiers such as
`text`, `image`, or `video`. Missing or malformed metadata is not evidence of
vision support and therefore normalizes to the conservative `text` default.
The Codex catalog projects only modalities Codex can currently consume (`text`
and `image`); other valid identifiers remain internal capability data.

Refreshes update advertised modality data without replacing manual model
overrides, visibility, or context limits. `supports_image_detail_original`
describes image-detail fidelity only and must not be used as the image-support
flag.

Protocol translation preserves the supported declared input. Responses
forwarding keeps `input_image` parts unchanged. Chat Completions translation
maps each `input_image` with a string URL, including a data URL, to an
`image_url` content item while retaining its position among text parts. The
v0.5 contract and tests cover valid URL/data-URL image parts; malformed image
parts are outside this guarantee.

## Request observation and diagnostics

Every routed request receives a local trace identifier. Diagnostics may include:

- route, Provider/model fingerprint, and selected protocol;
- transport and streaming mode;
- timing, byte counts, token estimates, and upstream usage;
- capability decisions and their sources;
- HTTP status class and normalized failure category;
- whether Context Guard warned, allowed, or blocked.

Diagnostics must redact authorization headers, API keys, cookies, subscription
tokens, prompts, responses, tool payloads, and upstream HTML error bodies. Raw
model-visible content is off by default and is not required for v0.5 diagnosis.

Failures should be classified at their actual boundary: integration conflict,
authentication, protocol rejection, transport, timeout, rate limit, model not
found, context limit, or malformed upstream stream. A generic `not found` or
`timeout` message without the failed boundary is not sufficient.

## Context Guard

Context Guard is a preflight safety mechanism, not a conversation manager.

### Input modes

| Request mode | Available evidence | Guard behavior |
| --- | --- | --- |
| Full request | Complete translated payload | High-confidence estimate; enforcement allowed |
| Stateful WebSocket increment | First full request plus this connection's accounting | Cumulative estimate; enforcement allowed when state is intact |
| Lost or unknown state | No trustworthy full baseline | Explicit low-confidence warning; never claim exact usage |

The ordinary HTTP path is supported as a full-request path. A Responses
WebSocket starts from a full request and may then use incremental reuse; EMP
keeps only the bounded accounting needed for that active connection. A reconnect
that loses this state cannot inherit imaginary precision.

### Budget calculation

For a target request, the Guard derives an effective ceiling from valid hard
limits and observed failure boundaries. The translated input estimate includes
tool definitions, schemas, and protocol structure. It separately reserves
budget for expected output and a conservative safety margin, without counting
tools or schemas twice.

The resulting safe input budget and the translated input estimate are both
reported with their sources. Tokenization may be exact for a supported model or
conservative for an unknown tokenizer; the confidence must remain visible.

The Guard never silently:

- removes messages or tool results;
- compacts or summarizes the Codex thread;
- changes model or Provider;
- lowers output requirements;
- retries an unchanged request already rejected for context length.

On an explicit upstream `context_length_exceeded` response, EMP records the
smallest known failure boundary for that endpoint/model/protocol key, lowers the
confidence or safe budget as appropriate, and returns a clear error. A later
successful request may update the largest known success. Network, authentication,
WAF, timeout, and rate-limit failures must not modify context calibration.

## Explicit Context Migration after v0.5

The only acceptable future migration flow is user initiated:

```text
Original Codex thread (unchanged)
              |
              v
     lossy checkpoint proposal
              |
              v
       new Codex thread/fork
              |
              v
       smaller-context model
```

Migration requires a stable public Codex API that can create a new thread or
fork and inject the explicit checkpoint. If that API is unavailable, the feature
is not implemented. EMP does not gain authority to mutate private Codex state.

## Security and isolation

- The management listener remains loopback-only by default.
- Provider keys and imported subscription credentials remain encrypted at rest
  and are never returned to the browser after storage.
- The integration lease is private local state, written atomically with
  restrictive permissions, and contains no account or Provider credential.
- Migration bundles remain separately password protected and are re-encrypted
  under the destination machine's local key.
- Repository output must exclude local configuration, credentials, account
  identifiers, Provider-specific test data, generated catalogs, leases, and
  planning artifacts not intended as product documentation.
- Concurrent requests keep routing, tool-call pairing, stream, and Context Guard
  state isolated.

## Performance model

Python remains acceptable for v0.5 provided the router stays streaming and
bounded:

- do not buffer complete streams when forwarding can apply backpressure;
- do not retain per-connection accounting after close;
- bound diagnostic and capability caches;
- avoid global locks on the request path;
- keep integration file locks outside normal inference traffic;
- measure idle memory, connection churn, and concurrent streams before setting
  release regression budgets.

Cross-platform packaging is deferred until the local vertical slice is stable.
The core must avoid platform-specific architecture, while file locking,
permissions, path handling, and process signals receive explicit Windows,
macOS, and Linux acceptance later.

## Architecture decisions

1. **Codex owns history.** EMP stores no conversation database.
2. **Default integration is leased.** EMP edits only two root fields and can
   compare, restore, or report conflicts offline.
3. **Capabilities are data with provenance.** Unknown support remains unknown;
   it is not guessed into certainty.
4. **Context safety evaluates the real upstream payload.** Guard logic runs
   after translation and never mutates history silently.
5. **WebSocket accounting is connection local.** Numeric continuity is allowed;
   persistent transcript reconstruction is not.
6. **Migration is explicit and postponed.** It requires a public Codex fork/new
   thread API and leaves the source thread untouched.

The normative runtime synchronization and recovery contract is defined in
`docs/codex-runtime-sync-v0.5-spec.md`.
