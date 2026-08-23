# EasyMultiProvider v0.6 Cross-Model Continuity Specification

Status: implemented; offline release gate passed

This document closes the remaining v0.6 continuity gap exposed by native
opaque compaction. It supplements `docs/v0.6-spec.md`; when the two documents
conflict on cross-route compaction behavior, this document is authoritative.

## 1. Objective

A Codex task must remain usable when the user switches among:

- `N`: models from the current Codex login;
- `S`: models from an imported Codex subscription;
- `E`: models from an external Provider.

The required directed transitions are `N→S`, `S→N`, `N→E`, `E→N`, `S→E`,
and `E→S`. One representative `E1→E2` transition is also required.

Codex remains the sole owner of the task, persisted history, resume, tool
execution, and compaction lifecycle. EMP owns only routing, destination-aware
ephemeral projection, and the minimum content-free metadata needed to make a
safe projection.

## 2. Non-goals and hard boundaries

EMP must not:

- edit Codex rollout JSONL, state databases, or task records;
- retain prompts, assistant output, tool arguments, tool results, reasoning,
  compaction summaries, or complete request/response bodies;
- forward a Provider-owned opaque item to another trust domain;
- silently drop or replace meaningful history with a generic placeholder;
- guess which subscription produced opaque state;
- retry model generation after a bridge failure;
- disable native WebSocket, HTTP fallback, remote compaction, authentication,
  or resume behavior;
- introduce a second conversation-history database.

The implementation must not stop or reconfigure a running Codex or EMP process.
All automated runtime checks use injected fakes and isolated temporary state.

## 3. Terms and invariants

### 3.1 Route identity

A **route** is the resolved destination selected by a catalog slug. It includes
the Provider or subscription, authentication mode, endpoint, and upstream
model.

A **native trust domain** is the authentication identity plus Codex endpoint
that can consume a native opaque item. Two different model slugs under the same
current-login or imported-subscription authentication route may share a trust
domain. The current login and every imported subscription are distinct trust
domains even when they use the same endpoint.

Provider IDs, model display names, or endpoint equality alone are not proof of
a shared trust domain.

### 3.2 Compaction classes

- **native opaque compaction**: a `compaction` item produced by a `codex_native`
  route whose `encrypted_content` is not EMP-owned;
- **EMP portable compaction**: a `compaction` item whose
  `encrypted_content` uses the versioned `emp1:` envelope;
- **unknown opaque compaction**: any non-EMP opaque item for which EMP has no
  valid source binding.

Opaque bytes are never decoded, logged, exported, or sent outside their proven
native trust domain.

### 3.3 Projection invariant

For every destination request, each history item has exactly one outcome:

1. preserve it because the destination can consume it safely;
2. convert it without semantic loss into the destination dialect;
3. replace it with a source-authorized provider-neutral checkpoint; or
4. reject the request with a content-free recoverable error.

There is no permissive `continue`, generic history placeholder, or best-effort
cross-account passthrough for a meaningful unsupported item.

## 4. Required design

### 4.1 External compaction is EMP-owned

When Codex requests compaction from an `E` route, EMP must produce an EMP
portable compaction regardless of whether the upstream protocol is Responses,
Chat Completions, or Anthropic Messages.

EMP may ask that selected external model to summarize the visible compact
input, but the item returned to Codex must be the versioned `emp1:` envelope.
EMP must not persist a Provider-owned opaque compaction item from an external
route. This rule applies to both `POST /v1/responses/compact` and a Responses
request ending in `compaction_trigger`.

An ordinary generation response that unexpectedly contains a Provider-owned
opaque compaction item is rejected; it is not installed into Codex history.

### 4.2 Native compaction source binding

Native compaction remains native so subsequent requests within the same trust
domain retain full fidelity. Whenever EMP observes a successful native compact
output, it records a **source binding** for each opaque compaction item.

A binding contains only:

- a one-way fingerprint of `encrypted_content`;
- a stable native trust-domain identifier;
- the exact routable source model slug used to create the item;
- endpoint/deployment fingerprint needed to detect configuration drift;
- format version and timestamps.

A binding contains no opaque bytes and no conversation content. Bindings use
the existing Fernet vault, a private atomic file under ignored local `state/`,
and a bounded oldest-first capacity. Loading corrupt, undecryptable, or invalid
entries fails closed without replacing the master key.

The native trust-domain identifier is a domain-separated one-way fingerprint
of the stable `chatgpt-account-id` plus the Codex endpoint fingerprint. For an
imported subscription the account ID is read only from its decrypted auth in
memory; for the current login it is read from the validated incoming header.
EMP never fingerprints a bearer token as a substitute. If no stable account ID
is available, EMP may preserve an already-bound item only when identity can be
proven, but it must not create or use a cross-domain handoff binding.

