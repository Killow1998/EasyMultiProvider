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

When projecting an inherited checkpoint, explicit Codex items keep their places.
EMP matches overlapping records by item type and ID. A client tool call and its
output can come from different sides of the checkpoint. EMP pairs them across
the combined request before filling missing results. If the destination needs
another compaction, a final assistant answer followed by new user input marks
a completed turn in the supplied tail. Earlier turns can enter the summary.
Instructions and tool activity after that answer stay with the current turn.
A tool exchange spanning the cut moves the boundary back to its call.
A completed server-side tool search contributes its tool declarations to external
protocols without creating a client tool invocation.

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

A Windows loopback replay with a frozen parent history reproduced the v0.15.7
HTTP 413 before the mock upstream received any calls. With completed turns
eligible for compaction and a 467,368-token window, the same history reached
the mock through 18 summary calls and one final request with intact tool pairs.
EMP keeps a bounded in-memory cache of summaries. Reuse requires the same
thread lineage, destination route, input budget and an exact match for the
complete summarized prefix. A follow-up keeps its recent turns, and a sibling
can reuse the common prefix without importing messages from the other branch.
Restarting EMP clears the cache. Its summaries never modify Codex rollouts.

Codex can pin a request's initial reasoning effort and send subsequent changes
as ordered `configuration_update` input items. For Native routes EMP preserves
these items. For external routes it applies the last supplied effort to the
request before history reconstruction, context estimation and protocol conversion.
The destination's existing effort and summary policies still apply.

An isolated Codex 0.171.0 app-server ran two turns with effort set to `low`
and then `high`. With effort updates enabled in the client and model catalog,
the Native mock received a `low` baseline plus an ordered `high` update on
the second turn. The Chat Completions mock received `reasoning_effort: high`.

A Windows loopback replay included all 552 inherited items and synthetic side
questions. The top-level effort was deliberately set to `low`. Both archived
controls specified `xhigh`. Before the fix, EMP returned 422 before reaching
the mock. After the fix, all three final Chat Completions requests carried
`xhigh` and paired tools. The first needed 18 summary calls, the follow-up
and sibling needed zero, and the sibling contained only its own branch markers.

## Official source references

Reviewed at `rust-v0.153.4`; the fork metadata fields also exist in `rust-v0.152.1`:

- [Side chat lifecycle and boundary](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/app/side.rs)
- [Side fork request](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/app_server_session.rs)
- [Responses fork metadata](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/responses_metadata.rs)
- [Ephemeral app-server contract](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/README.md#lifecycle-overview)

Codex 0.171.0 restores replacement history before replaying its suffix and
pairs client tool calls over the resulting timeline.
[Rollout reconstruction](https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/session/rollout_reconstruction.rs)
and [tool normalization](https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/context_manager/normalize.rs).

Codex pins and updates effort in [session reasoning settings](https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/session/reasoning_effort.rs).
The [request builder](https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/core/src/client.rs#L1199-L1215)
filters control items for unsupported models and deduplicates adjacent updates for supported ones.
