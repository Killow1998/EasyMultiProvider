# Changelog

## Unreleased

## 0.9.4 (2026-08-30)

### Shared Codex runtime compatibility

- Stop treating the persistent Codex App Server as an EMP-owned process:
  integration enable, restore, catalog refresh, and reload checks no longer
  stop, start, restart, or terminate Codex processes.
- Query the existing Unix control socket with a real WebSocket Upgrade and the
  `initialize` / `initialized` / `model/list` JSON-RPC sequence instead of
  writing JSONL into the raw `app-server proxy` byte tunnel.
- Report saved files separately from model IDs observed in the live backend;
  stale or unavailable listeners now wait for the backend owner instead of
  claiming synchronization.

## 0.9.3 (2026-08-30)

### Codex and catalog management

- Show the current `.codex` login beside imported Subscription accounts, with
  credential-free quota refresh and direct Native model visibility controls.
- Make Native the sole visibility owner when an imported account duplicates the
  current `.codex` login, while retaining quota refresh on the duplicate row.
- Apply display name, context label, and reasoning-summary policy once per
  canonical model family while retaining account and Provider source prefixes.
- Shorten the browser title and heading to `EMP`.
- Prefer the Codex-managed runtime for integration and report an
  older standalone `PATH` CLI separately.
- Sample each unique Subscription quota every five minutes and show local
  one-hour, one-day, one-week, and 15-day trends without storing credentials.
- Scale quota charts to the observed range so small changes remain readable.
- Keep imported account route prefixes stable while allowing a separate
  display label, including emoji, in the model catalog.
- Classify imported-account quota refresh failures without replacing a stored
  credential after an unsuccessful refresh, and use the selected managed Codex
  runtime for quota requests.
- Treat a disconnected same-route Native WebSocket as lost transport continuity
  so Codex automatically retries the full request instead of upstream rejecting
  an old `previous_response_id` on a new connection.

### Packaging

- Remove the opaque navy tile from the master application icon so generated
  Windows, macOS, and Linux icons retain a transparent background.

## 0.9.2 (2026-08-30)

### Model catalog

- Honor current-login model visibility by omitting user-hidden Native picker
  entries while preserving internal hidden service models such as Codex Auto
  Review.

### Packaging

- Keep the complete checksum-verified build matrix in CI while exposing only
  five normal installation downloads on GitHub Releases.

## 0.9.1 (2026-08-30)

### Codex compatibility

- Publish the supported Codex CLI range (`0.149.x`–`0.151.x`) and recommended
  release line in the Web UI and documentation without exposing source hashes.
- Detect newer, older, unavailable, and unrecognized Codex installations with
  one bounded compatibility probe outside the routing path.
- Keep quota refreshes on the official Codex App Server path while forwarding
  Codex-specific CA settings, exporting Windows trust roots when needed, and
  accepting the Codex 0.151 rate-limit response shapes.
- Passively verify the active EMP catalog after Codex restarts so stale runtime
  failures recover without stopping Codex or asking the user to reapply models.

## 0.9.0 (2026-08-29)

### Packaging

- Add reproducible native PyInstaller builds for Windows x64, Linux x64,
  macOS Intel, and macOS Apple Silicon.
- Produce direct executables, ZIP/tar archives, Linux `.deb`, macOS `.dmg`, and
  SHA-256 sidecars from one cross-platform packaging script.
- Smoke-test each packaged service on an isolated loopback configuration before
  uploading the artifact.
- Optionally collect all four native builds into a checksum-verified GitHub
  Draft Pre-release for manual review and publication.
- Add original EMP artwork and native Windows, macOS, and Linux application
  icons.
- Make packaged no-argument launch open the authenticated Web UI in a visible,
  foreground terminal that can be stopped with `Ctrl+C`.
- Add a macOS application bundle inside each DMG and a Linux desktop menu entry
  inside the Debian package.

### Maintenance architecture

- Centralized immutable route resolution and request dispatch across HTTP and
  Responses WebSocket entry points.
- Isolated Native Responses, portable Responses, Chat Completions, and
  Anthropic projection behind protocol adapters.
- Unified content-free upstream failure classification and bounded stream
  lifecycle handling without changing fallback policy.
- Split credential-free management and Codex integration projections out of
  the runtime request path.
- Preserved Codex-owned history, transport-only `previous_response_id`, Native
  compaction, final-payload Context Guard authority, and fail-closed external
  history reconstruction.

