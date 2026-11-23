# HTTP forwarding

EMP uses the Rust transport in `crates/emp-transport/src/http_client.rs`.
Reqwest clients are reused by proxy and redirect policy. The cache retains at
most four client configurations; each destination keeps up to 32 idle
connections for 60 seconds. These are idle-connection limits, not generation
concurrency limits. Authorization remains a per-request header.

Proxy selection is performed by `http_policy` and `system_proxy`. Loopback
requests bypass proxies. Native WebSocket preparation and its handshake use the
same selected proxy snapshot. Later requests can observe refreshed system
settings without changing an in-flight request.

TLS verification stays enabled. Ordinary requests reject redirects; callers
that explicitly need redirects use the bounded same-origin path. Transport-level
retries are disabled. Route-level recovery decides whether a request can be
sent again, so a failed POST is not silently replayed by the HTTP client.

Streaming responses are read incrementally. Response reads enforce decoded-body
limits and first-event/idle deadlines. After a validated terminal event, cleanup
may drain at most 64 KiB within 100 ms. Unread, cancelled or unfinished responses
lose their connection; cleanup does not extend generation timeouts.

Native WebSocket failures can temporarily use HTTP/SSE. The affected route waits
30 seconds before a later full-history request may try WebSocket again. An
incremental request that cannot reuse its continuation asks Codex to resend full
history; EMP does not forward its delta as a complete conversation. A failed
probe does not automatically repeat inference. The implementation is in
`crates/emp-app/src/services/native_connections.rs` and the native continuation
path.

## Protocol negotiation and terminal results

External protocol fallback requires `auto` selection, an available alternate
adapter and a pre-output protocol rejection (404/405/415/501). An explicit
`model_not_found` never triggers it. Authentication, permission, WAF, rate-limit,
context, network, timeout and generic 5xx failures do not establish a different
protocol. Same-protocol retry decisions remain separate from negotiation.
Successful protocol observations are scoped to endpoint, deployment and model.

Generation is never replayed after visible output or tool activity. Recovery of
an existing task and requesting full input from Codex are distinct from replay.
An upstream incomplete response or missing terminal event must not become
`response.completed`. Output limits, content filtering and `stream_incomplete`
retain their bounded reasons; a normal tool-call boundary is not truncation.
Diagnostics retain lifecycle facts and error classes, without response content.

## Verification

- `crates/emp-transport/tests/http_client.rs`: real loopback TCP/TLS tests for
  pooling, credential separation, redirects, absence of automatic POST replay,
  early streaming, cancellation, deadlines and response limits.
- `crates/emp-transport/src/http_client/tests.rs`: proxy changes and the bounded
  client cache.
- `crates/emp-app/src/tests/native_api_contract/continuation/transport.rs`:
  HTTP fallback, WebSocket recovery after cooldown and continuation isolation.

The 2026-09-14 Python implementation measured warm connection medians of 819 ms
without pooling and 193 ms with pooling (five samples each). Those historical
measurements describe connection overhead, not current Rust performance or model
TTFT. Current transport acceptance uses the Rust contracts listed above.
The 2026-09-18 Python diagnosis reproduced a 1009 close with roughly 48 MB of
history and passed 103 related tests; that result belongs to the old transport.
