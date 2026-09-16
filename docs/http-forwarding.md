# HTTP forwarding

External HTTP requests use urllib3 connection pools through `http_pool.py`.
Managers are separated by the current proxy URL; loopback providers bypass proxies.
Origin credentials remain request headers and are never stored as manager defaults.
HTTP proxy userinfo is URL-decoded into proxy-only authentication headers, including
CONNECT requests. Different proxy credentials select different managers.
The manager cache keeps four proxy configurations, each with at most sixteen origin
pools. A pool retains up to 32 idle connections; it does not impose a new generation
concurrency limit or wait queue.

TLS uses the verified operating-system context selected at startup. Redirects and
automatic transport retries are disabled so POST bodies and authorization headers
cannot be silently replayed. Existing route-level retry rules remain unchanged.

SSE iteration uses `read1` and a line buffer, rather than waiting to fill a large
read buffer. Incomplete lines are limited to 1 MiB at this layer, including the
newline when present; scanning resumes at newly received bytes. An oversized line
closes the connection before the outer SSE parser can be bypassed by buffering.
Completed HTTP bodies return their connection to the pool. Cancelled
or unread responses close their connection. A validated Responses terminal permits
up to 100 ms / 64 KiB of trailing HTTP cleanup after terminal delivery, allowing the
final chunk to be consumed. If cleanup cannot finish, the connection is discarded.
This cleanup budget does not extend generation or first-output timeouts.

`tests/test_http_pool.py` checks reuse using actual TCP client ports, early SSE
delivery before the upstream finishes, cancellation, bounded cleanup, credential
separation, proxy changes, and the absence of automatic replay/redirects.

Native compressed WebSocket plans carry the selected proxy from preparation into
the handshake. The connection key uses that same snapshot; the next plan reads
system settings again. Successful responses and pongs remain fresh for 20 seconds,
after which reuse requires a new probe. Send attempts invalidate health until a
successful terminal arrives. No send failure is automatically replayed.
Native pre-output events share the external stream's 256-event / 1 MiB limits.
The downstream WebSocket uses each event's existing UTF-8 JSON encoding for both
the byte count and transmission.

A same-path network A/B check on 2026-09-14 measured warm-request medians of 819 ms
with a new connection and 193 ms with pooling (five samples each, excluding the
first connection). This isolates connection overhead; it is not a model TTFT or
TPS benchmark. Large history uploads and upstream generation latency remain
separate costs.
