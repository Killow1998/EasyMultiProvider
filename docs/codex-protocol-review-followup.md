# Codex protocol review follow-up

Audit artifact names in this document identify local, ignored evidence files;
they are not release downloads. Source references, regression tests and the
verified results below are retained in the repository.

Implementation checked on 2026-09-15 against the current EMP checkout (base
`5fcbdbb`, with earlier uncommitted fixes retained). The original external review
is preserved in [GPT Pro's review](gpt-pro-review-2026-09-14.md).

## Evidence and delivered changes

Official source snapshot:
[`openai/codex` at `2fdcdeaf0e219eea34c710e01de2ee0571ddeeb5`](https://github.com/openai/codex/tree/2fdcdeaf0e219eea34c710e01de2ee0571ddeeb5).
This is a source snapshot, not a claim about the latest stable release.

- `catalog._external_entry` now copies an explicit set of coding instructions
  and tool settings. New native fields no longer silently become external
  capabilities. Nested model messages also use an allowlist: token-budget and
  other unreviewed modes are not copied. Experimental context is explicitly
  disabled. Native model capabilities remain intact.
- `_account_entry` omits `available_access_programs` because the source catalog
  belongs to the current login, not the imported account. Absence preserves
  unknown status; it does not assert an empty set of entitlements.
- `HistoryAnchor.from_headers` compares explicit thread identities and no
  longer equates them with `session-id`. Only a positively identified stable
  legacy `version` header through 0.154.0 permits session-only fallback.
  Unknown and newer versions need explicit thread identity to read history.
  The outgoing cache key is not rewritten, and execution-session metadata is
  not substituted for a thread ID. An empty prewarm turn ID means no turn yet.
- WebSocket continuation records the completed response's thread/window scope.
  Changed scope (including losing a previously known scope) requires the client's full request through the
  existing `previous_response_not_found` response; EMP does not replay a task.
  A turn change alone is not treated as expiry.
- `tool_bridge.ExternalTools` maps namespace/name identities to deterministic,
  request-local portable function names and restores them on the way back.
  Definitions, historical calls, `tool_choice`/allowed-tools restrictions,
  ordinary/custom calls, full responses, streaming items, and history observers
  share this one boundary. Plain names and call IDs remain unchanged. No global
  alias registry, schema cache, or second implementation per protocol was added.
  Portable names include a readable prefix, and descriptions retain namespace
  identity and documentation so a model can distinguish similar tools.
  Identical definitions remain valid; conflicting schemas or aliases return
  HTTP 422 before contacting an upstream. Native requests are not mapped.
- Client-executed `tool_search` is exposed as an external function and restored
  as a Codex `tool_search_call` with object arguments. Codex performs discovery
  and execution. Loaded tools are collected from `additional_tools` and
  `tool_search_output`; call/output identity and discovered definitions survive
  history normalization. Compaction treats search calls/results as tool pairs.
  Search argument deltas are not mislabeled as function-call deltas. Malformed
  search output fails explicitly, and server-executed search is not emulated.
  External catalog entries now enable search after the complete round trip was
  exercised with the official runtime and a local MCP server.

The identity interpretation follows `core/src/client.rs` (`responses_session_id`,
`build_session_headers`, and incremental-request handling). Capability ownership
follows `protocol/src/openai_models.rs` (`ModelInfo` and `ModelMessages`). Tool
search follows `core/src/tools/handlers/tool_search.rs`, `tool_search_spec.rs`,
`core/src/tools/spec/plan.rs`, and `protocol/src/models.rs` in the same snapshot.
[Official App Server documentation](https://learn.chatgpt.com/docs/app-server)
describes the client interface used by the binary-level checks.

## Verification

- Windows native PowerShell 7.6.5; Python 3.11; 259 targeted tests passed,
  including router integration across all three external protocols, destination
  compaction, and existing local WebSocket server continuity tests.
- Official Codex 0.154.0: four isolated binary tests passed on Windows,
  Intel macOS, and Linux x64. They cover the
  existing backend's model list, recovery after a simulated TLS failure without
  EMP replay, and an external-model tool round trip that verifies actual command
  output rather than accepting an unknown-tool error as success, and a complete
  tool-search/discovery/MCP-execution round trip. The MCP fixture is read-only,
  has no network or filesystem operations, and returns a fixed marker.
- Four in-memory ablations independently restored the old catalog inheritance,
  cache/thread comparison, tool deduplication, and continuation decisions. Each
  was caught by its corresponding regression. No working files were reverted.
- Two additional in-memory ablations disabled namespace restoration and search
  event conversion independently. Their integration/round-trip tests detected
  both regressions. Shared projection stays in one small boundary rather than
  introducing provider-specific discovery engines or persistent mapping state.
- `.github/workflows/runtime-compatibility.yml` includes `tests/**` triggers and
  separates compatibility checks from packaging. It declares Windows, macOS,
  and Linux jobs pinned to Codex 0.154.0. The workflow has not been pushed or run
  on GitHub. macOS/Linux protocol suites were also executed over SSH in isolated
  source copies (259 tests on each platform). The Mac test environment uses a canonical temporary path to
  avoid system `/var` symlinks; production symlink protections remain unchanged.
  Noninteractive CLI tests explicitly close stdin, including when run over SSH.
  The command fixture separates its home from its scratch directory so Codex
  can create its sandbox helper normally. Linux uses the complete official npm
  distribution, including bundled `bubblewrap`; a lone release executable was
  insufficient for sandboxed command execution. Sandbox settings were not
  weakened to make the check pass.
- The existing non-fatal observer contract is preserved: an observer exception
  cannot turn otherwise valid forwarding into a failed request. The final
  bridge/stream checks passed on all three platforms after that integration.
- Temporary remote source copies, test runtimes, virtual environments, and
  source archives were removed after checking for surviving test processes.
  Existing installations and original remote source directories were untouched.

Run the real-binary checks with:

```text
python tools/test_codex_runtime.py /absolute/path/to/codex
```

The runner exports the binary's bundled catalog, creates an empty temporary
`CODEX_HOME`, supplies fixture-only credentials, and blocks non-loopback network
access through an unavailable proxy. It does not use personal model caches,
account credentials, or a paid upstream. Temporary homes are removed on exit.

## Acceptance boundaries

The fixed binary and protocol fixtures establish the tested Codex contract, not
the failure rate of paid upstream services or behavior of every ChatGPT App
build. No broad cross-turn invalidation rule was inferred from an unconfirmed
upstream issue, and no runtime compatibility range was automatically expanded.
No running EMP binary was replaced and no release was published in this round.

## 2026-09-18 — Luna Max ten-round source audit, Round 1/10 completed

Round 1 is complete; the ten-round audit remains in progress. The durable
detail is in
`artifacts/lunamax-audit-20260918/round-01-report.md`.
This entry records only the verified state of this round and does not claim
Rounds 2–10.

### Baseline and source boundary

- EMP source: `e365622`, version `0.11.3`; installed CLI: `codex-cli 0.155.0`.
- EMP's compatibility policy still ends at `0.154.x` (`codex_compatibility.py`);
  the installed 0.155.0 binary is recorded as newer/unverified, not silently
  added to the supported range.
- The read-only Codex reference is local source HEAD
  `e269f2164cbb9f499e4f22301c393500e2a831f3`. It is not treated as a stable
  0.155 release tag.
- Round 1 enumerated all 59 EMP production Python modules and traced catalog,
  config, route resolution, native/external projection, tools, history/context,
  HTTP/SSE/WS, errors/retries/cancellation, limits, management, quota,
  accounting and diagnostics. Deep-read paths and limitations are listed in
  the artifact; path enumeration is not presented as a claim that every
  low-frequency module was read line-by-line.

### Confirmed finding and smallest fix

Codex HEAD's `codex-api/src/sse/responses.rs` consumes response headers for
`x-codex-turn-state`, `X-Models-Etag`, `openai-model`,
`x-reasoning-included`, `x-request-id` and rate-limit snapshots. Before this
round, EMP forwarded the native response body/events but dropped those
response headers at the router/server boundary. That broke native sticky
routing and client-side model/reasoning/rate-limit observations.

EMP now captures an explicit non-credential native response-header allowlist,
passes it through body/SSE/compact and HTTP WebSocket fallback paths, and
keeps EMP's catalog ETag authoritative. Cookie/auth/provider-internal headers
are excluded. Native request replay, history ownership and execution behavior
were not changed.

### Verification

- Focused route/stream/native-WebSocket/continuity suite: 164/164 passed.
- New response-header regressions: 5/5 passed.
- Full EMP suite after the change: 1182 tests run, including 13 skipped;
  all remaining tests passed (59.359 seconds).
- Installed 0.155.0 fixture-only CLI baseline: 4/4 passed; no paid upstream or
  personal auth was used.
- `git diff --check` passed. No Codex source or running binary was modified.

### Language assessment started

The current Python hot path is JSON decode/encode, native zstd request
compression, SSE/event validation, bounded thread/socket handling and
post-response SQLite/diagnostic accounting. Measured fixed-fixture costs were
8.00 ms for 500 native encode/compress calls and 194.06 ms for 500 batches of
100 SSE events. Request/websocket slot bounds and relevant locks were
identified; realistic concurrent p95/p99, RSS/CPU, payload-size curves and an
apples-to-apples Rust/Go/C++ comparison remain unmeasured and are reserved for
later rounds. Model latency is not attributed to Python from these local
figures.

No release was published and no installed EMP or user account configuration
was replaced.

## 2026-09-18 — Adaptation target updated to Codex CLI 0.155.0

Xian requested tracking official `rust-v0.155.0` for the ongoing ten-round
audit. The exact release reference is commit
`f0a1b8f0849d90960bc406b848f32e5a129b0457`, published
`2026-09-17T23:14:43Z`. A selected source snapshot was downloaded to this
audit's ignored artifacts; the local `codex` checkout remains unchanged.
Source HEAD is supplementary and is not used as proof of release behavior.

Compared with the exact local 0.154 release files, the Responses SSE/WS
endpoints, `ModelInfo` schema, App Server model mapping and refresh worker
are unchanged. Client changes include auth ownership invalidation, filtering
raw tool-result metadata for other destinations, image-detail normalization,
and retirement of the older unary compact client in favor of the v2 path.
The GitHub compare API returned a divergent comparison capped at 300 files;
that list is not treated as an exhaustive release diff.

An added fixture-only real CLI tool round trip on installed 0.155.0
reproduced missing `x-codex-turn-state` on the second HTTP request through
the WebSocket fallback. The previous four CLI contracts still passed; the
new fifth contract failed at the sticky-header assertion. Round 2 will
repair and validate this behavior before compatibility status is promoted.
The earlier Round 1 policy description is its historical baseline.

## 2026-09-18 — Luna Max ten-round source audit, Round 2/10 completed

Round 2 is complete; Rounds 3–10 remain unrun. Detailed evidence is in
`artifacts/lunamax-audit-20260918/round-02-report.md`.
The earlier read-only findings and the failed initial 0.155 metadata contract
remain preserved as historical evidence; this entry records the final state of
the same round.

### Exact 0.155 boundary and whole-code rescan

- Primary reference: local read-only `rust-v0.155.0`, commit
  `f0a1b8f0849d90960bc406b848f32e5a129b0457`, under
  `artifacts/lunamax-audit-20260918/codex-0.155.0/codex-rs/`.
- Installed binary: `codex-cli 0.155.0`; local Codex HEAD is supplemental, not
  release proof. EMP's current compatibility target, README, compatibility
  test and runtime CI pin now name 0.155.0, while this remains a targeted
  compatibility policy rather than a claim of complete model/provider parity.
- The Round 1 inventory of all 59 EMP production Python modules was reused and
  the native/imported-subscription/external lifecycle was rechecked across
  catalog/config/routing/projection/tools/history/compaction/HTTP/SSE/WS,
  retries/cancellation, limits/accounting/management. High-risk transport and
  error modules were read line by line; the artifact does not claim every
  low-frequency module was read line by line again.

### Confirmed fixes

- Native response headers now use an explicit non-credential allowlist,
  including safety-buffering, turn-state, model, request/error and rate-family
  values consumed by Codex. HTTP/SSE/compact and WS fallback carry the selected
  values; EMP's catalog ETag remains authoritative and cookies/auth headers do
  not cross the boundary.
- WebSocket state metadata is now `response.metadata`, while model-catalog ETag
  is `codex.response.metadata`, matching the exact 0.155 consumers. The
  installed CLI's former second-request sticky-state failure is fixed.
- Native WebSocket handshake state retains only safe selected headers. The
  transient connection key follows official owner semantics: a complete
  same-user/same-workspace credential refresh reuses the identity; user or
  workspace changes, or incomplete/opaque owner information, conservatively
  isolate it. Raw credentials are never stored in the key.
- Native `response.failed` events retain upstream error codes/details. A native
  400/401/429/context error before any stream output propagates its HTTP status
  and safe headers instead of becoming an HTTP 200 failure stream. No request
  or tool generation is replayed after it has been sent.

### Verification and boundaries

- Final local EMP suite: `1197 tests`, `16 skipped`, `OK`; loopback tests ran in
  the permitted local socket environment.
- Focused owner/bridge/continuity regression: `41/41 OK`; native error/header
  regression: `8/8 OK`.
- Installed 0.155 fixture-only official CLI contracts: `7/7 OK` (7.886s), using a
  temporary empty home and blocked non-loopback network. This evidence does
  not prove every model, external provider, paid upstream, or experimental
  realtime/voice capability.
- `git diff --check` passed after source, tests, report and this record were
  written. No Codex source, account, service or installed runtime was changed.

### Explicitly retained limitations

Official WS reads `x-reasoning-included` directly from its 101 handshake, so
safe event metadata preserves the value but does not prove the same
`ServerReasoningIncluded` observation. WS rate-limit snapshots are consumed
from `codex.rate_limits` events; EMP has not invented a header-to-event
translation without a further source-backed contract. Malformed/context and
nested terminal error shapes remain narrower follow-up questions. The current
language assessment is synthetic local serialization/loopback overhead only;
no Python/Rust/Go/C++ decision is made before Round 10.

## 2026-09-18 parent verification between rounds 2 and 3

Round 2's native CLI eventually emitted `turn.completed` and its final result
after the old supervisor timed out; its native process then exited. The
supervisor preserved that failure record, recovered the completed result, and
resumed the same Luna Max thread at round 3. Subsequent productive turns are
not killed by the old arbitrary 40-minute limit. This is two completed rounds,
not a ten-round completion claim.

The parent applied downstream TCP_NODELAY after a frozen-source WebSocket
comparison. Single/eight-client median proxy overhead fell by approximately
12/18 ms in that comparison; 32-client results did not improve consistently.
A fresh live-source fixture retained correct text and reused 1/8/32 upstream
connections. Median direct/proxy TTFT was 26.941/33.225, 25.870/35.647 and
25.661/47.019 ms. These synthetic local fixtures support transport tuning
before language migration; they do not measure paid models or production RTT.

Two narrow owner-hint guards now handle non-object JWT claims conservatively
and keep non-Bearer credentials separate from Bearer sockets. Six public
identity regressions pass; later broad verification must include these changes
rather than reusing Round 2's earlier full-suite count.

Parent binary coverage now includes the preserved builtin native catalog as
well as the external alias fixture. Native tool declarations can be in
`input` `additional_tools` items; the fixture now understands that placement
and native custom exec input. Combined installed 0.155.0 CLI contracts pass
`8/8` in 8.889 seconds (`codex-0155-eight-contracts.log`). Earlier fixture
failures were unsupported fixture formats, not product catalog defects.

A separate post-Round-2 probe confirms that context failure handling can
append a replacement SSE frame after an unterminated original data line,
producing `sse_invalid_json`. It remains assigned to the protocol/error
follow-up rounds; the eight contracts do not prove this boundary repaired.
The high-concurrency CPU profiling attempts failed in the profiler itself, so
no CPU hotspot or cross-language speedup is claimed from them.

## 2026-09-18 — Luna Max ten-round source audit, Round 3/10 completed

Round 3 is complete; Rounds 4–10 remain unrun. Detailed evidence is in
`artifacts/lunamax-audit-20260918/round-03-report.md`.
The same explicit Luna Max thread remains the control session; no later round is
being implied.

### External projection and stream findings

- A whole-EMP rescan reused the 59-module production inventory and rechecked
  native/imported-subscription/external catalog/config/routing, projection,
  tools/history/compaction, HTTP/SSE/WS, errors/retries/cancellation,
  limits/accounting/management. The artifact distinguishes inventory coverage
  from line-by-line reading of every low-frequency module.
- The exact read-only 0.155 source confirms that Codex sends
  `store=false`, `include=["reasoning.encrypted_content"]`, a stable
  `prompt_cache_key`, and `text.format` JSON Schema. EMP now retains the first
  three only for exact official OpenAI HTTPS `/v1` or `/v1/responses` roots,
  including trailing slash and effective-port-443 forms. Compatible gateways
  still receive the portable allowlist only.
- Chat projection now maps canonical `text.format` to OpenAI
  `response_format.json_schema`; Anthropic maps the representable schema to
  `output_config.format` and effort to `output_config.effort`. Anthropic
  `name`/`strict` have no equivalent Messages wrapper and are intentionally not
  invented. Unsupported Anthropic effort values (`none`, `minimal`, `ultra`,
  etc.) now fail explicitly with bounded 422 projection errors rather than being
  silently dropped or sent as an invalid official enum. Model capability filtering
  still runs before this mapping.
- Both shared external SSE parsers now buffer bytes until a complete line before
  strict UTF-8 decoding. A multibyte character split across transport chunks is
  accepted; genuinely invalid UTF-8 remains a transparent protocol failure.
  Responses DONE filtering is also continuous across chunks. Text/refusal deltas
  remain incremental; complete tool JSON remains buffered for validation, a
  deliberate execution-safety translation with an unmeasured tool-call TTFT cost.

### Verification and boundaries

- Focused external projection/stream/multimodal/transport tests: `72/72 OK`.
- Loopback router/http-pool/context/reliability group: `165/165 OK` after the
  permitted local-socket escalation. The restricted first attempt's 10 bind
  errors were environment failures.
- Full isolated suite with a task-owned `/tmp` `CODEX_HOME`: `1203 tests`,
  `17 skipped`, `OK` in `60.307s`. The first default-state run was not accepted:
  it hit restricted loopback and an unwritable default `/home/fumo/.codex` path;
  no real configuration was changed.
- The parent-maintained fixture-only installed `codex-cli 0.155.0` acceptance
  set later reached `8/8 OK` in `8.889s`; Round 3 records that as prior evidence,
  not as a new external/provider parity claim. No paid upstream, personal auth,
  realtime/voice call, or verified-runtime-range expansion occurred.

### Language assessment update

The observed Python hot paths remain JSON/deepcopy/projection, SSE/event
validation, bounded thread/socket/pool work, and post-response SQLite/diagnostic
accounting. Parent synthetic 1 MiB direct/native/chat encode medians were
`5.389/5.494/5.849 ms`; these and loopback TTFT figures are local fixture
overhead only. Fragmented-UTF-8 CPU/RSS curves, valid high-concurrency CPU
attribution, production RTT, and equivalent Rust/Go/C++ prototypes remain
missing. No migration decision is made before Round 10.

`git diff --check` passed after the Round 3 source/tests and durable records.

## 2026-09-18 — Luna Max ten-round source audit, Round 4/10 completed

Round 4 is complete; Rounds 5–10 remain unrun. Detailed evidence is in
`artifacts/lunamax-audit-20260918/round-04-report.md`.
This round focused on the complete tool lifecycle: namespace aliases,
function/custom tools, allowed choices, parallel calls, client/server
`tool_search`, discovered/MCP tools, history/compaction and external
collaboration boundaries.

### Whole-EMP coverage and 0.155 source comparison

- The 59-module production inventory was reused and native/imported-subscription/
  external catalog/config/routing, projection, tools/history/compaction,
  HTTP/SSE/WS, retries/cancellation, limits/accounting/management were rechecked.
  High-risk paths were read directly; the report does not claim every low-frequency
  module was read line by line again.
- The primary read-only reference was exact official `rust-v0.155.0`, commit
  `f0a1b8f0849d90960bc406b848f32e5a129b0457`, especially
  `codex-api/src/common.rs`, `tools/src/{responses_api,tool_spec,tool_search}.rs`,
  `core/src/tools/{router,tool_namespaces_info,handlers/tool_search}.rs`,
  `protocol/src/models.rs`, `codex-api/src/sse/responses.rs`,
  `codex-api/src/endpoint/responses_websocket.rs`, `core/src/client.rs`, and
  the session model-mismatch consumers. Local Codex HEAD stayed supplemental.
- EMP's native path does not create `ExternalTools`; Codex owns native namespace,
  discovery/MCP, parallel execution and history. External `ExternalTools` aliases
  are request-local and restored across definitions, choices, stream events,
  results, history and compaction. Client tool search is translated reversibly;
  server-side search remains an explicit 422 ownership/capability boundary.
  The 0.155 approved-host raw tool-result metadata filter is intentional Codex
  telemetry behavior, not a visible tool-call defect.

### Confirmed fix: native alias model metadata

Official 0.155 compares `ServerModel` metadata with the requested catalog slug.
Before this round, a known native/account alias such as
`native/gpt-6-astra -> gpt-6-astra` exposed the expected upstream basename and
triggered a false account-risk warning in the installed CLI fixture. EMP now
stores the known requested/upstream pair per native request and maps only matching
`openai-model`/`x-openai-model` values at the local response boundary. The same
bounded rule covers HTTP, SSE event headers, compact responses, HTTP-to-WS
metadata, fresh native WS handshake metadata and native WS event headers. It does
not rewrite `response.model`, arbitrary response content, or genuine reported
reroutes such as `gpt-5.2`; request payload/history/execution/retry semantics are
unchanged.

### Verification and remaining boundaries

- New native boundary regressions: 5/5 OK. Tool bridge: 7/7; router: 129/129;
  native WS: 32/32; catalog loopback: 10/10 after socket escalation; server:
  91/91 after socket escalation. Dialect/final-review/stream/collaboration/search/
  history focused groups also passed 31/31, 19/19, 6/6, 6/6, 2/2 and 12/12.
- Installed official `codex-cli 0.155.0` fixture-only contracts: 9/9 OK in
  11.038s, including alias no-warning, genuine reroute preservation, tool-search
  follow-up and permanent-error no-replay. This does not expand verified runtime
  range or establish all-model/provider parity. Round 3's 1203-test full-suite
  result remains prior evidence; Round 4 intentionally used focused suites.
- The restricted first loopback attempt produced only socket `PermissionError`s;
  the escalated reruns passed and are classified as environment limitations. No
  credentials, paid upstream, real config/service, Codex source, commit or
  publication was touched.
- Python hot paths remain JSON/deepcopy/tool projection, SSE validation,
  thread/socket/pool work and accounting. Existing synthetic microbenchmarks are
  not production latency or a Python/Rust/Go/C++ comparison; no migration decision
  is made before Round 10.

## 2026-09-18 — Luna Max ten-round source audit, Round 5/10 completed

Round 5 is complete; Round 6–10 remain unrun. Detailed evidence is in
`artifacts/lunamax-audit-20260918/round-05-report.md`.
This round focused on history continuity and destination switching: explicit
thread/turn/window identity, resume, compaction/checkpoints, reasoning-signature
scope, cache stability, and preservation of visible tool pairs.

### Coverage and official 0.155 comparison

- The 59-module EMP production inventory was re-enumerated. Native,
  imported-subscription, and external lifecycles were rechecked through route
  snapshots, history readers, portable projection, compaction, tool bridge,
  provider replay, HTTP/SSE/WS dispatch, errors/retry/cancellation, limits,
  accounting and management. High-risk history/continuity modules were read
  directly; the record does not claim every low-frequency module was read line
  by line again.
- Primary reference remains exact read-only `rust-v0.155.0`, commit
  `f0a1b8f0849d90960bc406b848f32e5a129b0457`. `core/src/context_manager/normalize.rs`
  preserves server tool-search output and inserts `ToolSearchOutput` for an
  incomplete client search; `core/src/compact_remote_v2_attempt.rs` uses a
  normal Responses request with `compaction_trigger`; `core/src/client.rs`
  scopes incremental reuse over request properties including reasoning/tools/
  cache fields. Local Codex HEAD remains supplemental.

### Confirmed fixes

- Checkpoint cache fingerprints now include the request-local summary
  `output_limit`, preventing a summary generated under one `max_output_tokens`
  reserve from being reused under another reserve.
- Portable history now synthesizes the official `tool_search_output` shape for
  an incomplete `tool_search_call`.
- Server-side tool-search output without a local call is retained and crossed
  to portable destinations as data-only standalone history without a
  `call_id`; EMP does not fabricate or replay a search call.

These changes preserve Codex ownership of native history/execution and leave
opaque native compaction/reasoning state untouched. Official remote-compaction
V2 and EMP external local checkpointing remain an intentional provider-specific
translation, not a claim that every provider supports Codex compaction.

### Verification and limits

- Focused continuity/compaction/tool/router group: `230 tests in 0.179s — OK`.
  Changed history modules also passed `py_compile`; `git diff --check` passed.
- Round 4's installed `codex-cli 0.155.0` fixture-only `9/9` result and parent
  baselines remain prior evidence; Round 5 did not rerun the binary runner or
  full suite. They do not establish complete 0.155 provider/model parity or
  expand the verified runtime range.
- No paid/native model call, real long-context resume, external provider call,
  or production latency/language benchmark was performed. Python/Rust/Go/C++
  migration remains undecided until Round 10.

### 2026-09-18 Point cross-review after Round 5: confirmed open findings

The same Luna Max session has completed five substantive rounds. Its audit
control requests are authorized; statements above about no real provider calls
refer to EMP target-provider tests and benchmarks. Rounds 6–10 remain required.

The parent independently verified Round 4's known native model-alias projection
with the installed official 0.155.0 binary: nine contracts passed in 10.353s.
Healthy aliases no longer produce the false account-risk warning; an actual
reported model change remains visible. The subsequent two new contracts use
a real local fake upstream WebSocket, in addition to the existing upgrade-404
HTTP-fallback fixtures. The current eleven-contract baseline is nine passing
and two failing in 13.181s, both due to duplicate Authorization headers.

Three confirmed defects still require closure before the ten-round goal ends:

- With ordinary lower-case incoming `authorization`, native WebSocket planning
  constructs both `Authorization` and `authorization`. Public route planning
  and actual upstream raw headers independently reproduce this. Normalize the
  selected credential names consistently for the wire request and owner key;
  preserve the healthy connection and genuine model-reroute contracts.
- Growing only `body.instructions` from zero to 900 characters, with the same
  history, destination, input budget 1500 and summary output limit 32, changes
  the mapped prefix from 12 to 13 units. The cache nevertheless reuses the old
  summary of 12 units and retains only the final three: the thirteenth unit
  is missing. This remains after Round 5's output-limit key fix. Cache identity
  must include the actual mapped prefix selected after tail packing.
- A valid native context-length failure emitted over SSE is forwarded before
  its event delimiter, then concatenated with the validator's generated error
  frame. The next parser reports invalid JSON and loses the original native
  context-error semantics. Repair complete-event validation and framing while
  retaining bounds, terminal truth and context accounting.

Evidence is retained under `artifacts/lunamax-audit-20260918`, including
`native-ws-duplicate-auth-probe.json`, `checkpoint-mapped-prefix-probe.json`,
`native-context-framing-probe.json`, and the eleven-contract before-fix log.
Historical full-suite and nine-contract totals are stage snapshots. Fresh final
counts are required; the default suite currently skips twenty opt-in contracts.

During Round 6, the parent verified the mapped-prefix cache repair independently:
instruction sizes `0 → 900 → 900` produce mapped units `12 → 13 → 13`,
cache miss/miss/hit and cumulative summary calls `4 → 9 → 9`. The history-prefix
data-loss finding above is closed, with valid repeated-prefix reuse preserved.

The parent also caught a regression in Round 6's proposed blanket
`supports_search_tool=False` catalog change. The existing installed 0.155.0
deferred search/load/execute contract fails (`1 test in 1.077s`): one generation
occurs instead of three, with no tool results. That same acceptance previously
passed. Codex-owned client tool discovery is supported by EMP's reversible
bridge and differs from unsupported vendor-side search execution. The blanket
change and its corresponding false assertion require reversal or an equivalently
verified capability policy; the behavioral acceptance must remain intact.

### 2026-09-18 Point Round 6 completion: catalog capability and cache closure

Round 6 completed the catalog/config/capability pass. The parent evidence above is
preserved as the discovery record: the temporary `supports_search_tool=False` change
was a regression, not a fix. EMP's `tool_bridge.py` supports the Codex-owned
client-side deferred `tool_search`/load/execute round trip, and the installed 0.155.0
contract failed with one generation and no tool results while that field was false.
The final external catalog therefore retains `True`, with an explicit comment that it
does not claim vendor server-side web search.

Two confirmed issues were fixed and regression-tested in EMP: catalog cache ownership
now hashes the bounded user/workspace owner when available and conservatively isolates
opaque/incomplete credentials; history compaction now keys the summary to the actual
mapped prefix after tail packing, preserving valid same-prefix reuse. The parent
reproduced the latter as mapped `12 → 13 → 13`, cache `miss → miss → hit`, summary
calls `4 → 9 → 9`. Native duplicate-Authorization headers and SSE context-error
framing remain open for later transport-focused rounds. Round 7–10 remain pending.

Final Round 6 evidence: EMP targeted catalog/cache/owner checks `31 tests — OK`;
the installed official 0.155.0 deferred-search contract `1 test — OK` using only
local fake fixtures; the affected cross-module suite `498 tests — OK (skipped=1)`;
selected-module `py_compile` and `git diff --check` both passed. No full discover
run or verified-runtime-range expansion is implied.

### 2026-09-18 Point Round 7 completion: reliability, cancellation and native transport closure

Round 7 completed the reliability-focused whole-EMP rescan. The detailed ledger is
in `artifacts/lunamax-audit-20260918/round-07-report.md`.
Historical Round 1–6 evidence above is preserved; Round 8–10 remain pending.

Three confirmed native transport issues were closed with minimal EMP changes:

- Responses SSE now buffers and validates one complete event before forwarding it,
  rejects malformed/non-object events with bounded categories, flushes a valid
  terminal event at EOF, bounds an unbroken event early without rejecting multiple
  legal events in one coalesced chunk, and preserves native context `response.failed`
  details while updating request-local context accounting. This removes the prior
  context-frame merge into `sse_invalid_json` and does not add replay.
- Native WS credential header selection now canonicalizes Authorization and
  `chatgpt-account-id` once for both the owner key and actual handshake, removing
  duplicate raw Authorization headers without broadening credential scope.
- Native WS safe model/reasoning handshake metadata is emitted for every downstream
  `response.create`, including a reused upstream socket, matching the official
  0.155 `responses_websocket.rs` stream behavior. The `x-openai-model` handshake
  variant is retained; known aliases are projected while genuine reroutes remain
  visible.

The final reliability group passed `342 tests in 34.511s`; the focused native
SSE/model/WS/catalog group passed `50 tests in 4.615s`. The proper installed
0.155.0 fixture runner passed all `11 tests in 11.785s`, including no unsafe HTTP
replay after TLS/WS failure, permanent native errors without generation replay,
reused native WS metadata/tool follow-up, and genuine upstream reroute visibility.
The first sandbox attempt's 17 loopback `EPERM` errors were environment failures;
the escalated same-class run and final group passed. `py_compile` and `git diff
--check` passed. These fixture results do not expand the verified runtime range to
complete 0.155.0 provider parity, and no paid/personal upstream was used.

The lifecycle rescan retained Codex ownership of history, execution, cancellation,
and request identity. External protocol translations, safe response-header
filtering, client-owned tool discovery, opaque reasoning/compaction limits, and
provider capability boundaries remain intentional. The measured language record
still contains only synthetic local Python CPU figures; no fair Rust/Go/C++
prototype or production latency evidence exists, so the language decision remains
deferred to Round 10.


### 2026-09-18 parent cross-review during Round 8: native malformed-event policy correction

The parent rechecked exact `rust-v0.155.0` source: `codex-api/src/sse/responses.rs:613–624` and `codex-api/src/endpoint/responses_websocket.rs:737–742` skip a malformed/unparseable event and continue. EMP's prior generic strict parser and Round 7's restored strict policy differ from that native consumer behavior. The parent's earlier classification of silent-drop as an unconditional defect was too broad; retaining EMP's previous policy does not itself establish official native parity.

An isolated installed-CLI direct-versus-EMP comparison confirms the visible discrepancy: **4 total tests, 2 direct controls passed, 2 EMP-native cases failed, 4.634 s**. Direct SSE and direct WS tolerate malformed JSON / array / null events, execute the unchanged local command tool, send its result and finish the expected reply with two generation requests. EMP-native SSE returns 502 and causes a CLI reconnect; EMP-native WS abandons the healthy route and attempts the guarded HTTP fallback. Evidence: `artifacts/lunamax-audit-20260918/codex-0155-native-malformed-direct-vs-emp-current-state-frame.log`.

The initial direct-WS fixture completed its tool flow but failed a separate sticky-state assertion because a prewarm handshake can precede the current turn's OnceLock. The parent added a current-stream `response.metadata` state frame only to these malformed WS cases, preserving all malformed data and tool/reply/fallback assertions; the historical initial log remains. Existing healthy/reroute/error cases were unchanged.

The parent updated its own native malformed-event unit acceptance to the proven skip/continue semantics and added a separate portable API-key strict parsing case. The five-test stream-fidelity module is currently red only for the three native malformed-event subcases pending production alignment; EOF, original context failure, explicit context accounting, coalesced per-event bounds and portable strict failure remain passing. The official fixture runner now contains **15** contracts; default optional CLI skips rose from 20 to 24. These are newly established open findings for the remaining rounds, not a final all-green result. Native-only alignment and the separate stale-101-state replay refinement remain with the ongoing Luna audit; no broad parser relaxation or language rewrite is implied.


Parent closure of the malformed-event cross-review: Round 8 production alignment now skips malformed/non-object events only for declared native Responses (`account` and `forward`) and native WS, preserving portable strict parsing and prior bounds/UTF-8/terminal semantics. The five parent stream-fidelity tests pass within the 44-test native focus group. The isolated installed 0.155.0 runner is **15/15 OK, 15.462 s**; its exact completed-command output is preserved in `artifacts/lunamax-audit-20260918/codex-0155-fifteen-native-aligned.log` (extracted from the authoritative completed Round 8 tool event). These newly established native cases are closed; the ten-round audit and final broad verification remain in progress. Earlier strict-policy claims above are historical and superseded by the verified official semantics.

### 2026-09-18 Point Round 8 completion: performance evidence and native 101/event fidelity

Round 8 completed the performance/language-focused whole-EMP rescan. Detailed
coverage, findings and measurements are in
`artifacts/lunamax-audit-20260918/round-08-report.md`.
Round 9–10 remain pending; this entry is not the ten-round result.

Two confirmed native 0.155 discrepancies are now closed:

- On a reused native WebSocket, EMP repeats only `openai-model`/`x-openai-model`
  with the current route mapping. Fresh connections retain the selected safe
  metadata. Stale turn-state, rate-limit and catalog headers are no longer
  replayed before the current turn's events. This follows the exact 0.155
  `responses_websocket.rs` distinction between per-stream model metadata and
  connection-established turn state.
- Native Responses SSE and WS now skip malformed JSON/non-object events only for
  declared native `account`/`forward` routes, matching the exact 0.155 SSE/WS
  consumer. Portable/external strict parsing, UTF-8, size, terminal and
  incomplete-stream rules remain unchanged. No request replay was added.

Current-source measurements remain synthetic and local: projection/JSON encode
at 1.05 MiB was `5.330 ms` median for native and `5.350 ms` for Chat; the
native event parse/rewrite/encode slice was about `10.584 µs/event` at 5000
events. The bounded deflate WebSocket fixture at 32 concurrency measured
`101.923 ms` median proxy TTFT versus `29.069 ms` direct under an aggressive
1 ms/event workload. This is not model latency or a Python-vs-Rust/Go/C++
benchmark. The evidence supports retaining Python provisionally and profiling
CPU/RSS/lock-wait at scale before selective extraction; it does not justify a
full rewrite.

Verification: native stream/WS/model focus `44 tests — OK`; cross-module
transport/reliability/accounting focus `355 tests in 39.214 s — OK`; installed
0.155.0 fixture runner `15 tests in 15.462 s — OK`; touched-file `py_compile`
passed and final `git diff --check` exited 0 with no output. These fixture results do not
expand the complete verified runtime range or claim universal model/provider
capability parity.

**Round 8/10 completed. Round 9–10 remain pending explicit supervisor resume.**

### 2026-09-18 Point Round 9 completion: adversarial cross-protocol review

Round 9 completed the adversarial whole-EMP rescan. Detailed coverage and the
source/test ledger are in
`artifacts/lunamax-audit-20260918/round-09-report.md`.
Round 10 remains pending; this is not the ten-round reconciliation.

One new confirmed discrepancy was fixed. Official Codex 0.155 keeps the local
`Persistent` reasoning choice but projects it to Responses wire effort
`disabled` (`codex-rs/protocol/src/openai_models/reasoning_effort.rs:36–37`,
`core/src/client_tests.rs:525–542`). EMP's external Responses capability filter
compared that wire alias literally with a catalog's advertised
`reasoning_levels=["persistent"]` and removed the selected effort. The narrow
fix preserves `disabled` only for an external Responses route that explicitly
advertises `persistent`; explicit `supports_reasoning=false` still wins, and
Chat Completions/Anthropic routes remain strict and do not receive the alias.

The installed 0.155.0 fixture consumer verified the complete consequence:
CLI→EMP received `disabled`, the local fake external Responses upstream received
`disabled` for both generation requests, and the existing command-tool/final
reply flow remained unchanged. No provider/account call was made. Prior native
SSE/WS error, malformed-event, header, no-replay, history, tool and continuity
fixes were rechecked rather than duplicated; their pre-fix probes remain
historical evidence, while current unit and installed fixture contracts pass.

Round 9 verification: the focused cross-protocol/reliability/router group passed
`300/300` after one non-escalated localhost `EPERM` environment failure was
retried with the task-owned loopback permission; catalog/server transport and
management group passed `101/101`; the installed official 0.155.0 fixture runner
passed `15/15` in `15.442s`; `git diff --check` and touched-file compilation
passed. These fixture results do not expand EMP's verified runtime range, prove
all external vendors accept `disabled`, or establish full native WebSocket parity
for arbitrary API-key providers. Synthetic Python language measurements remain
the only performance evidence; no Rust/Go/C++ prototype or production latency
comparison was added.

**Round 9/10 completed. Round 10 remains pending explicit supervisor resume.**

### 2026-09-18 Point Round 10 completion: final ten-round reconciliation

Round 10 completed the final whole-EMP rescan and reconciled the durable record in
`artifacts/lunamax-audit-20260918/round-10-report.md`.
The ten-round audit is now complete. Historical Round 1–9 entries above are
preserved, including their pre-fix probes and stage-specific test counts; this
entry records only the final evidence and does not rewrite those historical
claims.

The final approved loopback run found one confirmed native 0.155 fidelity defect:
EMP applied its portable Responses output-item validator to native terminal
events, rejecting valid Codex server-side `tool_search_call`/
`tool_search_output` items and emitting `response.failed` on native JSON/SSE/WS
paths. `easy_multi_provider/stream_adapters.py` now bypasses only that
external item-schema check for native `account`/`forward` passthrough while
retaining top-level terminal, UTF-8, frame-size, lifecycle and completion
boundaries. Portable API-key/Chat/Anthropic validation remains strict. Existing
native stream regressions and the installed CLI server-search contracts prove
the fix; no generation/tool replay was introduced.

Final evidence:

- `.venv/bin/python -m unittest discover -s tests -q`: **1238 tests, 28 skipped,
  OK, 61.015 s**, with loopback/socket fixtures run under the task-approved
  environment. The earlier ordinary-sandbox `EPERM` results are environment
  failures, not product failures.
- `.venv/bin/python tools/test_codex_runtime.py
  /home/fumo/.config/nvm/versions/node/v20.20.2/bin/codex`: **19 tests, OK,
  19.248 s**, fixture-only against temporary `CODEX_HOME` and local fake
  endpoints. This does not expand full 0.155.0 provider parity or prove all
  models/capabilities match native behavior.
- `compileall` and `git diff --check`: passed.

Final language decision: retain Python provisionally. Synthetic local
serialization/event and WebSocket measurements identify bounded JSON/SSE,
compression, buffering, threads and locks as profiling targets, but there is no
production CPU/RSS/lock-wait curve or fair Rust/Go/C++ implementation benchmark.
Selective Rust remains a conditional parser/transport extraction candidate only
if production profiling proves a local bottleneck; Go/C++ and full migration are
not justified by current evidence. No claim is made that another language lowers
model latency.

Remaining unsupported/unverified behavior is explicit: arbitrary external
Responses WebSocket parity, realtime/voice/media, real provider acceptance of
reasoning aliases and provider-specific search/compaction/capability claims,
long-context production resume, full WS rate-event parity, and production-scale
performance. The compatibility policy tracks `0.149.x–0.155.x` and recommends
`0.155.0`; fixture evidence is not a license to broaden that claim further.

**Round 10/10 completed; ten-round audit completed.**

### 2026-09-18 Point final review: body guards and final accepted snapshot

The same Luna control thread completed all ten substantive rounds; the supervisor
checkpoint is `complete`, with `completed_rounds=10`, and the tenth turn emitted
`turn.completed`. Point then narrowed the final native terminal exemption before
closing the parent goal. The initial Round 10 patch returned early for all native
terminal payloads, which also skipped existing body checks. The accepted code now
calls `validate_responses_body(response, validate_output_items=not native_passthrough)`:
native output-item interpretation belongs to Codex, while output-array/item-object,
status, error and incomplete-details consistency checks remain in force. A public
helper probe rejected `output="not-an-array"` in both native and portable modes;
the seven observable native stream regressions remained green (`0.037s`).

Scope correction to the earlier Round 10 wording: installed direct SSE, direct
WebSocket, and EMP's native **upstream WebSocket** controls already accepted the
server-search results before the fix. The independently demonstrated CLI defect
was EMP's native HTTP/SSE relay, including a downstream local WebSocket using
HTTP/SSE fallback; unit regressions also demonstrated native JSON fallback
rejection for both account and forward modes. This does not claim that every
native WebSocket path was broken or needed an output-item fix.

Final checks after this last production refinement supersede the earlier Round 10
accepted-snapshot timings:

- Full regression: **1238 tests, 28 skipped, no failures/errors, 60.685s**;
  `final-parent-broad-regression.log`.
- Installed official Codex CLI **0.155.0: 19/19 contracts passed, 19.088s**;
  `final-parent-codex-0155-nineteen-contracts.log`.
- `compileall` and `git diff --check` passed. Codex checkout remained clean at
  `e269f2164cbb9f499e4f22301c393500e2a831f3`; the exact 0.155 reference used for
  adaptation is `f0a1b8f0849d90960bc406b848f32e5a129b0457`.

The acceptance and performance fixtures used temporary homes, fake credentials
and local fake upstreams. The explicitly authorized Luna audit control session
itself used the native model; the earlier reports' "no real provider calls"
statements apply to EMP test targets, not that audit control session. The language
decision and unverified boundaries above remain unchanged: keep Python, consider
selective Rust only after a production profile demonstrates a local bottleneck,
and do not start a full Rust/Go/C++ rewrite on this evidence. No commits, publishing,
personal configuration changes or Codex source changes were performed.

**Ten rounds and final source review completed; this is the final accepted snapshot.**

### 2026-09-18 EMP 0.11.4 release preparation

Xian subsequently authorized updating GitHub and publishing the fixes. The
earlier audit entries describe their historical no-commit/no-publish boundary;
this release stage has explicit authorization to commit, push and publish.

Before preparing 0.11.4, Point fetched and retained upstream main commit
`e6fa73ba01aee1496324b8854ecb4afce58ede88`: peer-initiated 1009 WebSocket
recovery before any response event, context-managed compressed connections, and
restoration of GPT-5.5 when advertised by the subscription catalog. The only
source merge conflict was an import block; both `ExitStack` and dataclass
`field` were retained. One existing proxy test used a non-context-managed Mock;
its fixture was updated to match the new connection interface without removing
the proxy/identity assertions.

The merged release candidate passed **1242 tests in 62.240s, 28 skipped, no
failures/errors**, and the installed official Codex CLI 0.155.0 runner passed
**19/19 contracts in 19.629s**. These replace the pre-integration audit counts
for release acceptance. Source, pyproject, editable lock metadata and both
READMEs now agree on **0.11.4**; the unchanged dependency resolutions were
preserved. Release notes include both the audit fixes and the upstream fixes.
`compileall`, CLI `--version` and `git diff --check` passed.

Runtime compatibility CI now includes the new native model/owner/stream
regressions in its Linux, Windows and macOS protocol jobs, in addition to the
19 installed-CLI contracts. Publication remains pending those remote checks and
the four native package builds, smoke tests, manifest and checksum verification.
