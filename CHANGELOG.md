# Changelog

## 0.6.0 (2026-08-23)

### Product

- Unified native-login, imported-subscription, and external Provider models in
  one stable catalog with compact context labels, deterministic grouping, and
  provider-qualified external slugs across the CLI, TUI, and desktop app.
- Added capability-aware Responses dialect projection for text, image,
  reasoning, structured tools, and external Codex child workers without making
  EMP the owner of tasks, permissions, or persisted history.
- Made external compaction EMP-owned and portable, while rejecting unknown or
  unexpected Provider-owned opaque state instead of silently dropping history.
- Added encrypted, content-free native compaction bindings and exact-source
  handoff so model switches among the current login, imported subscriptions,
  and external Providers can preserve compacted task context without storing a
  second conversation history.
- Added protocol-specific recoverable handoff failures: structured HTTP 409,
  one terminal SSE failure, and one request-scoped WebSocket failure that leaves
  the connection usable for the next request.
- Preserved native Codex Zstandard request compression and added one bounded
  pre-header retry using identical encoded bytes, while leaving external routes
  uncompressed unless explicitly supported.
- Added request-local compression diagnostics and bounded concurrency evidence
  without recording prompts, responses, tool payloads, opaque state, headers,
  endpoints, account IDs, or credentials.

### Verification

- Passed the seven focused v0.6 modules with 278 tests and the bounded complete
  suite with 731 tests; the two existing opt-in live Provider/Codex checks
  remained skipped.
- Proved six simultaneous mixed native/external, streaming/non-streaming
  requests overlap without slot rejection or cross-request diagnostic state,
  including exact native zstd round-trip equality.
- Passed compile, lock consistency, whitespace, ignored-local-state, and
  secret/private-data checks using offline fixtures and temporary loopback
  servers only.

## 0.5.0 (2026-08-22)

### Product

- Replaced profile-based startup with an explicit, leased default-Codex
  integration that preserves native session identity and ordinary `codex`,
  `codex resume`, `/model`, and Desktop App configuration behavior.
- Added offline `doctor` and `restore` commands, atomic compare-and-restore,
  conflict preservation, and stale-lease recovery after an interrupted EMP
  process.
- Added capability records with source, confidence, timestamps, and
  endpoint/model/protocol/deployment identity; unsupported values remain
  `unknown`.
- Made `auto` protocol reuse identity-safe and limited fallback to explicit
  protocol rejection statuses instead of authentication, WAF, rate-limit,
  timeout, or server failures.
- Added a bounded in-memory diagnostics ring and compact Web status view without
  retaining prompts, responses, tool payloads, credentials, raw endpoints, or
  upstream HTML bodies.
- Added Context Guard preflight checks over translated upstream payloads,
  connection-local bounded WebSocket replay state, and numeric context
  calibration from terminal success and explicit context-length failures.
- Added persisted model input modalities from Provider discovery, conservative
  text-only fallback, Codex text/image catalog projection, and image-preserving
  Responses and Chat Completions routing.
- Added portable stop-only Codex runtime synchronization. Initial enable and
  restore each use one confirmation to write the target, request the supported
  Remote Control graceful stop, and verify the complete paginated `model/list`
  if an external owner brings Codex back. EMP never starts or restarts Codex.
- Added a targeted cross-platform residual-host scan after both successful
  lifecycle statuses and for the documented unmanaged App Server error. It uses
  lazy psutil inspection, canonical official Node-shim resolution, exact
  same-user and active-integration-`CODEX_HOME` identity revalidation, and
  strict parsing of supported root options before the semantic host command.
  Environment inspection occurs only after a supported host role is proved and
  reads only `CODEX_HOME`. It uses graceful termination only, bounded waits,
  and strict exclusions for Codex clients, lookalikes, ambiguous argv, other
  homes, and helper commands; no process details leave the local control boundary.
- Bounded Codex control-command memory during execution by directing stdout and
  stderr to temporary file sinks and reading only the documented caps after
  exit; timeout, no-shell, return-code, and JSON parsing behavior remain intact.
- Persisted bounded runtime recovery phases while treating prior loaded states
  as stale after EMP restarts. Offline `doctor` and `restore` never probe Codex
  or claim a live catalog verification.
- Added a searchable model-discovery picker with selected/total counts and
  bulk select/clear actions for the current filter. Existing imports remain
  selected while newly discovered models start unselected.
