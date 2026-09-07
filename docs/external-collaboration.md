# External models in Codex collaboration

Codex 0.153.4 distinguishes plaintext collaboration calls using an explicitly
empty `encrypted_function_args` list. Its V2 handler then emits a readable
`agent_message`. An omitted list is not equivalent to an empty list.

When an API-key provider is configured, EMP presents collaboration tools to the
native upstream under an ordinary transport namespace, with plaintext message
parameters. Returned calls are restored to Codex's collaboration namespace and
marked as plaintext. This retains Codex's own dispatch, permissions and agent
lifecycle. It does not decrypt existing tasks or modify reasoning ciphertext.

Both standard top-level `tools` and Responses Lite `input[].additional_tools`
are adapted. Codex builds the latter in `core/src/client.rs`; adapting only the
top-level field leaves Lite delegation encrypted. Altered additional-tools items
receive a deterministic new ID because Codex derives their identity from their
schema. WebSocket output restoration also applies to incremental requests that
omit inherited tool definitions.

The external Responses projection maps readable agent messages to user messages.
Encrypted agent messages are rejected rather than silently sending an incomplete
task. Previously failed encrypted tasks require a new delegation after updating
EMP. Mixed-provider inherited histories may still encounter separate history
projection limitations.

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
