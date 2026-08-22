# External Models as Codex Agent Workers

Status: implemented for v0.6; GLM and ox-alpha acceptance completed.

Gemini is temporarily excluded from live acceptance because its configured
quota is exhausted. Offline compatibility coverage remains enabled and does
not issue Provider requests.

## 1. Goal

Allow a model already routed and advertised by EMP to be selected explicitly as
a Codex subagent model. Codex remains the owner of delegation, threads,
permissions, tools, and results. EMP remains the model catalog, capability, and
routing control plane.

This is not an EMP-owned agent framework and does not create a second task or
history database.

## 2. Product boundary

EMP owns:

- projecting eligible external and imported-subscription models into the same
  catalog used by the parent Codex runtime;
- recording honest model capabilities used by the native Codex runtime;
- routing the child request through the existing Provider or subscription
  account path without exposing credentials;
- explaining why a model is unsuitable for agent work.

Codex owns:

- spawning, waiting for, steering, and closing subagents;
- workspace, sandbox, approval, tool, and concurrency policy;
- thread/history persistence and compaction;
- the parent-to-child prompt and the child result.

EMP must not emulate these Codex responsibilities with `codex exec`, a private
session store, or a hidden background supervisor when the native Codex
subagent interface is available.

## 3. Capability boundary

A routed model remains usable only for capabilities that its Provider and EMP
route can preserve. Useful coding-worker capabilities include:

- text input and text output;
- a route that EMP can translate to a Codex-compatible Responses exchange;
- streaming or a correct non-streaming-to-Responses adaptation;
- structured tool-call round trips when the delegated task exposes tools;
- a usable context window and output budget.

Reasoning-effort controls are optional. A model that has no reasoning-effort
setting remains eligible; Codex must omit the setting instead of inventing a
value.

Vision is independent of agent eligibility. It is required only when the
delegated input contains images.

## 4. Catalog and runtime contract

There is one model slug and one catalog entry for both interactive and delegated
use. EMP must not create a second subagent-only alias.

An explicit native Codex subagent request uses that slug as its model override.
The current catalog may be newer than a model-candidate list cached in an
already-running parent task; an explicit slug is authoritative if the Codex
runtime accepts it. A fresh parent runtime must receive the complete current
catalog.

EMP exposes diagnostics but does not claim that every model visible in
`/model` is a reliable coding agent. Conversational-only, malformed-stream, or
tool-incompatible routes remain usable only for the capabilities they actually
support.

`multi_agent_version` is not an eligibility flag for being a child model. It
describes the collaboration backend a model can orchestrate itself. EMP clears
that field on external catalog entries instead of copying it from a native
template. Codex `0.149.0` still accepts the exact external slug as a
`spawn_agent` model override.

EMP does not automatically probe discovered models, spend Provider quota, or
claim that catalog visibility proves coding-agent quality. Live acceptance is
explicit and model-specific.

## 5. Bootstrap safety

EMP may itself be the route required by the selected worker model. Therefore:

- worker probes and delegated tasks must never stop, restore, restart, or
  reconfigure the EMP instance carrying them;
- lifecycle/integration mutation endpoints are unavailable to the probe;
- EMP health is checked before launch and loss of the route produces a bounded
  failure rather than recursive recovery;
- no worker is launched as a replacement EMP supervisor;
- EMP does not change Codex WebSocket, compaction, authentication, or session
  behavior to make a worker pass.

## 6. Security and isolation

- The child inherits Codex's normal workspace, sandbox, approval, and policy
  boundaries; EMP cannot widen them.
- Provider keys and imported subscription credentials remain encrypted and are
  never placed in prompts, environment dumps, diagnostics, or catalogs.
- Incoming current-login authorization continues to use the validated header
  allowlist.
- Probes are single-model and bounded by configurable timeout and concurrency
  limits.

## 7. WebSocket completion reliability

External workers depend on long-running streams, so v0.6 must diagnose and
reduce intermittent failures reported by Codex as:

```text
stream disconnected before completion: websocket closed by server before response.completed
```

EMP must distinguish at least these boundaries:

- the upstream closed before sending a terminal event;
- EMP received a valid terminal event but did not forward it;
- WebSocket framing, compression, or translation rejected a final event;
- the Codex client disconnected or cancelled the request;
- a network or proxy reset interrupted the connection.

Diagnostics record only transport state, close code, normalized failure class,
whether any output or tool call was already emitted, and whether a terminal
event was observed. They must not retain message content, tool arguments,
credentials, raw URLs, or upstream response bodies.

Recovery, replay, and fallback are separate operations:

- a transport reconnect may happen before or after output when it reattaches to
  the same running task or resumes an existing stream without rerunning model
  generation;
- transport recovery must preserve exactly-once visible output and tool-call
  delivery, and a successfully recovered reconnect is recorded as recovery,
  not as a task failure;
- never replay upstream model generation after output or a tool call has been
  emitted;
- never duplicate a tool call to conceal a broken stream;
- upstream generation may be replayed at most once only when failure occurred
  before the first output and the request is safe to replay;
- after partial output, prefer protocol-defined stream/session reattachment; if
  reattachment is unavailable, fail explicitly rather than regenerate the
  response from the beginning;
- preserve Codex's native WebSocket-to-HTTP fallback instead of disabling
  WebSockets globally;
- convert a proved upstream premature close into one terminal
  `response.failed` event when the client connection is still writable.

Required regressions cover a normal terminal response, close before first
output, one and multiple recoverable mid-task reconnects, close after partial
text, close after a tool call, malformed final frame, proxy reset, client
cancellation, and WebSocket-to-HTTP fallback. Recovered cases must reach one
terminal success; failed cases must reach one terminal failure. Every case must
finish once without duplicate output or tool execution.

## 8. Acceptance criteria

1. A routed external model can be passed explicitly to the native Codex
   subagent interface and completes a bounded read/edit/test task.
2. An imported-subscription-prefixed model can do the same without becoming the
   parent task's default model.
3. A model with no reasoning-effort options succeeds when effort is omitted.
4. A model that cannot preserve structured tool calls is reported as
   unsupported, not silently used as a text-only coding worker.
5. A stale candidate list does not reject an explicit catalog slug that the
   runtime accepts; a fresh runtime advertises the full current catalog.
6. Worker execution does not create EMP-owned history and does not change
   native Codex resume behavior.
7. Stopping EMP during an EMP-routed worker is neither attempted nor used by
   automated tests.
8. An intermittent WebSocket close is attributed to the correct boundary. A
   recoverable transport reconnect may still complete the task, while no path
   produces duplicate text, duplicate tool calls, or an unsafe generation
   replay.

## 9. Delivery result

1. Missing-`response.completed` boundaries and exactly-once transport behavior
   have regression coverage.
2. External entries retain their exact catalog slug and do not inherit false
   collaboration-backend metadata.
3. Codex `0.149.0` supplies the native model override; EMP adds no worker
   launcher, task store, or Web action that would duplicate it.
4. GLM and ox-alpha have completed native child-model acceptance, including
   models with different reasoning-effort surfaces. Future Provider models
   remain explicit, quota-bearing acceptance cases rather than catalog claims.
