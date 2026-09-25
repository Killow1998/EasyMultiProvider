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