"Oldest-first" means FIFO by `created_at`. Re-observing the same fingerprint
may update `updated_at` but must preserve its original `created_at` and therefore
must not make an old binding immortal. Store format, every entry field, and
finite timestamps are validated before any entry is used.

The source model slug is operational metadata, not a display label. A later
lookup must resolve it through the current configuration and confirm the same
trust domain and endpoint/deployment fingerprint before use.

The store has two distinct read semantics: a content-free fingerprint lookup
returns the recorded source binding for cross-domain handoff, while same-domain
resolution additionally verifies an expected trust domain and route
fingerprints. Callers must not inspect private store internals to discover the
source, and lookup never returns the opaque value itself.

Source binding must observe both compact forms:

- JSON output from `POST /v1/responses/compact`;
- the completed compaction output item in a Responses SSE or WebSocket stream.

No normal assistant response content is retained while observing the stream.

### 4.3 Same-domain native reuse

If a native opaque item is bound to the destination's native trust domain, EMP
preserves it byte-for-byte. This covers model switches within one Codex login
or one imported subscription.

If source and destination are different native trust domains, EMP must not
forward the item. `N→S` and `S→N` therefore use the handoff in section 4.4.

### 4.4 Source-authorized ephemeral handoff

When a destination cannot consume a bound native opaque item, EMP resolves the
exact stored source route and performs one internal, non-streaming handoff
request to that native trust domain.

The handoff request:

- contains the original opaque compaction item and a fixed EMP instruction to
  produce a provider-neutral continuation checkpoint;
- uses the exact bound source model and credentials;
- exposes no destination credentials, tools, MCP server, Web search, or user
  runtime permissions;
- asks only for final checkpoint text, never chain-of-thought;
- has a fixed bounded output limit;
- is not automatically retried;
- never falls back to the current login or another imported account.

The returned checkpoint is untrusted model output. EMP validates that it is a
non-empty bounded text result, inserts it only as a user-role historical
checkpoint in the current destination projection, and discards it when that
request finishes. It is never cached, journaled, written to the binding store,
or promoted to system/developer instructions.

Visible messages and valid tool-call/output pairs outside the opaque item remain
in their original order. The opaque item itself is not sent to the destination.

The source request and destination request are separately diagnosed with safe
metadata, but their contents and the generated checkpoint are never logged.

### 4.5 EMP portable compaction at a native destination

An `emp1:` compaction is decoded into its provider-neutral checkpoint before a
native request is sent. EMP must not forward the `emp1:` envelope as if it were
native encrypted state.

This makes `E→N`, `E→S`, and `E1→E2` independent of the original external
Provider's private state format.

### 4.6 Unknown or stale source binding

If a native opaque item has no valid binding, EMP fails before contacting the
destination. It must not probe subscriptions to discover which account accepts
the item.

The error is recoverable and tells the user, without content or local account
names, to switch back to the source subscription/model that created the compact
history, continue or compact once while EMP is active, and then retry the model
switch.

This is expected for tasks compacted before this feature, for an evicted binding,
or after migration to a machine that does not possess the source subscription.

### 4.7 Request integration order

Continuity preparation runs after EMP has resolved the destination route and
before `project_request`, `responses_to_chat`, or Anthropic conversion. It
receives a configuration snapshot, the destination route/model, the requested
catalog slug, and transient validated auth headers. It returns a new request
body and never mutates the caller body or saved Provider configuration.

For each compaction item, in input order, preparation performs exactly one of:

1. decode an EMP-owned `emp1:` checkpoint to a user-role historical message;
2. preserve a native opaque item byte-for-byte for a proven same-domain route;
3. resolve and execute the source-authorized handoff, replacing only that item;
4. fail with `context_handoff_required` before contacting the destination.

After a successful native JSON compact response or native completed stream,
the observer registers opaque compaction outputs only after the successful
terminal state is known. It passes the opaque value directly to the fingerprint
helper and never places it in diagnostics. An incomplete, failed, malformed, or
externally produced response does not register a native binding.

Every external compact endpoint and external `compaction_trigger` path returns
one EMP-owned `emp1:` compaction item. Portable Responses Providers do not get
an exception merely because their wire protocol is named Responses. An
unexpected Provider-owned compaction in an ordinary external generation is a
bounded protocol error, not history to install.

## 5. Error and transport contract

The normalized failure class is `context_handoff_required`. The public message
must be concise and actionable. It may include safe item index/type and whether
the reason is `binding_missing`, `binding_stale`, `source_unavailable`, or
`summary_invalid`; it must not include opaque content, prompt fragments,
credentials, endpoint URLs, account IDs, or local paths.

