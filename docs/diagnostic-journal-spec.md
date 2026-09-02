# EMP Diagnostic Journal Specification

Status: normative for implementation

## Objective

EMP persists a bounded, structured diagnostic journal so a later bug report can
be reconstructed from local evidence instead of relying on the user's memory or
conversation transcript. Python's standard library is sufficient; this feature
must not add a glog or other logging dependency.

The journal is diagnostic metadata, not conversation history. It must never
store prompts, assistant content, tool arguments/results, images, uploaded
files, HTTP bodies, headers, cookies, credentials, or complete upstream error
bodies.

## Storage and lifecycle

- A `serve` invocation creates one logical run with a random run ID.
- Default location: `<config directory>/state/logs/`.
- Files use UTF-8 JSON Lines and a stable `emp-<UTC>-<pid>-<run>-pNNN.jsonl`
  naming scheme. Multiple parts still belong to one startup run.
- Directory permissions are private where the operating system supports POSIX
  modes: `0700` directory and `0600` files.
- Reject or disable journal writes through symlinked directories/files. Journal
  setup failure must be visible on stderr but must not stop EMP from serving.
- Each part is bounded to 2 MiB. The whole managed log directory is bounded to
  10 MiB. After every rotation and whenever the total exceeds the budget,
  delete the oldest matching regular log parts until the budget is restored.
- Never delete non-matching files, symlinks, directories, or the active part.
- A single bounded record may cause only a small temporary overshoot. Keep the
  newest evidence when pruning.
- Flush every accepted record; close cleanly during normal, SIGTERM, and Ctrl-C
  shutdown. Do not call `fsync` per record.

## Record contract

Every line is one JSON object containing:

- UTC timestamp;
- monotonically increasing sequence within the run;
- run ID;
- level (`debug`, `info`, `warning`, `error`);
- stable event name;
- bounded structured fields.

Records are capped at 16 KiB. Strings and collections are bounded before
serialization. The writer is thread-safe and produces valid individual JSONL
records under the server's bounded concurrency.

## Mandatory events

Lifecycle:

- `process_start`: EMP/Python version, platform family, PID, configured host and
  port, and counts of accounts/providers/models. Do not record username,
  hostname, environment contents, or an absolute config path.
- `proxy_selected`: only `environment`, `system`, or `direct`; never proxy URL.
- `startup_reconcile`: action/state/relation/conflict codes and duration.
- `service_listening`: local host and effective port; never bootstrap/session
  tokens.
- `shutdown_start` and `shutdown_complete`: reason and restore result.
- `startup_failure` / `internal_error`: operation stage, exception class, and a
  bounded stack-frame list without locals or source-line text. Do not persist an
  arbitrary exception message.

HTTP management surface:

- method, query-free path, status, request byte count, and duration;
- dynamic account path segments are replaced by the constant `{account}`;
- no request/response body and no client headers;
- unexpected handler errors include only stage/path/exception class.

Routing and reliability:

- persist the already-normalized `ObservationRing` record for each route;
- provider/model safe IDs, endpoint fingerprint, deployment identity;
- selected/resolved protocol, dialect, transport and fallback/retry decision;
- HTTP status/error class, duration and request/response byte counts;
- content-free performance facts: TTFT, upstream first-token time, generation
  duration, output-token count, TPS, local preparation time, and requested
  standard/Fast mode when available;
- stream terminal observation, close code, output/tool activity and recovery;
- context estimate/limit/reserve/confidence/source and allow/warn/block result;
- never persist the original request, SSE events, WebSocket frames, response
  text, image URLs/base64, or tool payloads.

Low-frequency management operations should record only outcome metadata:

- model discovery: provider safe ID, discovered/selected/added/hidden counts,
  duration and result class;
- catalog refresh: visible model count and result;
- integration enable/restore/reload: resulting state/relation/conflict codes;
- account import/delete/quota refresh: an irreversible hash of account ID,
  operation result and duration; never email, account ID, auth JSON or quota
  token;
- migration import/export: success/failure and item counts only; never password
  or bundle content.

## Safety boundary

All callers pass structured fields through one journal API. The API must drop
explicitly forbidden keys (`authorization`, `cookie`, `headers`, `body`,
`request`, `response`, `prompt`, `content`, `input`, `output`, `api_key`,
`token`, `bootstrap`, `session`, `password`, and credential/auth payloads) and
redact credential-shaped fragments in string values. Token-count field names
such as `estimated_tokens` are allowed because they contain numeric metrics,
not credentials.

The journal must be best-effort and fail closed: formatting, pruning, file, or
callback errors never affect routing. At most one concise stderr warning is
emitted for a disabled journal.

## Integration shape

- Implement the bounded writer in a dedicated module, not inside the HTTP
  handler.
- `serve()` owns journal creation and shutdown.
- `AppState` accepts an optional journal and defaults to a no-op journal in
  tests/embedded use, so constructing `AppState` never creates files.
