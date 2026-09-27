# EMP Transport Security Handoff — 2026-09-26

## Active transport assignment

- Worktree: `/home/fumo/codex_ws/agent_dev/emp-sec-transport`
- Branch: `sec/transport`
- Verified worker turn context: `gpt-6-luna`, effort `max`; the coordinator
  independently verified the same active-thread metadata. The parent confirmed
  quota is above the switch threshold; coordinator owns any account switch.
- Starting HEAD: `bda4092` (`Scan SSE line buffers linearly across chunks`),
  following `24d87f1` (`Stop permessage-deflate decompression spinning on stream end`).
- Preserve the existing uncommitted context classifier and SSE call-site drafts.
  At the initial checkpoint, only these files were modified:
  `crates/emp-protocol/src/context_error.rs` and
  `crates/emp-router/src/native_http/stream.rs`.
- Current Git author is `h2q`; new local commits must include
  `Co-authored-by: Point <point@local.invalid>`.
- Do not push, start CI, create worktrees, or stop Python port 4200. Serialize
  Cargo commands using the shared lock and target directory specified in the
  active security handoff in the integration checkout.

## Scope and next action

The transport implementation and targeted validation are complete. The next
step is the final diff review and local commit of source plus this checkpoint.
Then continue Xian's authorized CI assignment in the existing `emp-sec-ci`
worktree; do not push or start workflows. No shared origin/credential config
was changed, so no STATE edit was needed.

### Work log

- Initial checkpoint: confirmed effective model/effort, branch, existing
  commits and dirty draft scope. No implementation changes or tests run yet.
- Next: inspect the existing transport diffs and adjacent behavior, then make
  the smallest scoped fixes and record each targeted validation here.
- Resume: Xian revoked the pause and authorized completion through a tested
  local commit. Coordinator confirms active `gpt-6-luna/max`, native remaining
  quota 58% / 83% and egg 99% / 84%; coordinator owns threshold-based account
  switching. Refreshed the coordinator scope on 2026-09-26; no tests have run.
- Next: complete `may_carry_context_error` and the bounded classifier, then
  repair account-prefix fallback, credential-safe Gemini redirects and Windows
  registry tool lookup. Review streaming/retry/cancellation coverage and test
  only changed behavior using the shared Cargo lock.
- Validation: `cargo test --offline -p emp-protocol --test
  context_error_python_oracle explicit_context_error` passed (2/2), including
  the configured Python oracle. The initial `--locked` attempt correctly
  reported that adding the existing `regex` dependency required a lockfile
  update; the offline rerun updated `Cargo.lock`.
- Validation: `cargo test --locked -p emp-protocol --lib context_error` passed
  (3/3), covering evidence text, node and depth limits with adversarial JSON.
- Validation: `cargo test --locked -p emp-core --test route_python_oracle
  native_forward` passed (2/2). The security change intentionally rejects
  unknown/disabled slash-qualified fallback routes while preserving bare model
  forwarding and explicitly configured slash-qualified models.
- Validation: `cargo test --locked -p emp-router --lib context_error` passed
  (2/2), confirming ordinary output events are untouched and terminal context
  errors retain the 413 mapping. Initial build exposed one unused platform
  import; it is now cfg-gated to Windows/tests.
- Validation: `cargo test --locked -p emp-transport --test http_client
  followed_redirects_stay_on_the_initial_origin` compiled but could not start
  its loopback fixture in the default sandbox (`PermissionDenied` on bind).
  Retry this targeted test with approved local-network access; no assertion has
  failed yet.
- Validation: after approval-gated local-network access, the same targeted
  redirect test passed (1/1). Same-origin redirects retain credentials;
  cross-origin redirects return the 3xx without sending a second request.
- Validation: `cargo test --locked -p emp-transport --lib
  redirect_origin_rejects_host_port_and_scheme_changes` passed (1/1), including
  the HTTPS-to-HTTP downgrade case.
- Validation: `cargo test --locked -p emp-transport --lib
  windows_registry_tool_uses_absolute_systemroot_path_with_spaces` passed
  (1/1), covering absolute roots, spaces and missing/relative roots on this host.
- Validation: after tightening array traversal to stop at the node budget,
  `cargo test --locked -p emp-protocol --lib context_error` passed again (3/3).
