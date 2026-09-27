# EMP Handoff

## Active security repair handoff — 2026-09-27 UTC

This section is the current resume point. The dated records below it are
historical. Xian authorized Point to continue the interrupted review with
Luna 6 **max** workers and to preserve enough progress for Opus to take over.
Do not restart the Rust rewrite or discard the existing work.

### Coordinator and recovery

- Integration checkout: `EasyMultiProvider-rust`, branch `security-fixes`,
  starting at `2c35c8f`. Point owns integration, review, and this active section.
- Six existing `emp-sec-*` worktrees contain four completed local commits,
  48 modified tracked files, and two untracked files. None was integrated at
  handoff start. Preserve all of them, including partial implementation.
- Baseline patches, HEADs, status, and untracked files were copied to ignored
  `artifacts/security-recovery/20260927T052409Z/` in the integration checkout.
  These are local recovery data; never publish credentials or private logs.
- Each worker updates **its own worktree's copy of this existing handoff file**
  before substantial work and after each tested change. Record exact commits,
  commands/results, remaining work, and the next action. Point consolidates
  the results here; do not concurrently edit the coordinator's copy.
- New commits use the current GitHub author (`h2q`) and the exact trailer
  `Co-authored-by: Point <point@local.invalid>`. Do not rewrite prior commits,
  force-push, release, or start GitHub workflows from a worker.

### Runtime, models, and quota policy

- Python EMP is the control service on port **4200**. It must remain running.
  Rust on **4201** is a separate test service. Point may stop/restart Rust as
  needed; workers use their own temporary test listeners and never touch 4200.
- Use the app-server task tools, with `gpt-6-luna` and effective effort `max`.
  Do not substitute low/high, another model, or a `codex exec` worker.
- When the current native account has about **10% or less remaining** in any
  applicable finite quota window, change every Luna task to
  `egg/gpt-6-luna`, still at `max`. Preserve task history and worktree state.
  Keep this account choice for subsequent tasks; do not spend reset credits.
- Coordinator checks quota before launch and periodically during supervision.
  An unavailable/stale quota is not evidence of headroom. If the egg route is
  unavailable, preserve progress and report the specific missing route rather
  than silently using a different model or account.
- Initial Python management API snapshot: native primary used 7%, secondary
  used 12% (remaining 93% / 88%). Xian subsequently reimported egg; both its
  credential-set flag and `egg/gpt-6-luna` catalog route are now present.
  No quota reset was performed.

### Work ownership and sequence

At most three implementation workers run at once. Use the existing worktrees;
do not create additional worktrees or new per-worktree Cargo caches.

| Task | Worktree / branch | Starting state | Owner status |
| --- | --- | --- | --- |
| HTTP | `emp-sec-http` / `sec/http` | 29 changed files; header-auth backend, UI still on cookie/EventSource; not integrated | Running, Luna/max verified |
| ROLLOUT | `emp-sec-rollout` / `sec/rollout` | `2abc9fe` metadata validation; other reader fixes incomplete | Running, Luna/max verified |
| TRANSPORT | `emp-sec-transport` / `sec/transport` | `24d87f1`, `bda4092`; unfinished context classifier and undefined `may_carry_context_error` call | Running, Luna/max verified |
| CONTEXT | `emp-sec-history-ctx` / `sec/history-ctx` | Compaction/calibration drafts plus `tests/tmp_baseline.rs` | After an initial worker finishes |
| STATE | `emp-sec-state` / `sec/state` | Migration v2, origin-bound secrets, bounded filesystem drafts; quota persistence unfinished | After an initial worker finishes |
| CI | `emp-sec-ci` / `sec/ci` | `d7ebcc0`; runtime inventory trust draft | After an initial worker finishes |

**HTTP assignment:** Own `emp-app` HTTP/auth/web, request lifecycle, desktop
launcher, the migration/quota management adapters, relevant app tests, and
`easy_multi_provider/web/index.html`. Complete bootstrap/header authentication
end-to-end, including all browser requests, downloads and quota events. Remove
dynamic-data interpolation into executable inline handlers. Export confirmation
must be session/operation-bound, short-lived and single-use; it is not a claim
of independent reauthentication. Complete safe path parsing, common response
headers, Host checks and total request read deadlines. Keep Codex caller auth
working. Coordinate the export v2 minimum (12 UTF-8 bytes) with STATE while
retaining legacy v1 imports (8 bytes). Do not redesign the UI or its layout.

**ROLLOUT assignment:** Own `emp-codex/src/history.rs`, `history/**`, its
contract tests and only necessary `emp-codex/Cargo.toml` dependency changes.
Review `2abc9fe`, then converge reverse/full parity using the existing walker,
shared control-state scan and replay engine. A checkpoint cannot discard
earlier turn-success, model, role/dedup, identity/mode or ordinal information.
Verify partial-window and non-zero frozen-prefix cases on identical inputs.
Validate SQLite/session history modes. Restrict ancestor lookup to legitimate
session roots without recursive symlink traversal or arbitrary first matches.
Add bounded `.jsonl.zst` full replay using Codex's uncompressed byte-offset
semantics. Reject invalid lineage bounds and distinguish depth from cycles.
Keep modules cohesive; do not create another giant history implementation.

**TRANSPORT assignment:** Own `emp-transport`, `emp-router`, `emp-protocol`,
`emp-core` and their relevant tests. Review the two existing commits; finish
the context-error draft and its incomplete call site. Bound error observation
cost without losing normal context errors or changing terminal/retry truth.
Check streaming order, compression handling, cancellation and pre-output-only
retry. Prevent unknown/disabled account prefixes silently falling into a
forward route; retain explicitly configured forwarding. Avoid forwarding
credentials across unapproved origins or redirect downgrades. Finish Gemini
metadata redirect handling and Windows registry tool path handling. Coordinate
origin/credential configuration changes with STATE rather than both editing it.

