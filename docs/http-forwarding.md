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

Concurrent system-proxy reads share one lookup. Cold-cache callers wait within
the system-proxy read timeout; warm-cache callers keep the last valid snapshot.
OpenRouter discovery checks `/key` before its unauthenticated model catalog.

TLS verification stays enabled. Ordinary requests reject redirects; callers
that explicitly need redirects use the bounded same-origin path. Transport-level
retries are disabled. Route-level recovery decides whether a request can be
sent again, so a failed POST is not silently replayed by the HTTP client.

Streaming responses are read incrementally. Response reads enforce decoded-body
limits and first-event/idle deadlines. After a validated terminal event, cleanup
may drain at most 64 KiB within 100 ms. Unread, cancelled or unfinished responses
lose their connection; cleanup does not extend generation timeouts.

Request JSON preallocation uses the unescaped size as a lower bound to avoid
repeated buffer growth for large histories.
Native SSE framing scans only new bytes and compacts once per chunk. Its ordinary
JSON fallback buffer excludes delivered SSE frames and comment heartbeats.
External streams stop parsing at a terminal event, including when a network
chunk contains a malformed tail. Context-error classification bounds rendered
evidence while still searching nested keys in oversized composites.

## WebSocket framing

Downstream socket handles share a lock across each complete frame, including
ping/close replies from a temporary reader. Nonblocking reads restore blocking
writes to preserve whole-frame writes on Windows. Frame extraction copies the
smaller of the payload and remaining buffer to avoid retaining large buffers.
RFC 7692 section 7.2.3.4 permits a final deflate block with only the synthetic
empty-block tail, optionally preceded by `0x00`; other trailing bytes are rejected
and the decoder resets before the next message.

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

Native WebSocket control forwards interrupt fields unchanged for the active
upstream to validate and acknowledge, and retains one next request for normal
route/scope checks after the current turn. Buffered upstream traffic yields
to downstream controls; a queued next request retains its disconnect sender,
and steering frames racing a terminal event remain control messages.

Native restoration drains idle sockets while active turns remain with their
executor; a restore timeout reopens admission without interrupting them.
Incremental turns inherit their socket's tool catalog, so a policy change
requires full-history recovery on a fresh connection; only explicit protocol
rejection suppresses reconnects for the downstream connection's lifetime.

Only successful generated turns calibrate native context. A 1009 size
rejection before any upstream event permits HTTP/zstd replay of full input;
incremental requests still require previous-response recovery.

Native prefill keeps whole input items and client tool call/output pairs
together, retains completed server-owned search/image items and injected app
page context without inventing client pairs, and declines unknown tool
protocols. A peer generating during prefill ends that attempt without replay,
repeated tool execution or success attribution; only the final generated turn
reaches the client and inference accounting.

Stateless adapters send completions without a resumable ID so Codex sends full
input, retaining the original ID for accounting and late interrupts. Claude
stream completion includes each public message once when the CLI repeats its
last message in the final proposal.

HTTP status takes precedence over a status field in a decision response body.
HTTP error responses carry `Retry-After` in the actual header and its legacy
body field; other upstream headers stay private, and streamed/WebSocket retry
advice retains its client-visible location.

Custom-tool projection changes required output fields, so complete Responses
replies and streamed terminal bodies are validated after projection. Native
Anthropic forwarding strips the CLI-only `[1m]` model suffix while retaining
configured gateway model mappings.

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