- Non-streaming HTTP fails with `409 Conflict` and a structured JSON error.
- SSE emits exactly one `response.failed` terminal event and then closes the
  response cleanly.
- WebSocket emits exactly one request-scoped `response.failed`; the socket stays
  usable unless the client closes it.
- Codex must not enter a reconnect loop for this deterministic failure.
- No destination upstream request occurs after handoff failure.
- Existing native HTTP fallback and reconnect behavior remain unchanged for
  real transport failures.

Projection failures unrelated to continuity retain their existing bounded
`422` behavior.

### 5.1 Native request-compression fidelity

Codex uses Zstandard request compression for the Codex backend when native
request compression is enabled. EMP may decode the client body to route,
project, and assess it, but it must not silently turn that request into a much
larger uncompressed native upstream upload.

For `codex_native` routes:

- an HTTP request received with `Content-Encoding: zstd` is re-encoded as zstd
  after projection and sent with the same content encoding;
- a client WebSocket request translated by EMP to native HTTP is zstd-encoded,
  because the upstream Codex backend supports the same official encoding;
- retries reuse the same encoded bytes instead of serializing or recompressing
  a second body;
- Context Guard always assesses the decoded semantic payload;
- diagnostics record only decoded bytes, encoded upstream bytes, encoding, and
  compression ratio, never body content;
- API-key external routes remain uncompressed unless their own explicit
  capability/configuration says otherwise.

Compression failure is a bounded local error and does not fall back to sending
the native request uncompressed. Authentication and all native request headers
remain unchanged.

### 5.2 Reconnect and bounded concurrency

EMP's request slots are not a reconnect mechanism. Slot exhaustion, upstream
first-response timeout, upstream stream timeout, upstream 5xx, client close,
and missing terminal event remain separate failure classes.

A timeout while opening an ordinary native upstream response, before any response
object, output, or tool activity exists, may receive one internal retry within
the existing total request deadline. The retry must reuse identical encoded
bytes and route/auth identity. After that attempt EMP emits one terminal
failure; it must not start an unbounded reconnect loop.

The source-authorized handoff explicitly disables this retry, and this v0.6
change does not add a new pre-header timeout retry to external API-key routes.

EMP must retain bounded threading/backpressure. A regression with six
simultaneous mixed native/external fake requests must complete without slot
rejection, cross-request state contamination, or serialization. This proves the
observed v0.6 workload is below the configured capacity; it does not justify
raising the 32-request limit.

Large-request diagnostics must make upload expansion visible. A native request
that arrives compressed and leaves EMP uncompressed is a test failure even when
the fake upstream eventually returns 200.

## 6. Destination behavior matrix

| Transition | Plain visible history | Tool pair history | EMP compaction | Native opaque compaction |
| --- | --- | --- | --- | --- |
| `N→S` | preserve | preserve | decode | source-authorized handoff |
| `S→N` | preserve | preserve | decode | source-authorized handoff |
| `N→E` | project | project | decode | source-authorized handoff |
| `E→N` | preserve/project | preserve | decode | not newly created by E |
| `S→E` | project | project | decode | source-authorized handoff |
| `E→S` | preserve/project | preserve | decode | not newly created by E |
| `E1→E2` | project | project | decode | not newly created by E |

For same-trust-domain native model changes, native opaque compaction is
preserved instead of summarized. The table treats `N` and `S` as different
trust domains.

## 7. Diagnostics and privacy

Allowed diagnostic fields are:

- transition class (`N`, `S`, or `E`) without configured names;
- source-binding result (`hit`, `missing`, `stale`);
- same-domain boolean;
- handoff attempted/succeeded boolean;
- normalized failure class;
- item index/type, timing, transport, status, and terminal-event state.

Forbidden fields include opaque bytes or fingerprints, generated checkpoints,
model prompts, tool content, account/provider IDs, endpoint URLs, credentials,
and complete payloads. UI status may describe the recovery action but must not
expose the stored source route.

Migration export must not include source bindings. A target machine establishes
new bindings using its own subscriptions and master key.

## 8. Regression requirements

### 8.1 Unit boundaries

1. Native compact JSON and stream outputs register only content-free bindings.
2. The encrypted binding file contains neither opaque bytes nor fixture text.
3. Same-domain native projection preserves opaque bytes exactly.
4. Cross-domain projection calls only the exact bound source route.
5. A successful handoff replaces only the opaque item and preserves order,
   images, visible messages, and valid tool pairs.