- Review of `24d87f1`: `cargo test --locked -p emp-transport --lib
  deflate_stream_end_terminates_instead_of_spinning` passed (1/1), covering
  BFINAL termination, next-message reset and trailing bytes.
- Review of `bda4092`: `cargo test --locked -p emp-transport --test
  sse_parser` passed (7/7), covering split chunks, CRLF, multiline frames,
  completion markers and bounded unterminated lines.
- Review of `bda4092`: `cargo test --locked -p emp-router --lib
  sse_lines_split_across_every_byte` passed (1/1), confirming native event and
  wire-frame order is stable across byte-at-a-time chunks.
- Transport cancellation review: the targeted streaming-before-EOF test passed
  (1/1) with approval-gated loopback access; dropping a partial stream forces a
  fresh connection for the next request.
- Retry review: `native_complete_http_matches_python_retries_errors_and_wire_requests`
  passed (1/1) against the live Python oracle, covering pre-output retries,
  account refresh and terminal error classes.
- Streaming retry review: `native_stream_http_matches_python_events_headers_and_retry_decisions`
  passed (1/1) against the live Python oracle. The stream route preserves event
  order and terminal/retry behavior without replaying after output.
- Route compatibility: `cargo test --locked -p emp-core --test
  route_python_oracle route_resolution_matches_live_python_oracle_when_configured`
  passed (1/1); existing Python route fixtures remain unchanged by the stricter
  fallback rule.
- Gemini metadata: the new `gemini_metadata_follows_same_origin_redirect_with_api_key`
  targeted test passed (1/1). The configured Google API-key header is retained
  across a same-origin redirect; the transport test confirms cross-origin and
  HTTPS-downgrade redirects are not followed.
- Validation: the complete targeted `provider_discovery_python_oracle` test
  binary passed (3/3), including Gemini metadata redirect, normal Gemini model
  discovery, Anthropic pagination and the Python projection oracle.
- Validation: `cargo test --locked -p emp-transport --test http_client` passed
  (10/10), including redirect origin checks, no replay on dropped POSTs,
  cancellation, TLS policy and the live Python socket oracle.
- Formatting and diff checks: isolated `rustfmt --check --edition 2024
  --config skip_children=true` passed for all touched Rust files; `git diff
  --check` passed. `cargo fmt` is not installed in this environment.
- Windows cross-check: `cargo check --locked --offline --target
  x86_64-pc-windows-msvc -p emp-transport --lib` could not reach this crate;
  `aws-lc-sys` attempted to use GNU `cc` for an MSVC target and failed on
  `pthread_rwlock_t`. The Windows path helper unit test passed on Linux, but
  Windows compilation still needs an MSVC-capable environment.

### Pause checkpoint — 2026-09-26

- Changed files: the pre-existing drafts in
  `crates/emp-protocol/src/context_error.rs` and
  `crates/emp-router/src/native_http/stream.rs`; this handoff copy was updated.
  No implementation files were changed in this turn. Commits `24d87f1` and
  `bda4092` remain intact.
- Tests run: none.
- Unfinished next step: define and test `may_carry_context_error`, then finish
  the assigned route-prefix, credential/redirect, Gemini metadata and Windows
  registry-path review. Initial review found the Gemini metadata request follows
  redirects with `x-goog-api-key`, and Windows invokes `reg` through PATH.

---

# EMP Handoff — 2026-09-25

## Current branch and scope