**CONTEXT assignment:** Own `emp-history/**` and app services
`{context,history,compaction}.rs`. Review and finish incremental compaction
estimation against the existing oracle, preserving retained items, tool pairs,
summary requests and error results. Calibration must distinguish input/output
budgets and actual deployment identity; repeated uncorroborated errors alone
must not shrink every session's window. Preserve hot reload semantics. Convert
useful existing baseline evidence into readable outcome tests, keeping recovery
copies of scratch until its useful content is retained.

**STATE assignment:** Own `emp-state/**`, except any explicitly coordinated
provider routing edit, plus `emp-codex/src/quota{.rs,/process.rs}`, app services
`{accounts,quota}.rs`, `emp-integration/src/lib.rs` and relevant tests. Review
the existing migration v2, fixed KDF cost, origin comparison and bounded-file
drafts. Preserve v1 import compatibility. Finish durable recovery when token
rotation succeeds but saving fails, private temporary storage and atomic config
writes. Avoid broad path checks that reject normal macOS/Windows installations.
Clear/rebind secrets only on a meaningful origin change, not `/v1` cleanup.
Record any intentional security contract changes instead of weakening the
archived Python oracle to hide a difference.

**New user-reported STATE priority — quota history retention:** Xian removed
egg after OAuth expired, then reimported it and found the trend empty. Both
Python `AppState._delete_account` and Rust `delete_account_state` call the
history store's delete operation. The live store currently has only two new
egg samples; a separate older local store has 406 egg samples. Point saved
read-only SQLite snapshots under the ignored recovery directory before any
recovery. Do not automatically clear historical usage when removing credentials.
Reimporting the same upstream account should reconnect its history; reusing a
display name for a different account must not attach the prior account's data.
Use stable account ownership, keep history within its existing retention policy,
and preserve explicit deletion semantics as a separate operation if needed.
This fix includes the Python source in `EasyMultiProvider` and the Rust source,
with focused removal/reimport and identity-separation tests. Do not restart the
Python service. Point handles any live history restoration after identity review.

**CI assignment:** Own `.github/**`, packaging scripts and
`emp-codex/src/runtime_inventory/**` with their tests. Review `d7ebcc0`; finish
runtime candidate validation without breaking supported installations. Verify
release gates and provenance configuration locally. Do not start workflows,
publish, change repository environments, or invent signing credentials.
Provenance generation is separate from updater signature verification; record
the remaining trust/configuration prerequisites explicitly.

### Verification and integration rules

- Review before editing. Preserve unrelated work. Only use local synthetic
  fixtures for boundary/failure checks; no third-party security probing.
- Run targeted tests for changed behavior. Do not run the full workspace per
  worker or after every small change. Point runs final format, strict Clippy,
  workspace and consumer E2E once the implementations are integrated.
- Serialize Cargo work to control storage/RAM:
  `flock -x /home/fumo/codex_ws/agent_dev/.emp-security-cargo.lock env CARGO_TARGET_DIR=/home/fumo/codex_ws/agent_dev/.emp-rust-target-root CARGO_BUILD_JOBS=2 cargo ...`
- Python oracle: `EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider`
  and `EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python`.
  Use the pinned Python semantics for unchanged workflows; document deliberate
  authentication/migration changes and use local Codex source for new history
  forms that the archived Python does not support.
- Workers commit reviewed source and their own progress record locally. Point
  reviews and integrates; no worker merges into another worker's branch.
- Earlier one-time Runtime/Package authorization was already used at
  `5fa6a64` and both passed. Latest `main@041f000` Runtime passed, while Package
  failed a Windows CRLF assertion; `685253a` fixes the assertion in this branch.
  There is no CI evidence for these pending security fixes. Do not trigger a
  new run without corresponding authorization.
- Final acceptance includes the working browser, authenticated management,
  history/compaction/model switching/subagent, cancellation, persistence,
  package lifecycle and required cross-platform proof. Model canaries use
  authorized accounts/providers and lowest reasoning for the *test* calls;
  implementation workers remain Luna **max**.

### Latest coordinator checkpoint

- Latest Xian clarification: only the root Point thread should stop active
  implementation. Luna workers continue. Root is limited to analysis, dispatch
  and review. Prior pause messages were explicitly revoked. Worker queues are
  HTTP → STATE (including Python/Rust trend retention), ROLLOUT → CONTEXT,
  TRANSPORT → CI. Workers check quota and switch to egg Luna/max at <=10%.
  Root leaves the workers running and reviews their completed results later.

- Baseline saved; HTTP, ROLLOUT and TRANSPORT app-server workers started.
  Point verified all three in both thread metadata and actual `turn_context`:
  `model=gpt-6-luna`, `effort=max`. Generic system wording "GPT-6" is not
  contrary evidence. Local task IDs are in the ignored recovery manifest.
- Python 4200 and Rust 4201 both observed running. Existing TCP connections
  were to 4200, with none observed to 4201. Rust's old executable and temporary
  cwd are marked deleted; recreate an isolated installation when restarting it.
- Next: supervise targeted work, prioritize STATE/history retention as the next
  available worker, and verify/restorably merge older quota samples if ownership
  agrees. If Codex quota runs out, Opus resumes from this
  section and the corresponding worktree progress without reverting drafts.

---

# Historical EMP Handoff — 2026-09-25

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
