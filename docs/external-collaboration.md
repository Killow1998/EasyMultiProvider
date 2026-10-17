# External models in Codex collaboration

Codex 0.153.4 distinguishes plaintext collaboration calls using an explicitly
empty `encrypted_function_args` list. Its V2 handler then emits a readable
`agent_message`. An omitted list is not equivalent to an empty list.

Native requests retain their official collaboration tools by default, including
when external services are configured. **Settings > Cross-service subagents**
has three modes, persisted as `external_collaboration`:

| Mode | Value | Behavior |
| --- | --- | --- |
| Off (default) | `"off"` | Only Native GPT subscription subagents are allowed. External delegation returns an error explaining where to enable it. |
| On-demand recovery | `"on_demand"` | Keep native collaboration tools. Recover an encrypted task with GPT-6 Luna, low effort, on its original subscription before forwarding to an external child. |
| On | `"on"` | Present plaintext collaboration tools to Native models, permitting external children without a recovery inference. Native generation can slow down. |

Ordinary external conversations and the separate Auto Review routing setting
are unaffected. Switching affects subsequent requests; an incremental Native
WebSocket turn recovers its full history on a fresh upstream socket when the
tool policy changes, so it cannot inherit the other mode's catalog.

In **On** mode, EMP presents collaboration tools to the
native upstream under an ordinary transport namespace, with plaintext message
parameters. Returned calls are restored to Codex's collaboration namespace and
marked as plaintext. This retains Codex's own dispatch, permissions and agent
lifecycle. It does not decrypt existing tasks or modify reasoning ciphertext.

This compatibility can reduce Native generation speed. In the issue #60 count
fixture, Codex 0.163.0 produced about 50 streaming-text TPS with the rewritten
collaboration tools versus 95 with the official tools, measured between the first
and last text deltas. A direct-upstream custom-schema
control showed the same lower rate, while ordinary additional tools did not
necessarily cause it. These measurements describe that fixture, not a universal
throughput limit. Merely changing proxy, buffering or compression settings does
not address the request-shape difference.

On cannot repair an already encrypted task: select On-demand recovery or send a
new task after selecting On. On-demand recovery is explicitly model-assisted
transcription, not local cryptographic decryption. It adds latency and usage;
reproduced text may differ from the original. The forced capture tool does not
execute the assignment. Incomplete, ambiguous or opaque output fails closed.

EMP binds a task to the actual subscription observed on its Native parent's
response. Recovery checks that the configured route still selects that same
account, including on cache hits. Missing origins, restarts, expired entries or
account changes require a new task; EMP does not substitute another account.
Only validated agent-message envelopes are recovered, never arbitrary encrypted
reasoning. A bounded memory cache retains up to 64 recovered tasks for one hour,
at most 256 KiB each, scoped by account, endpoint and full message envelope.
The recovery cache is memory-only. Recovery is limited to 45 seconds and follows client
disconnection. Each helper inference has its own `agent_task_recovery` call and
usage record; replaying a cached task does not duplicate that charge.

Both standard top-level `tools` and Responses Lite `input[].additional_tools`
are adapted. Codex builds the latter in `core/src/client.rs`; adapting only the
top-level field leaves Lite delegation encrypted. Altered additional-tools items
receive a deterministic new ID because Codex derives their identity from their
schema. WebSocket output restoration also applies to incremental requests that
omit inherited tool definitions.

The external Responses projection maps readable agent messages to user messages.
Unrecovered encrypted messages are rejected rather than sent as incomplete tasks.
Mixed-provider inherited histories can still encounter separate history
projection limitations; recovery does not claim to reconstruct an entire fork.

The question-mark help explains all three modes before its comparison table.
The comparison uses the same isolated Codex 0.163.0 subagent counting task
(integers 1 through 1024) and interleaves mode order. The parent is GPT-6.1 Sol at
low effort; children use low effort except GLM, which has no effort override.
Ratios compare each model with itself: Off is the GPT
baseline and On is the external baseline. External Off cells mean prohibited,
not zero throughput. Valid timing samples require the requested model, complete
output and exactly one completed EMP child request, with no failed or retried
model requests in the workflow. Completions after retries or extra child requests
are retained as functional results and excluded from speed ratios. TPS uses reported output tokens divided by
the full child request duration, matching EMP call records; it includes child
startup and first-token delay. Buffered Claude output is not treated as an
instantaneous token-generation rate. Workflow time covers parent dispatch,
recovery when needed, child execution and the parent's final acknowledgement.
Keep failed samples separate. Scheduling,
caching and task length affect ratios too; these are measurements, not a fixed
speed guarantee. No raw credentials or private histories belong in public
benchmark results.

On 2026-10-17, ten rounds covered all 11 requested models in all three modes:
330 unique model/mode/round combinations. The 40 external-Off attempts were all
rejected with the expected settings guidance and no external inference. Of the
290 permitted attempts, 271 completed the exact sequence; 267 also met the clean
timing criteria. Eighteen incomplete outputs came from GPT-5.6 Terra, and one
from GPT-6 Luna. Three Opus workflows completed after Claude process retries;
one GPT-5.6 Luna workflow completed with an extra child request. All 40 external
On-demand attempts used exactly one recorded recovery inference, including the
two that retried the child. No other mode or Native child invoked recovery.

The displayed ranges pair the same model and round, using Off as the Native
baseline and On as the external baseline. Terra has only one valid pair for each
comparison, explicitly marked as insufficient for a performance conclusion.
Three separate direct-upstream Terra controls reproduced one incomplete output
without EMP; they are not included in the 330 samples or ratios. This establishes
that the symptom also occurs without EMP, not its upstream cause. Model IDs
absent from the local catalog used a common GPT-6.1 Sol metadata template with
the requested ID retained, in both EMP and direct controls; these results do not
establish model-specific harness tuning.

Reference: [Codex 0.153.4 tool routing](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/tools/router.rs)
and [V2 communication](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/tools/handlers/multi_agents_v2.rs).
The Lite request layout is defined in
[client.rs](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/client.rs).

Validation includes transport regression tests, a live native parent plaintext
tool call, and Gemini 3.8 Flash high receiving an agent message and completing a
two-turn tool exchange. These checks are not a full desktop agent-tree lifecycle
acceptance test. Desktop validation subsequently confirmed fresh Gemini 3.7 Flash
and Gemini 3.8 Flash high subagents both completed plaintext delegation through
the packaged EMP, after the Responses Lite entry point was added.
Both children also completed follow-up tasks that used available time-reading
tools. This verifies delegation, follow-up and a tool round trip, not every
possible model capability or inherited-history combination.
