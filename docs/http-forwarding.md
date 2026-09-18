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
successful terminal arrives. Ordinary send failures are not automatically replayed.
The sole after-send exception is a peer-initiated 1009 (message too big) close
before any response event: the full request falls back to the existing HTTP/zstd
path inside EMP, without first sending `response.failed` to Codex. That route
stays on HTTP for the remainder of this downstream connection; other routes and
connections retain native WebSocket support. An incremental request still asks
Codex for a full retry with `previous_response_not_found`, rather than dropping
its transport state and forwarding only the delta. Local 1009 closes, accepted
requests, output/tool activity and ambiguous disconnects do not permit replay.
Native pre-output events share the external stream's 256-event / 1 MiB limits.
The downstream WebSocket uses each event's existing UTF-8 JSON encoding for both
the byte count and transmission.

A same-path network A/B check on 2026-09-14 measured warm-request medians of 819 ms
with a new connection and 193 ms with pooling (five samples each, excluding the
first connection). This isolates connection overhead; it is not a model TTFT or
TPS benchmark. Large history uploads and upstream generation latency remain
separate costs.

A 2026-09-18 investigation found repeated 1009 closes for an approximately
48 MB native WebSocket request, followed by successful HTTP/zstd forwarding of
the same logical history. Codex's client source keeps a failed WebSocket fallback
at session scope; EMP must not multiply that transition into repeated failed
model responses. The older journal lacks close-frame ordering, so it cannot
establish which peer initiated those historical 1009 closes. New exception
diagnostics retain the close ordering without its reason or frame contents.
The local patch ran 103 relevant tests successfully (one platform-specific skip),
including a real compressed loopback socket's close ordering, one internal HTTP
fallback, subsequent full-request recovery, non-replay guards and preserved
prompt-cache keys. Compressed connections now explicitly own the library's
context-manager lifetime across supported versions. This is controlled
regression evidence, not a measured improvement in real model TTFT or an
installed-app test.