- Kept Codex as the sole thread/history owner: EMP never silently trims,
  compacts, switches models, or retries a known over-limit request.
- Routed known unprefixed native models through the current validated Codex
  login without requiring a synthetic forward Provider.
- Kept successful enable/restore configuration transactions out of the
  `Conflict` state when only the independent runtime catalog verification
  warns, and exposed the warning separately as an action-required runtime state.
- Added compact usable-context suffixes to generated model display names, using
  native effective-window percentages and conservative unknown handling; the
  same context is appended to descriptions for slug-oriented TUI pickers.
- Kept hidden native service models out of user model pickers while allowing
  internal Codex requests such as auto-review to route through the current login.
- Clarified Provider discovery bulk actions with dynamic `Select all` / `Select
  none` labels that explicitly scope themselves to search results while filtering.
- Made an imported account that matches the current Codex login act as the
  visibility controller for unprefixed native models while retaining the
  account row and suppressing only its redundant prefixed aliases.

### Verification

- Covered the v0.5 implementation with offline unit and loopback integration
  tests. Cross-platform packaging and runtime checks remain future hardening.
- Added 11 focused multimodal regressions covering discovery, persistence,
  catalog refresh, manual overrides, URL/data-URL conversion, and Responses
  passthrough.
- Live-validated unprefixed current-login routing, a prefixed imported
  subscription, the combined `/model` catalog, native resume visibility,
  Desktop App model selection, an external Provider, and image input on Codex
  0.149.0 running on the current Linux host.

## 0.4.0 (Unreleased)

### Product

- Reused Codex's native `openai` session identity while routing the EMP profile
  through `openai_base_url`, so native resume commands include default history.
- Added the native profile resume command to generated integration output.
- Added native Responses WebSocket handling and bounded zstd/gzip/deflate
  request decoding without disabling Codex transport features.
- Preserved Codex remote compaction v1/v2, including translated Chat
  Completions and Anthropic providers.
- Accepted valid subscription SSE streams when an upstream proxy omits the
  `Content-Type` response header.
- Kept native hidden models such as Codex Auto Review out of subscription
  aliases, and added per-account model visibility controls.
- Added one-click hide/show for every imported model under a Provider.
- Separated the 30-second upstream connection timeout from the 180-second
  response deadline and retried one transient connection failure, preventing
  slow Gemini responses from being cut off as internal errors.
- Made custom Provider `auto` mode negotiate Responses first, fall back only on
  explicit protocol rejection, and persist the working protocol instead of
  silently forcing Chat Completions during model discovery.
- Converted translated streaming failures into terminal `response.failed`
  events with the upstream HTTP status, so Codex displays the real failure
  instead of only reporting a missing terminal event.
- Kept Codex client telemetry out of external API-key Responses requests while
  preserving native subscription passthrough, and replaced raw upstream HTML
  error pages with bounded gateway/WAF diagnostics.

## 0.3.0 (Unreleased)

### Product

- Added the ChatGPT Subscription forward Provider for Codex subscription traffic.
- Added structured tool-call and history support across Responses and Chat
  protocols.
- Added streaming handling for non-SSE responses, empty streams, and upstream
  errors.
- Added interception for textual `<think>` and `<tool_call>` leakage.

### Verification

- Added deterministic CLI contract coverage for JSONL, profiles, resume/restart,
  and failure semantics.
- Validated real Luna and Sol subscription canaries, explicit-thread resume,
  controlled cancellation/recovery, and the LIVE-02/LIVE-03 tool oracles.
- Added the 401/404/429/500 and malformed-stream fault matrix plus a bounded
  deterministic soak.

## 0.2.0 (Unreleased)

- Added encrypted `.emp` migration bundles for moving configuration, model
  routes, Provider keys, and Codex subscription credentials between machines.
- Imported credentials are re-encrypted with the destination machine's local
  master key; the migration bundle never contains the local master key.
- Fixed Web UI status notifications and batch quota refresh across multiple
  accounts; repeated refreshes of the same account remain rate-limited.

## 0.1.0

- Added local Web UI management for encrypted Codex subscription accounts,
  API providers, model discovery, routing, and quota snapshots.
- Added Codex profile generation with one EMP model catalog and isolated EMP
  sessions.
- Added Responses, Chat Completions, and Anthropic Messages upstream routing.
- Added proxy environment detection and a real Codex CLI demo model test.
- Published as a Linux-validated MVP; ChatGPT App and other platforms remain
  manual acceptance items.
