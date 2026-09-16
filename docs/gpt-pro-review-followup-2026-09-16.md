# GPT Pro review follow-up — 2026-09-16

Review baseline: `8944264`. The supplied review describes focused module checks
and simulated latency; it does not establish whole-repository or production
compatibility. Its downloadable candidate patch bundle was not supplied here.
The changes below were implemented against the actual checkout independently.

## Implemented

- HTTP SSE iteration bounds incomplete lines at 1 MiB before returning to outer
  framing/total-size checks. Newline searches resume at newly received bytes;
  oversized lines close the response. Complete-line and strict UTF-8 semantics
  are preserved, including split characters and final lines without a newline.
- HTTP proxy URL userinfo is decoded into urllib3 proxy-only headers. CONNECT
  does not receive origin Bearer credentials. Managers remain keyed by the full
  selected proxy configuration, including its authentication identity; proxy
  userinfo is removed from the manager's transport URL. Redirect/replay rules
  are unchanged. See the [urllib3 proxy-header interface](https://urllib3.readthedocs.io/en/stable/reference/urllib3.poolmanager.html#urllib3.ProxyManager).
- Native WebSocket pre-output events use the existing 256-event / 1 MiB stream
  limits. A limit violation closes the upstream and returns a failed response
  without replay. Already encoded JSON bytes are also used for transmission.
- Without a provider replay scope, signature observation yields unchanged bytes
  without parsing. Cancellation still closes the underlying generator.
- Read-only namespace containers are traversed without an extra deepcopy. The
  request and mutable leaf definitions retain independent copies, including
  the counterexample where multiple namespaces share one original tool object.
- A single timestamp represents successful native completion/pong freshness.
  Reuse within 20 seconds avoids an extra synchronous probe; idle reuse still
  probes. The timestamp expires even when no subsequent request was sent, and
  sending invalidates it until successful completion. Send failures are never
  automatically replayed.
- Native compressed WebSocket plans carry the resolved proxy into the handshake.
  Their connection key hashes the same selection. The next plan reads current
  settings again; no persistent system-proxy snapshot cache was introduced.
- The compatibility workflow's explicit test list now includes HTTP pooling,
  pooled TLS and provider replay. Workflow execution is still deferred.
- Two previously confirmed quota omissions are fixed: non-object JSON-RPC fails
  with `quota_output_protocol_error`, and management diagnostics retain specific
  quota failure codes rather than collapsing them to `quota_error`.

## Unresolved installation boundary

Linux in-app system updates still have a same-user staging-file substitution
window between ordinary-user verification and the privileged installer's read.
This assumes local same-user write access and approved authorization, not an
anonymous remote attack. Neither another pre-install hash nor the existing
post-install executable/version check establishes package-wide integrity across
that boundary. The implementation has not been silently disabled or presented
as fixed. The update documentation now explicitly records this limitation.

The smaller design is to delegate protected installations to the system package
manager. Keeping in-app elevation requires a separately trusted privileged entry
point that snapshots and verifies trusted release metadata and the complete
package within that boundary. This changes installation/distribution behavior
and remains an explicit architecture decision.

## Verification scope

Windows native PowerShell 7.6.5 / Python 3.11: 104 targeted tests completed
successfully (103 passed, one Unix-only permission check skipped). Tests requiring
temporary file writes were run with ordinary host permissions after diagnosing
a sandbox temporary-directory denial. `git diff --check` also passed.

Targeted local checks cover line limits, UTF-8 framing, HTTP/CONNECT credential
separation, reuse/cancellation, route proxy changes, pre-output queue limits,
shared tool objects, signature isolation, native continuity and quota protocol
errors. The HTTP proxy and downstream WebSocket checks use actual local sockets;
upstream native/provider behavior is supplied by controlled fixtures.

The current checkout has not been tested against real model accounts, on macOS
or Linux in this follow-up, or with a real privileged package substitution.
No production TTFT/TPS or 502-rate improvement is claimed. Linux `gsettings`
process consolidation and broader event-observer consolidation remain future
measurement work, not delivered behavior. No CI, release, installation replacement
or version bump was performed for this follow-up.
