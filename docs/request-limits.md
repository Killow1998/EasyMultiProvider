# Request size limits

EMP starts each incoming Codex request with a **64 MiB** allowance and grows it
automatically as needed: 128, 256, 512 MiB, up to a **1 GiB safety ceiling**.
Growth applies to the current request only, without replaying the upstream
request, changing saved configuration, or truncating history/attachments.

- `POST /v1/responses` and `/v1/responses/compact`: both the encoded HTTP body
  and the decoded JSON body are checked. Each compression layer is bounded;
  decoding can grow the same request's allowance while reading.
- The local `/v1/responses` WebSocket: the complete incoming message, including
  all continuation frames, uses the same policy. Ping/pong frames do not count
  as history. A data message has a 30-second socket read timeout so a stalled
  upload releases its reservation after a read timeout; idle between-turn
  connections remain unaffected.

Previously, WebSocket input was limited to 16 MiB and HTTP decoded input to
32 MiB. A long history could fail over from WebSocket only to be rejected
again after HTTP decompression. The two incoming transports now share a baseline
and automatic growth policy.

## Memory and notifications

Before growth, EMP reserves eight times the proposed allowance as an estimate
for parsing, strings, compression, and routing copies. Expanded requests share
one synchronized reservation ledger. Their combined reservations cannot exceed
half the currently available system memory at admission. This is conservative
accounting, not preallocation or a guarantee against all out-of-memory cases;
other applications can change memory usage. If memory information is unavailable
or insufficient, growth fails closed. Requests within 64 MiB keep existing behavior.

Reservations are released after the HTTP response/stream or WebSocket turn ends,
including errors and disconnects. A subsequent request starts at 64 MiB again.

The console prints each expansion or refusal. The authenticated EMP page polls
`/api/request-limits` every five seconds and displays the most recent notice in
a persistent banner. Dismissal hides that notice; a newer one appears again.
The process retains at most 20 notices containing only times, transport, sizes,
and fixed reasons. Notices are not inserted into model output or the Codex chat,
and do not claim that inference has completed. Restarting EMP clears notices.

The existing 4 MiB upstream native WebSocket threshold is unchanged. Larger
native requests continue through EMP's existing compressed HTTP forwarding
path. Upstream response/event limits are unchanged. Management requests retain
their 5 MiB limit, or 32 MiB for migration imports.

EMP starts with capacity for 48 long-lived local Responses WebSockets within a
64-request listener budget. When demand reaches either boundary, the listener
grows in 32-slot steps, up to 224 WebSockets and 256 total requests. The separate
ceilings leave 32 slots for new proxy turns, health checks, and management
requests. Only a connection beyond the expanded ceiling is closed, so ordinary
bursts do not turn idle Codex task connections into resets.

## Subscription generation concurrency

An idle local Codex WebSocket does not consume an upstream generation slot.
When a turn starts, EMP begins with four active generations for the same
content-free Subscription identity. While requests are waiting, a complete
successful window raises that identity's limit one slot at a time, up to 16.
An upstream 429 immediately halves the current limit. Additional turns wait in
FIFO order, with a bounded queue of 128, for up to 45 seconds. If no slot becomes
available, EMP returns status 503 with
`upstream_capacity` before sending the request upstream, so a client retry
cannot duplicate model or tool side effects. Different Subscription identities
have independent limits.

These limits belong to EMP, not Python and not the OpenAI service. They are
adaptive safety boundaries: local listener capacity grows from 64 to 256 under
demand, while a busy Subscription identity grows from 4 to 16 active
generations after successful windows and contracts after a 429. EMP cannot
raise a provider's own quota or concurrency allowance.

## Failure status classification

EMP preserves an actual upstream HTTP status. Local connection failures use a
separate boundary so Codex and the health view do not report every outage as
502: an unavailable configured proxy, DNS failure, or other unavailable
network path returns 503; a timeout returns 504; TLS and malformed gateway
responses remain 502. Proxy-generated HTTP errors are identified only from
explicit proxy evidence such as CONNECT/proxy response markers. Ambiguous
origin responses are kept as upstream 5xx rather than guessed.

Native WebSocket requests wait at most 120 seconds for the first substantive
model or tool event. After output starts, the existing 300-second inactivity
allowance applies. A request that may already have reached the upstream is
failed closed if the stream times out, closes, or omits its terminal event;
EMP does not replay that request over HTTP. Native WebSocket-to-HTTP fallback
is reserved for handshake failures that occur before the request is sent.
Queue wait time is included in content-free EMP preparation diagnostics.

An HTTP size rejection returns **413**, `error.code: request_too_large`, and
`error.limit_bytes`, then closes the connection. An oversized incoming WebSocket
message receives an error event with status 413 when the connection is writable,
followed by close code 1009. Clients may still choose to retry or fall back to
HTTP. The diagnostic journal records the rejection stage and limit, not content.

These are transport byte limits, not model token limits. The model's context
window and upstream limits still apply. If a request reaches the memory or 1 GiB
ceiling, wait for other requests to finish, compact history, or reduce attachments.
Increasing the transport limit does not give a model a larger context window.
