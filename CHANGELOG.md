# Changelog

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
  protocols, including text-only and disabled-tool modes.
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