- `ObservationRing` accepts an optional sink callback and emits only its final
  normalized record to the journal, outside its lock.
- HTTP access and lifecycle events use the same journal instance.
- Startup prints `Diagnostic log: <path>` after successful journal creation.
  It must not print or log the bootstrap/session secret.
- The authenticated diagnostics endpoint revalidates recent route records from
  managed log parts, aggregates at most 512 across runs, and returns only safe
  health/model summaries plus the latest 64 normalized request facts. It never
  returns raw JSONL records or conversation content.

## Current-checkout implementation map

The server integration for this checkout is intentionally narrow:

- `serve()` resolves the effective EMP config file first, creates the journal
  from that file's parent directory, and owns the journal until the final
  shutdown record has been flushed. Journal creation happens early enough to
  capture master-key, config-load, listener-bind, and startup-reconcile
  failures. The exception is re-raised after a safe `startup_failure` record;
  logging must never change existing startup failure semantics.
- `AppState(..., journal=None)` stores a `NullJournal` by default. When a real
  journal is supplied and no custom diagnostics ring is supplied, AppState
  creates `ObservationRing(sink=...)`. Existing direct AppState construction
  therefore remains filesystem-free.
- `ObservationRing.record()` builds one normalized record, appends it while
  holding its ring lock, then invokes its optional sink after releasing the
  lock. Sink exceptions are swallowed. The sink emits `route_observation`
  using only the normalized record, never the source event or request body.
- The HTTP handler records one `http_request` in `finish()` (or an equivalent
  exactly-once boundary) using `command`, query-free parsed path, selected
  response status, declared request byte count, and elapsed milliseconds.
  `send_response()` may remember status, but must preserve BaseHTTPRequestHandler
  behavior. Streaming and WebSocket upgrade requests are included without
  recording frames or chunks.
- Unexpected GET/POST/DELETE/WebSocket handler exceptions additionally use
  `exception_event()` with a constant stage and query-free path. The response
  behavior remains unchanged; arbitrary exception text is not passed to the
  journal.
- Request-size rejections emit `request_rejected` with a fixed transport and
  reason (`wire_body_too_large`, `decoded_body_too_large`, or
  `websocket_message_too_large`), `limit_bytes`, and the bounded declared HTTP
  `request_bytes`. A WebSocket upgrade has no HTTP body, so that count is zero;
  it is not the frame size. No request text, attachments, or headers are logged.
- Automatic growth emits `request_capacity` with a process-local sequence,
  timestamp, fixed transport/kind/reason, and previous/new byte allowances.
  Reasons are empty on expansion, `memory_limit`, or `hard_limit`. The same
  content-free records remain in local diagnostics and console output; normal
  Web UI status does not expose capacity policy.
- Management-operation records are emitted at the operation boundary after a
  result is known. Account IDs use `journal.pseudonym(account_id)`. Provider and
  model identifiers may use the existing bounded safe-ID normalizer. Migration
  records contain numeric item counts only.
- Lifecycle ordering is `process_start`, `proxy_selected`,
  `startup_reconcile`, `service_listening`, then `shutdown_start` and
  `shutdown_complete`. A startup exception emits `startup_failure`; the
  following `shutdown_start` and `shutdown_complete` records describe the
  cleanup attempt and do not imply that the service reached listening state.
  The journal is closed after server close and integration restoration have
  been attempted.

The integration must not add a general-purpose Python root logger, intercept
third-party logs, redirect stdout/stderr, or change request retry, WebSocket,
compaction, authentication, catalog, or integration behavior.

## Acceptance tests

1. One serve run creates a uniquely named JSONL run file under `state/logs`.
2. Every line is valid JSON with required run/timestamp/sequence/event fields.
3. Concurrent writes remain valid and ordered by unique sequence.
4. Segment rotation plus aggregate 10 MiB policy removes oldest matching files,
   preserves newest/active data, and leaves unrelated files/symlinks untouched.
5. POSIX permission tests assert `0700`/`0600` where applicable.
6. Forbidden fields and representative Bearer/API-key/cookie/bootstrap values
   never appear in persisted bytes.
7. Oversized fields/records are bounded without breaking JSONL.
8. ObservationRing persists its normalized safe record and discards unsafe
   source keys.
9. HTTP logging strips query strings and records status/duration without bodies.
10. Lifecycle tests cover clean shutdown and a startup failure without exposing
    secrets.
11. Journal setup/write/prune failure does not prevent service behavior.
12. Existing route, multimodal, reliability, integration and Web tests remain
    unchanged in behavior.

## Non-goals

- No conversation/thread/history database.
- No remote telemetry or automatic upload.
- No request/response replay.
- No log viewer, search UI, compression, encryption, or configurable retention
  in this slice. The journal is already private and content-free; encryption can
  be reconsidered only if future records need sensitive content (they should
  not).