- Repository: `/home/fumo/codex_ws/agent_dev/EasyMultiProvider-rust`
- Branch: `remake4rust`
- Base HEAD: `5fa6a64a8caf9496a8bdef435d3c8a69a203f421`
- Issue: [EasyMultiProvider #14](https://github.com/Killow1998/EasyMultiProvider/issues/14)
- Active task: Phase A repair for oversized Codex rollout history reconstruction.
- Working tree contains uncommitted changes in:
  - `crates/emp-codex/src/history.rs`
  - `crates/emp-codex/src/history/records.rs`

## Completed repair

The Rust reader no longer loads the entire rollout with `fs::read`. The original
128 MiB whole-file rejection is removed.

The reader now performs bounded forward streaming:

1. Canonicalize the rollout path and verify that it remains inside the Codex home.
2. Open the rollout once and use the same file handle.
3. Capture `captured_end` from that handle.
4. Keep a hard scan ceiling of 2 GiB.
5. First pass scans metadata, thread identity, successful turns, response roles,
   model context, ordinals, and exact compaction matches.
6. Second pass streams records before the computed history boundary and rebuilds
   visible history.

New bounds:

- `MAX_ROLLOUT_LINE_BYTES = 64 MiB`
- `MAX_ROLLOUT_SCAN_BYTES = 2 GiB`

A single JSONL line above 64 MiB returns `history_record_too_large` instead of
reading it unbounded. A rollout above 2 GiB still returns `source_too_large`.

## Semantics preserved

- Thread identity conflict and mismatch are still rejected.
- Failed turns remain excluded from visible history.
- Incoming-turn history boundary remains respected.
- Paginated mode still requires an ordinal.
- Exact compaction lookup still rejects missing or duplicate ciphertext.
- Partial trailing JSONL data is ignored as an unfinished prefix.
- The file handle is used with a frozen prefix rather than relying on repeated
  path metadata reads.

## Tests performed

All of the following passed:

- `cargo test -p emp-codex history -- --nocapture`
- `EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider cargo test -p emp-app history`
- Same Python environment variables with `cargo test -p emp-app conversation`

Added regressions cover:

- Rollout larger than the old 128 MiB limit, using bounded streaming.
- Oversized JSONL line returns `history_record_too_large`.
- Partial trailing JSONL is ignored.
- Duplicate exact compaction ciphertext returns `compaction_identity_ambiguous`.
- Paginated rollout without ordinal returns `ordinal_missing`.

`cargo fmt` passed with the isolated Rust toolchain after formatting the touched files.

## Latest verification

The isolated Rust toolchain was used for the CI-equivalent strict check:

```bash
PATH=/home/fumo/codex_ws/agent_dev/.emp-rust-toolchain/rustup/toolchains/1.93.1-x86_64-unknown-linux-gnu/bin:$PATH cargo clippy --locked --workspace --all-targets -- -D warnings
```

This passed.

The full workspace test suite was then run with:

```bash
export EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python
export EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider
cargo test --locked --workspace --all-targets
```

It reached 111/112 tests passing. The sole failure was
`tests::performance_contract::responses_endpoint_records_schema3_full_request_tps_and_preserves_stream_timings`,
which failed a timing assertion under parallel load. When rerun alone, the same
test passed. This is not related to the history reconstruction changes, but a
clean full-suite rerun is still recommended before commit.

A complete real 523 MB rollout end-to-end request remains to be retested.

## Not included in Phase A

These remain for Phase B/C and must not be mixed into the current small repair:

- Reverse JSONL scanner and paginated reverse checkpoint selection.
- Rollout lineage, history base, and multi-segment forks.
- `ThreadRolledBack` reconstruction.
- `thread_history_1.sqlite` projection acceleration.
- Compressed rollout segment support.
- Python-side equivalent repair or explicit Rust-only production cutover.

## OMP development setup

OMP was installed successfully:

```text
/home/fumo/.local/bin/omp
version: v18.3.1
```

OMP is configured to use EMP's external `chuang` provider directly. Credentials
were copied from EMP's local encrypted vault into the private OMP model config;
do not print or commit the key.

Config locations:

- `~/.omp/agent/models.yml`
- `~/.omp/agent/config.yml`

The configured model is:

```text
chuang/gemma-3.1
```

It is assigned to both the default and vision roles. OMP reports image input as
supported. The model config declares:

```yaml
input: [text, image]
```

Minimal end-to-end check passed:

```text
prompt: Reply with exactly: OMP-EMP-OK
result: OMP-EMP-OK
```

Use OMP from the EMP Rust repository for the next development session:

```bash
cd /home/fumo/codex_ws/agent_dev/EasyMultiProvider-rust
/home/fumo/.local/bin/omp
```

## Immediate next actions

1. ~~Run strict Clippy using the isolated toolchain.~~ Done: `-D warnings` clean.
2. ~~Run the full workspace suite with the Python oracle variables set.~~
   Done: all crates pass; the only failure inside full-workspace parallel
   runs is the pre-existing `performance_contract` timing flake (passes
   standalone and in crate-scoped runs).
3. ~~Re-test the real 523 MB rollout through the Rust service.~~ Done:
   200 OK in 4.4 s, 9,945 messages, no opaque tokens leaked, 223 MiB RSS.
4. ~~Review the diff and commit Phase A once all CI-equivalent checks
   pass.~~ Done: `2218a10 Stream Codex rollouts with bounded history
   reconstruction`.
5. ~~Start Phase B: bounded reverse scan and Codex-style checkpoint
   replay.~~ Done: `581c789 Add reverse scan and checkpoint replay for
   paginated resumes`.

## Phase B result and known limits

- Reverse locate scans doubling windows from the tail (16 MiB steps,
  64 MiB cap); a self-contained newest compaction (window-numbered,
  opaque-free `replacement_history`) becomes the replay base.
- All 63 compactions in the real 523 MB rollout carry an opaque
  compaction item, so today's real Codex files always take the
  full-scan fallback (~4.4 s end-to-end). Base replay wins only for
  opaque-free checkpoints; keep it for the resume contract when Codex
  stops emitting opaque references.
- Remaining Phase B ideas (not started): SQLite `threads`/turn
  projection pushdown for anchor lookup, and streaming the suffix
  replay directly into the provider request instead of materializing
  the full visible vector.

## Follow-up investigation: 429 retry policy vs OMP (oh-my-pi)

OMP retry (extracted from the bundled `pi-ai` runtime):

- `retry.enabled=true`, `maxRetries=10`, `baseDelayMs=500` with
  exponential `2**attempt`, per-delay cap 8 s, ±25% jitter,
  `maxDelayMs=300 s`.
- `Retry-After` honored in both integer-seconds and HTTP-date forms;
  retry fires when `Retry-After` is present OR the error class is
  Transient (`>=500`, `408`, `429`, network/timeout regex) OR
  UsageLimit (`429`/`402`, `CONCURRENT_LIMIT`).
- `ContextOverflow` is never retried; `retry.waitForUsageReset` sleeps
  until `x-ratelimit-reset-*`; usage-aware fallback reserves 10%
  (`usageReservePct`); `retry.fallbackChains` provides ordered
  role/provider/model fallback chains (`modelFallback=true` default).

EMP (crates/emp-transport/src/failure.rs,
crates/emp-app/src/services/failures.rs): a single retry
(`attempt in 0..2`), only 429 with reason `rate_limited` (never on
`:free` routes) or 504, `Retry-After <= 5 s`, sleeps exactly
`Retry-After`; 429 quota/capacity/balance reasons are terminal; no
backoff curve; protocol fallback instead of model fallback. EMP is
stricter mid-stream: retries only before any output/tool activity.

Parity gaps, cheapest first:

1. Honor `Retry-After` up to ~300 s instead of rejecting above 5 s.
2. Add exponential backoff + jitter when `Retry-After` is absent
   (500 ms base, 8 s per-delay cap) for 429/504/5xx before output.
3. Treat 429 `upstream_capacity` as failover-to-next-candidate rather
   than terminal when more candidates exist.
4. Optionally raise the external retry budget for non-streamed
   requests from 1 retry to 2–3.

## Follow-up investigation: metrics and pricing vs OMP

- TTFT: EMP records `ttft_ms`, `generation_ms`, `duration_ms`,
  `tokens_per_second` (schema 3 diagnostics); OMP exports only
  `gen_ai.response.time_to_first_chunk` via OTel. EMP surface is
  richer; nothing to port.
- Cost: OMP computes `per_million_rate * tokens / 1e6` with a 2x-input
  rule for 1h cache writes; EMP
  (crates/emp-state/src/usage/pricing.rs) uses decimal per-token rates
  with tier suffixes (`_priority`, `_flex`), threshold tiers
  (`_above_Nk_tokens`), 5m/1h cache-write split, and reasoning-token
  rates, producing `cost_nanos`. EMP is strictly richer; OMP has no
  equivalent to port.
- Routing: OMP resolves roles plus `retry.fallbackChains` (model and
  provider wildcards); EMP routes via per-request candidates and
  protocol fallback. Role-based chains are the only missing concept.

## Follow-up session results (2026-09-25, later)

1. OMP-aligned 429/504 retry shipped (`52be217`): `Retry-After` honored
   to 300 s, exponential backoff (500 ms base, 8 s cap, ≤25% jitter)
   when absent, capacity 429s retried alongside rate limits, quota
   exhaustion still terminal, free routes still never retry. Budget is
   3 pre-output attempts; mid-stream stays single-failure.
2. Gemma 3.1 CoT: reasoning stays separated from `response.output_text`
   in both Python and Rust stacks (23 reasoning deltas + 1 answer delta
   verified live per fresh turn); protocol tests pin complete/stream/
   late-reasoning separation.
3. Responses WebSocket external execution: an external route inside the
   Responses-WS endpoint executes over the provider's HTTP/SSE
   transport (the fallback) and now races upstream events against a
   downstream DisconnectMonitor (`1758108`). Previously a stalled
   upstream kept the worker and its HTTP stream alive after the codex
   client vanished. Contract test drops the WS after `response.created`
   and asserts the SSE upstream observes the closed connection.
4. Perf flake fixed (`b099a38`): upstream deltas can coalesce in one
   socket read, so `generation_ms >= 100` was not deterministic; the
   contract now asserts `ttft >= 100ms`, `generation <= duration`, and
   `duration >= 200ms`.
5. Full workspace suite green (112 app-lib + all crates), strict Clippy
   `-D warnings` clean, `cargo fmt` applied.
6. Phase B replay regressions pinned (`2e2dc0d`): an eligible-looking
   paginated compaction whose replacement history contains an opaque
   compaction item is rejected as a reverse base (full scan owns that
   case), and a `thread_rolled_back` event after the checkpoint base
   truncates the replayed suffix exactly like the full scan.
   `cargo test -p emp-codex --lib` 20/20, `emp-app history` and
   `conversation` green, strict Clippy `-D warnings` clean, fmt applied.
   Real 523 MB rollout retested end-to-end through the Rust service:
   reverse locate ~2.4 s, opaque gate forces full-scan fallback, request
   replayed (55.7 MB projected request, 256 items) and served an upstream
   429 rate-limit response; the service stayed alive and healthy
   throughout. Host OOM pressure (7.6 GB RAM, swap full, `oom_kill`
   events in the user slice) killed earlier foreground test-server
   instances; run such retests from a detached (`setsid`) process.

## Follow-up session results (2026-09-26)

1. Unified replay rewrite (`d7e74e2`): `emp-codex/src/history.rs` replaced its
   four duplicated scan/replay loops (`scan_suffix`, the inline replay inside
   `snapshot_from_base`, `scan_rollout`, and the `read_rollout` closure) with
   one `walk_records` record walker, one shared scan observer, and one
   `replay_records` state machine seeded by a `ReplayFrame`. The whitebox unit
   test module (≈1400 lines) and the `force_full` strategy switch it existed to
   exercise were deleted; the differential guarantee now rests on the e2e
   contract suites plus the `emp-history` python oracle. Net −1481 lines.
   Verified: workspace 415 tests green (clippy `-D warnings` clean, fmt
   applied), 523 MB real rollout reads in ~3.0–3.6 s with identical item
   counts (10195) on both strategies before the switch was removed.
2. Ablation pass across modules (`9c41576`): deleted the dead
   `ResponsesValidationErrorKind` enum + accessor and `anthropic_error_kind`;
   single-sourced the protocol→field tables, part classifier, tool item and
   open-text event builders in emp-protocol; merged `wire_item` tool arms,
   deduped prepare openings, shared `CHECKPOINT_PREFIX`/`portable_encrypted`
   in emp-history; shared `read_http_head` and the frame length-prefix helper
   between websocket server/client, folded `SystemProxyCacheState`, collapsed
   `run_command` arms in emp-transport; folded the `quota_status_value`
   wrapper and restructured `read_lease` validation in emp-app/integration.
   Net −175 lines over 12 files. One oracle regression (suppressed reasoning
   blocks consuming output indexes after the block-state helper merged the
   ordering push) was caught by the anthropic python oracle and fixed in the
   same commit — the oracle suites are the load-bearing contract.
   Workspace 415 green, clippy clean, fmt applied.