6. Missing, stale, unavailable, and malformed-summary cases fail closed with no
   destination call and no content in the error.
7. `emp1:` compaction decodes for `N`, `S`, and `E` destinations.
8. Every external compaction protocol returns EMP-owned portable state.
9. Chat conversion no longer inserts a generic opaque-history placeholder.
10. A bridge request has no tools and is not retried.

### 8.2 Directed transition matrix

Using isolated fake upstreams and temporary encrypted state, test all six
`N/S/E` directions plus `E1→E2`. Each applicable direction covers:

- plain visible text history;
- one valid function-call/output pair followed by a visible assistant result;
- a compaction boundary;
- exact route/auth selection and proof that no other upstream was called.

At least one matrix case reloads the binding store in a fresh EMP state object
to prove resume/restart continuity without persisting history content.

### 8.3 Transport boundaries

Cover HTTP, SSE, and WebSocket failure framing. Assert one terminal failure, no
replay, no reconnect classification, and continued WebSocket usability after a
request-scoped handoff error.

Also cover:

- semantic equality before and after zstd re-encoding of a large native body;
- HTTP native compression preservation and WebSocket-to-native-HTTP zstd;
- absence of implicit compression for external API-key routes;
- one pre-response timeout retry with identical encoded bytes and one final
  terminal result;
- six concurrent fake requests against the bounded server;
- diagnostics that distinguish decoded/upstream bytes, timeout, 5xx, stream
  incompleteness, and client disconnect without recording content.

## 9. Acceptance

Automated acceptance requires the focused continuity suite, the affected
routing/server/dialect suites, the bounded full Python suite, Web static checks,
compile checks, lock consistency, `git diff --check`, and a secret/private-data
scan. Tests must not contact live Providers or inspect/stop host processes.

The reliability acceptance fixture uses a multi-megabyte, highly compressible
synthetic payload with no private content. The native fake must receive zstd,
decode the original JSON exactly, and observe no more than one identical retry.

User acceptance is performed later against the already configured local
routes:

1. start a task on a native model and create or reuse native compacted history;
2. switch to one external model and complete a text turn plus a tool turn;
3. switch back to native and complete another turn;
4. repeat across current login and one imported subscription;
5. confirm `/model`, resume, WebSocket, HTTP fallback, and native features remain
   available;
6. confirm old unbound tasks receive the recovery message instead of leaking,
   dropping history, or reconnecting repeatedly.

Live credentials, configured route names, task IDs, and prompts never enter
tests, logs, documentation, commits, or issue comments.

## 10. Implementation slices

The implementation is divided into sequential boundaries so storage, routing,
stream framing, and reliability can each be verified at their actual public
seam. The seven-direction transition matrix remains a black-box acceptance
asset; it does not by itself prove route identity, authentication, or handoff
behavior.

### 10.1 Pure continuity-service handoff

Before router/server integration, the continuity module exposes a pure,
content-minimizing service boundary with focused tests:

- derive a native route identity only from a case-insensitive validated
  `chatgpt-account-id`, the canonical endpoint fingerprint, deployment identity,
  upstream model, and the exact requested catalog slug;
- domain-separate and hash the account/endpoint and deployment identities;
  bearer tokens, endpoint URLs, local account labels, and Provider labels are
  never accepted as identity input or returned by the service;
- create a `CompactionBinding` for an opaque value without retaining or
  returning that value;
- register compaction items from a successful native JSON result only;
- provide a request-scoped native stream observer that inspects only
  compaction items, keeps any opaque value in memory until terminal success,
  registers on `response.completed`, and discards on incomplete, failed,
  malformed, or abandoned streams;
- never copy or retain ordinary message, reasoning, tool, image, or checkpoint
  content while observing a response.

This layer is confined to `continuity.py` and its focused tests. Route
resolution, source-authorized bridge requests, SSE framing, WebSocket framing,
and AppState ownership belong to the subsequent integration layer.

### 10.2 Sequential router integration slice

Router integration consumes the pure continuity service through one narrow
boundary in `router.py`:

- `proxy` and `proxy_compact` resolve the requested destination first, then
  prepare a copied body before any dialect projection or upstream call;
- `emp1:` remains handled as a portable checkpoint; a bound native opaque item
  is preserved only when the destination has the same proven native trust
  domain;
- every other native opaque item is looked up, its exact stored source slug is
  resolved through the same configuration snapshot, and that source route is
  verified as native with matching trust, endpoint, and deployment identity;
- the bridge request contains only the original opaque item and a fixed user
  checkpoint instruction, has no tools or runtime integrations, is
  non-streaming, sets a fixed output-token bound, and calls the native request
  path with retries and protocol fallback disabled;
