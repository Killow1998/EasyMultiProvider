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

Packaged builds forward accepted native requests over a permessage-deflate
WebSocket without adding a second outbound request-size cutoff. The Python 3.8
compatibility fallback still uses compressed HTTP for native requests above
4 MiB because its legacy WebSocket client cannot negotiate compression.
Incoming upstream WebSocket messages use Codex's 64 MiB per-message boundary;
EMP does not add a cumulative response-size or event-count ceiling. Management
requests retain their 5 MiB limit, or 32 MiB for migration imports.

EMP starts with capacity for 48 long-lived local Responses WebSockets within a
64-request listener budget. When demand reaches either boundary, the listener
grows in 32-slot steps, up to 224 WebSockets and 256 total requests. The separate
ceilings leave 32 slots for new proxy turns, health checks, and management
requests. Only a connection beyond the expanded ceiling is closed, so ordinary
bursts do not turn idle Codex task connections into resets.

EMP doesn't impose a separate per-account generation queue. Accepted turns are
forwarded immediately, and the upstream remains responsible for its own account
concurrency and 429 policy. The process-wide listener boundaries above still
protect the local Python service from exhausting threads and memory.

## Failure status classification

EMP preserves an actual upstream HTTP status. Local connection failures use a
separate boundary so Codex and the health view do not report every outage as
502: an unavailable configured proxy, DNS failure, or other unavailable
network path returns 503; a timeout returns 504; TLS and malformed gateway
responses remain 502. Proxy-generated HTTP errors are identified only from
explicit proxy evidence such as CONNECT/proxy response markers. Ambiguous
origin responses are kept as upstream 5xx rather than guessed.

Native WebSocket requests apply the same 300-second per-message inactivity
allowance as Codex, including while waiting for the first response event. A
request that may already have reached the upstream is failed closed if the
stream times out, closes, or omits its terminal event; EMP does not replay that
request over HTTP. Native WebSocket-to-HTTP fallback is reserved for handshake
failures that occur before the request is sent. Wrapped WebSocket error events
preserve explicit upstream statuses such as 429 instead of being recorded as a
generic 502.

An HTTP size rejection returns **413**, `error.code: request_too_large`, and
`error.limit_bytes`, then closes the connection. An oversized incoming WebSocket
message receives an error event with status 413 when the connection is writable,
followed by close code 1009. Clients may still choose to retry or fall back to
HTTP. The diagnostic journal records the rejection stage and limit, not content.

These are transport byte limits, not model token limits. The model's context
window and upstream limits still apply. If a request reaches the memory or 1 GiB
ceiling, wait for other requests to finish, compact history, or reduce attachments.
Increasing the transport limit does not give a model a larger context window.
