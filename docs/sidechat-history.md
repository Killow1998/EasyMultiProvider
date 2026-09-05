# Ephemeral Side chat history

Codex CLI's `side_fork_config` creates an ephemeral fork. `fork_side_thread`
uses the normal fork operation, and the client inserts a reference-only boundary
before the side question. Ephemeral threads need not have a SQLite row or rollout.
Starting another app-server process cannot recover the original process's memory.

EMP previously required the child thread's disk history when translating native
opaque compaction to an external provider, producing `thread_missing` before any
upstream request. This differs from an upstream 502.

For a missing child with explicit `forked_from_thread_id` in Codex turn metadata,
EMP resolves only that parent's rollout in the configured Codex home. It requires
one exact match for the inherited `encrypted_content` in a persisted compaction
replacement. It normalizes only the prefix through that checkpoint, including a
checkpoint committed during a still-active turn. The incoming request supplies
all post-checkpoint items, including the side boundary and current question.
No parent-tail merge, transcript mutation, or heuristic parent search is used.
Native destinations retain their original opaque history.

Missing parent metadata, unavailable parent history, and absent/ambiguous checkpoint
identities still fail closed. A new native compaction created only inside the
ephemeral child cannot be recovered from the parent by this mechanism. EMP cannot
decrypt it or obtain it by launching a separate app-server. These limitations must
not be reported as successful reconstruction. Diagnostic events carry fixed reason
codes and pseudonymous references, never ciphertext or conversation text.

Tests cover inherited checkpoints after the parent has advanced/compacted again,
active-turn checkpoints, preserved side instructions, missing or malformed parent
metadata, ambiguous/missing checkpoints, cross-home paths, and native pass-through.

Validation: 977 suite tests completed with 11 conditional skips. On Intel macOS,
30 focused history tests passed, and both persisted checkpoints from the reported
parent rollout were reconstructed locally with no upstream calls or history
changes. That replay supplied synthetic official-format fork metadata, so it does
not substitute for a desktop retry. Subsequent Intel macOS desktop verification
completed an Astra reply in the existing long conversation and a reply in a new
Side chat without the previous history-reconstruction or item-ID errors.
Updated Windows and Intel macOS packages passed packaging checks; the Mac DMG
also passed packaged update and rollback scenarios.

## Official source references

Reviewed at `rust-v0.153.4`; the fork metadata fields also exist in `rust-v0.152.1`:

- [Side chat lifecycle and boundary](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/app/side.rs)
- [Side fork request](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/app_server_session.rs)
- [Responses fork metadata](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/responses_metadata.rs)
- [Ephemeral app-server contract](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/README.md#lifecycle-overview)