- bridge output is accepted only as non-empty bounded final text, is inserted
  as one user-role historical checkpoint at the original item position, and is
  discarded after the destination request;
- missing, stale, unavailable, or invalid-summary failures become the bounded
  `context_handoff_required` 409 class before any destination call;
- no body, opaque value, generated checkpoint, credential, account identifier,
  endpoint, or configured local name enters route metadata or diagnostics.

Focused fake-upstream tests prove exact source credential/route selection,
same-domain byte preservation, cross-domain replacement, no retry/fallback,
no destination call after failure, and preservation of surrounding images,
visible history, and tool pairs. The integration exposes a narrow successful
native-compaction observation hook but does not itself own durable state.

### 10.3 AppState, stream, and failure-framing slice

AppState wires one `CompactionBindingStore` using an ignored private file next
to the configured EMP state. It must not be included in migration export.

The concrete default location is
`<config directory>/state/continuity-bindings.enc`. `AppState` owns one store
instance for its lifetime; request handlers must not create per-request store
objects or retain a second in-memory binding index. A fresh `AppState` over the
same configuration directory must load the encrypted file through the store's
normal API. The migration format remains unchanged and does not enumerate this
file.

- successful native compact JSON and successful native normal JSON results are
  inspected through the pure continuity service;
- native SSE and WebSocket events use one request-scoped observer and register
  only after `response.completed`; incomplete, failed, malformed, disconnected,
  or abandoned requests discard pending opaque values;
- HTTP non-stream handoff failure returns structured 409 JSON;
- an SSE request that fails during preparation returns HTTP 200 with exactly
  one `response.failed` event and closes cleanly, rather than returning a JSON
  body that Codex retries as a transport failure;
- a WebSocket request that fails during preparation sends exactly one
  request-scoped `response.failed`, retains the socket, and accepts the next
  `response.create`;
- diagnostics contain only the allowed continuity result flags from section 7.

The request-scoped stream observer is created only after the router has resolved
and validated the native route identity. Parsed Responses events may be exposed
through one narrow router callback/factory; the server must not parse and
re-serialize a second copy of the stream, buffer a complete stream, or keep a
global map of pending opaque values. The stream wrapper calls `abandon()` from
its `finally` path so client disconnect, iterator close, and parse failure cannot
leave pending state. Observer or encrypted-store failures are recorded only as
content-free continuity diagnostics and must never place opaque or checkpoint
content in an exception message.

`ContextHandoffRequiredError` receives protocol-specific framing rather than
the generic router error path. Its structured HTTP body and stream events carry
only `error_class`, `reason`, `item_index`, and `item_type` in addition to the
safe public recovery message. SSE negotiation occurs before returning the
failure frame, so a streaming request gets HTTP 200 plus exactly one
`response.failed`. WebSocket uses the same request-scoped failure shape, does
not update replay state for the failed request, and continues reading the next
frame from the same socket.

This slice also proves restart persistence by constructing a fresh `AppState`
over the same temporary encrypted binding file. It does not stop, restart, or
reconfigure a real Codex or EMP process.

### 10.4 Compression diagnostics and bounded-concurrency slice

The reliability layer adds safe transport measurements and the six-request
server regression without changing the continuity model.

- the native request encoding boundary exposes decoded semantic JSON bytes,
  actual upstream bytes, and `zstd` or `identity` as request-local metadata;
- `compression_ratio` is `upstream_request_bytes / decoded_request_bytes`, is
  finite and bounded, and is omitted when the decoded size is zero;
- an identical retry retains the same byte counts and encoded payload rather
  than double-counting or recompressing;
- external API-key requests report `identity` and equal decoded/upstream bytes
  unless a future explicit Provider capability opts into compression;
- these measurements flow through the existing sanitized route observation;
  no request body, prompt, header, endpoint URL, opaque value, checkpoint, or
  credential is copied into metadata or the journal;
- timeout before response headers, timeout after a response object, upstream
  5xx, incomplete stream, client disconnect, and request-slot rejection remain
  distinct normalized outcomes. The zstd fix must not relabel a genuine
  external timeout or 5xx as a concurrency problem.

The concurrency acceptance starts a temporary `BoundedThreadingHTTPServer`
with injected fake upstreams and releases six requests together: native and
external, streaming and non-streaming, including one large compressible native
payload. All six complete within a fixed test deadline, no request is rejected
by the 32-slot bound, elapsed evidence proves overlap rather than serial
execution, each response and diagnostic record belongs to its own request, and
the native fake decodes exactly the original JSON. The fixture uses no live
network, credentials, configured local names, process inspection, or runtime
restart.