### Verification

- Passed focused routing, protocol, history, context, stream, replay, and
  runtime integration checks plus Python compilation and whitespace validation.
- Live-validated first-send Native to External, External to External, and
  External to Native transitions with repeated compaction, tools, and image
  continuity on the current host.

## 0.8.1 (2026-08-27)

### Product

- Added Codex 0.150 named standalone tool-output support. Responses keeps the
  native item shape, while Chat Completions and Anthropic receive explicit
  visible context without fabricated call IDs.
- Unified Native destination classification so forwarded Native routes keep
  Codex-owned opaque history and never invoke EMP destination compaction.
- Included the resolved upstream model in Provider replay identity, preventing
  opaque tool metadata from crossing a model remap.
- Bounded aggregate SSE events and pre-output retry buffers, and made temporary
  context-failure calibration expire or clear after contradictory success.
- Made Provider-key saves and `.emp` account imports transactional, restoring
  prior encrypted credentials if the surrounding configuration update fails.

### Verification

- Passed Python compilation, whitespace validation, and 336 focused regression
  tests for the affected continuity, protocol, context, stream, replay, and
  credential boundaries.
- Live-validated Codex 0.150.1 Native opaque compaction to External, portable
  External compaction to another External model, External to Native, image and
  tool continuity, and `codex resume` on the current host.

## 0.8.0 (2026-08-26)

### Product

- Separated WebSocket transport continuity, Codex-owned history
  materialization, and destination context budgeting into independent
  boundaries.
- Made an unavailable `previous_response_id` return Codex's standard retry
  event without reading local history, allowing Codex to resend the same turn
  as a full logical request.
- Limited rollout reconstruction to full external-destination requests that
  contain unreadable native opaque compaction state. Native destinations and
  portable EMP checkpoints remain reader-free.
- Implemented Codex 0.149 remote-compaction-v2 reconstruction from the latest
  `replacement_history` plus the successful tail, with fail-closed handling of
  unresolved opaque state.
- Made Context Guard the only final destination-payload budget decision. An
  oversized payload is compacted for that destination, re-projected, and
  checked once more before sending.
- Preserved current request window identity across long-lived WebSockets after
  native compaction while keeping thread identity conflicts strict.
- Kept standalone web search on Codex's Subscription-backed tool path so an
  external model can search without receiving Provider or Subscription
  credentials.
- Treated client-cancelled HTTP streams as normal disconnects: EMP now closes
  the upstream iterator without logging an internal 500 or writing a second
  response to an already closed socket.

### Verification

- Passed the complete offline suite for the P0 change set. After the final
  isolated WebSocket window-identity correction, its focused continuity,
  history, context, loopback, compilation, and whitespace checks also passed.
- Live-validated first-send Native -> External, External -> External, and
  External -> Native transitions after compaction, including tools, image
  input, standalone search, compressed large requests, later WebSocket turns,
  and native resume on Codex 0.149.
- Confirmed EMP does not write Codex SQLite or rollout files and does not put
  conversation content in its diagnostic journal.

## 0.7.6 (2026-08-25)

- Added destination-model hierarchical compaction, stricter stream terminal
  handling, and Subscription-backed standalone search for external models.
- Kept derived checkpoints memory-only and diagnostics content-free while
  preserving Codex-owned threads and resume state.

## 0.7.5 (2026-08-24)

- Replaced the legacy continuity layer with read-only Codex App Server,
  SQLite-locator, and rollout history adapters.
- Reconstructed visible history only for external handoffs across an opaque
  native compaction boundary; native routing retained Codex's own compaction
  and retry behavior.
- Simplified model presentation and management controls without introducing an
  EMP-owned history database.

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
- Made translated Chat and Anthropic streams require formal terminal markers,
  reject unknown finish states, preserve parallel tool turns, and keep
  fragmented or sparse tool calls type- and index-stable.
- Bound native upstream WebSockets by absolute time and cumulative bytes, made
  authentication/rate-limit failures terminal, and measured first-event latency
  from the actual upstream attempt rather than local preparation.
- Rejected symlinked credential-key paths and made Web UI account/model edits
  commit atomically so a failed save cannot corrupt browser-side state.

### Verification

- Passed the focused v0.6 modules and the bounded complete suite with 813 tests;
  the two existing opt-in live Provider/Codex checks remained skipped.
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
