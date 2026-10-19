# Claude switch failure boundaries

Three observed failure classes on EMP 0.13.8 have different boundaries. Repairing
one does not imply the others are fixed.

## Fresh chats: the process remains alive without its TCP listener

Fresh Claude and GPT conversations both failed WebSocket handshakes, then HTTP
fallback. The EMP process was still alive with established old connections, but
had no listening socket. Its descriptor count was near the macOS process soft
limit of 256. This is consistent with descriptor exhaustion; the exact original
accept error cannot be established because the old accept loop did not log it.
It is not evidence that Magpie ran a tool, changed EMP settings, or caused a leak.

The accept loop previously broke on any error, silently dropping the listener
while already-admitted workers remained alive. It now retries with a 100 ms
backoff, logs errors at most every five seconds, and records recovery. Interrupted
accepts are retried immediately. Shutdown still stops the loop.

Startup attempts to raise only its own soft `RLIMIT_NOFILE` to 4096, bounded by
the inherited hard limit, never lowering an existing higher or unlimited limit.
No system setting is changed. If the limit remains small, both connection pools
are capped using a reserve of 64 descriptors and four descriptors per slot per
pool. This is a conservative admission heuristic, not a guarantee about every
future transport's descriptor usage or a proof that all descriptors are released.
Retry remains necessary for pressure caused by work outside those pools.

The Unix regression fixture exhausts descriptors in a disposable subprocess with
a hard limit of 128, queues a new connection, releases pressure, and requires
HTTP 200, an authenticated WebSocket 101, an EMFILE receipt and a recovery receipt.
It uses a random port and isolated test configuration, not the installed service.
Source changes do not repair a listener already dropped by the running old build;
that process requires a normal restart. Restoring acceptance alone is not a
real-model or old-history compatibility acceptance test.

After a manual restart with a larger process-only file limit, the installed build
returned HTTP 200 from its health endpoint. One ephemeral Codex CLI turn through
that running service to Claude Opus 5.5 completed exactly once with a fixed short
answer, no failure/error events and no tool execution. This establishes fresh
turn recovery, not validation of the uninstalled source patch or every old chat.

## Encrypted parent history: 409 before destination execution

A top-level Native conversation contained four historical encrypted child replies
and was switched to Claude. With external collaboration off, every attempt failed
at `prepare_destination` with `encrypted_agent_task_requires_recovery`: six
WebSocket attempts followed by HTTP 409. No Claude request was dispatched.

The history-recovery opt-in is independent of external-child policy. It uses the
existing GPT-6 Luna low-effort capture on the original observed subscription,
records helper usage separately, and preserves the other input items and tool
pairings. No local session files are rewritten. Native requests are unchanged.
All envelopes/accounts are preflighted before any paid recovery; at most eight
encrypted envelopes are accepted per request. Each uncached inference has the
existing 45-second limit and downstream cancellation handling.

Origin hashes plus route ownership remain memory-only, bounded to 4096 entries
for 24 hours. Plaintext remains limited to 64 entries, 256 KiB each, for one hour.
The longer provenance window covers multi-hour chats without retaining plaintext
for longer. A restart, eviction, missing observation or account change can still
make a particular old chat unrecoverable. This patch cannot reconstruct missing
original-account provenance and does not infer it from the currently selected
account or merely successful replay of encrypted input.

### Alternatives and consent

- Automatic GPT recovery: opt in explicitly, accepting extra subscription usage,
  latency and possible transcription differences. The default remains off;
  permission from one reporter is not consent on behalf of every EMP user.
- No automatic GPT recovery: keep the setting off and start a new external chat
  with a readable, reviewed handoff. EMP explains the failure rather than dropping
  encrypted items or pretending that their unencrypted headers are the payload.
  A user may prepare the handoff manually or request it from the original Native
  model; that explicit Native turn is a separate quota-consuming operation.

These alternatives are also relevant when provenance has already expired. Merely
enabling recovery cannot repair that situation. Client reconnect behavior is not
changed or claimed fixed by this patch.

## Multi-image relay: 502 after starting Claude Code

Two other conversations failed with `claude_cli_content_mismatch` in
`relay_validation`, exposed as `claude_cli_transcript_mismatch`. Both Opus 5.5 and
Sonnet 5.5 were affected. The relay rejected the request before forwarding to the
configured Anthropic service. Thus this failure does not establish an upstream
credential, quota or provider-health problem.

A loopback-only probe with Claude Code 2.1.289, an isolated home, dummy token and
generated RGBA screenshots reproduced PNG-to-JPEG recompression during resize.
The three input dimensions were 1326×336, 2866×572 and 2878×1800. The first image
remained exact; the second became a 1999×399 PNG with a nominal 2000×399 note;
the third became a 2000×1251 JPEG. Resize notes were appended after the transcript.
The existing matcher already handles deferred notes and nominal one-pixel
rounding, but rejected every MIME change, including that PNG-to-JPEG conversion.

The matcher now permits PNG-to-JPEG only when the existing strict proportional
downscale, original dimensions, resize-note geometry and rounded scale checks
all succeed. No general MIME-change or unscaled-recompression bypass is added.
Image count, ordering, text/history contents and sole `StructuredOutput` tool
authority remain checked. Existing geometry validation does not establish pixel
provenance; this compatibility path also does not claim original image-byte or
lossless pixel preservation. JPEG conversion can lose image detail.

Synthetic unit/transport and installed-CLI tests are separate evidence from a
real-account desktop acceptance run. No private conversations, screenshots,
credentials, account aliases or session JSONL belong in the public PR. The
running application is not upgraded or restarted by the source patch.
