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

This is a native encrypted `agent_message`, not an EMP-owned `emp1:` compaction
checkpoint. The existing local `EMP migrate-history --thread-id ID` command only
converts the latter for direct native resume, with explicit user execution and
transactional backups. It cannot decrypt these native messages. This report does
not establish a defect in that checkpoint migration tool.

Parent history uses the existing Cross-service subagents > On-demand recovery
mode. This transcribes encrypted agent messages for the current forwarded request
using the observed original GPT subscription, with accounted extra latency/usage.
The proposed separate parent-history recovery setting is removed; a legacy flag
cannot independently enable recovery. Off/On modes explain the existing mode and
manual alternatives. Session files are never rewritten by this request handling.

Only current child metadata enables the existing cross-service child policy;
historical delegation messages cannot make a parent turn count as a child.
Origin fingerprints remain bounded to 4096 entries for 24 hours, separately from
the one-hour plaintext cache. Recovery preflights all envelopes/accounts before
any paid inference and accepts at most eight encrypted envelopes per request.
Missing or changed accounts fail closed. Review/compaction worker exclusions and
the 45-second inference limit remain in place. Client reconnect behavior is not
changed or claimed fixed by this patch.

### Isolated source verification

On Linux, a controlled substitution of the pre-fix accept loop failed the real
descriptor-exhaustion fixture with connection refusal; the PR accept loop passed.
The old same-MIME image matcher rejected the observed PNG-to-JPEG downscale; the
PR matcher passed all eight geometry cases. The installed-CLI three-screenshot
fixture passed for both Opus and Sonnet against a loopback fake service using
synthetic credentials and images. These checks do not establish macOS desktop
acceptance or reproduce every separate failure from the author's full local suite.

The parent-history contract checks Off/On rejection with zero recovery/destination
calls, then successful HTTP/WS On-demand recovery,
including inherited outbound tasks. Four forwarded turns share one accounted
helper inference; input/tool pairs and a synthetic saved session remain intact.
The Native GPT child reply passes through the real response observation path to
establish account provenance. A readable completion notification requires no
recovery call, even when external children are Off. Existing child contracts
retain ownership checks, accounting and preflight rejection.

The reverse external-parent route also exposed a missing plaintext marker:
external collaboration calls lacked `encrypted_function_args: []`, which Codex
requires to recognize readable messages. The protocol regression failed before
the marker fix and passed afterward for spawn, message and follow-up tools,
including complete and streamed items. Unrelated tools retain their metadata;
claimed native encryption from an external provider is rejected. These isolated
checks are complemented by the real-model app-server acceptance below.

The bilingual settings contract rejects any extra history-recovery control.
These checks ran in an isolated PR worktree without changing the live service,
native credentials or conversations. Integration and publication follow the
normal release gates.

### Real-model switch acceptance and modularization (2026-10-19 UTC)

Codex CLI 0.163.0 and Claude Code 2.1.300 completed the following against an
isolated Linux candidate, using actual provider/subscription endpoints:

| Same-thread pair | Parent turns | Completed children | Result |
| --- | --- | --- | --- |
| Opus 5.5 ↔ Native GPT-6.1 Sol | 8 | Native child from Opus; Opus child from Native | Pass |
| Chuang Gemma 3.1 ↔ Native GPT-6.1 Sol | 8 | Native child from Gemma; Gemma child from Native | Pass |

Each pair preserves a synthetic remembered code across switching, checks the
actual spawned child's model, completion and output, then switches again and
checks that the parent can recall the result. All 16 parent turns completed
with zero error events. All four actual children completed. Two encrypted Native
assignments to external children each triggered one successful, accounted GPT-6
Luna recovery. Normal child completion did not require a helper. Two zero-attempt
history replay responses were followed by successful full-history continuation;
there were no failed inference calls. The test does not claim a new macOS/Windows
desktop run or recovery of old ciphertext with lost account provenance.

The candidate EMP SHA-256 was
`2780688bba00472f6a8f8308c5a9bcb66180cb697f24d3d34b7c88165cd5a809`.
The repeatable opt-in [acceptance harness](../scripts/switch_acceptance/README.md)
uses fresh private state, reads production credentials without changing them,
and never supplies a refresh token to its disposable Codex. Exact source auth,
master/provider-key bytes and selected provider/model records remained unchanged.
The source config's whole-file hash changed during live account/quota updates;
it is not reported as byte-identical. The installed EMP was not upgraded.

The optimization keeps request policy/preflight in `collaboration.rs` (243 → 136
lines) and moves bounded provenance/cache ownership into `collaboration/state.rs`.
Tool projection stays in `tool_bridge.rs` (590 → 237 lines), with definition
adaptation, response restoration and tests in focused sibling files. No public
API, new setting, recovery button or session-management layer is added.
The obsolete separate-setting test matrix is removed; bilingual settings checks
still require reuse of the existing On-demand mode.

Controlled ablations removed the plaintext marker and bypassed the recovery cache
independently. The former failed the collaboration protocol assertion; the latter
failed repeated HTTP/WS parent-history recovery by requesting an extra helper.
Both were restored before acceptance. Seven collaboration tests, six tool-bridge
tests, two conversation-switch contracts, 22 frontend feature contracts and two
copy contracts passed, as did affected-library Clippy with warnings denied.
These checks establish useful cross-module behavior; they do not remove the need
to run affected integration paths after future interface changes.

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

In 0.13.9 the matcher permits PNG-to-JPEG only when the existing strict proportional
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

### Follow-up: Codex preprocessing plus a second CLI conversion

The 0.13.9 raw-screenshot test did not reproduce the actual failing inputs.
An explicitly authorized, OS-egress-isolated local replay of both complete
reported histories with Claude Code 2.1.289 found source PNGs at 1326×336,
2048×409 and 1995×1248. The CLI changed the second to a 1998×399 PNG while
claiming 2000×399 in its note (a two-pixel discrepancy), and re-encoded the
third as JPEG **without changing dimensions or emitting a resize note**.
This is a distinct second image-processing pass after Codex preprocessing.
Neither upstream health nor losing the original chat was responsible.

The follow-up does not widen geometry tolerances or accept arbitrary image
replacement. In CPA relay mode, EMP retains authoritative original image blocks
and passes fixed valid 1×1 transparent PNG slots through the intermediary CLI.
It validates the complete ordered text/slot transcript, association markers,
document blocks, known cache-breakpoint metadata and sole `StructuredOutput`
tool carrier before forwarding. It restores the original image source at each
validated position; source MIME, bytes and URL sources remain exact. Invalid
slots, changed tool text, dropped/reordered blocks and extra tool authority
still fail closed. The upstream model never receives placeholder images.

The obsolete 599-line image-geometry module and resize-note fallback are removed.
Slot comparison now checks equal block counts and exact ordered contents, with
only the existing cache metadata and semantic JSON-marker normalization. There
is no second image-identity rule to maintain alongside source restoration.

This policy applies only to the local CPA relay. Native local-login execution
has no interception boundary and must keep real images in CLI stdin. Context
assessment and the combined system/source-byte budget use original media in
both modes. This avoids introducing a lossy local codec or new dependency, but
also removes implicit CLI image compression from CPA requests. Providers may
still reject source media above their own limits; this patch does not claim to
override those limits. See [Claude vision documentation](https://platform.claude.com/docs/en/build-with-claude/vision).

Private replay inputs are stdin-only, opt-in, never CI fixtures, never logged
or committed. The guarded subprocess uses an isolated temporary home and dummy
authentication, with OS network policy allowing only one fake upstream port.
An independent negative check first denies a second loopback port and an
external address. Only image counts and pass/fail metadata leave the test;
temporary runtime files are removed. Public regressions use generated pixels
and synthetic tool histories, never private conversations or screenshots.

#### Follow-up validation record (2026-10-19, Asia/Shanghai)

- Rust 1.93.1, macOS, installed Claude Code 2.1.289: 65 Claude adapter tests
  passed; the private stdin acceptance remains ignored by default.
- Both explicitly authorized complete failed histories passed the opt-in
  guarded replay with Opus and Sonnet: four cases, three exact source images
  each, one successful CLI terminal and no error event. A nonblocking fake
  socket bug caused intermittent test timeouts before its bounded reader was
  normalized; no image assertion or CLI timeout was widened.
- All 12 public installed-CLI checks passed in the final serial run, covering
  original and Codex-preprocessed PNGs,
  HTTP/WebSocket appended tool-result turns, full-history continuation after
  rejecting a non-native prior ID, long exact tool text, PDF/URL association,
  effort/catalog roundtrips, bounded compaction and 429 classification.
- All 36 frontend contracts, formatting and workspace/all-target Clippy with
  warnings denied passed.
- **The full local workspace is not green.** The all-target/no-fail-fast run
  failed seven targets: `emp-app` lib (including a native image fixture cleanup
  abort), `emp-app` exit/startup/user-journey integration targets, `emp-codex`
  lib, router transport failures and transport HTTP-client tests. Directory
  refresh also timed out when run alone. These failures have not been proven
  to be introduced by this patch or all established as pre-existing. They are
  not waived, and this is not full desktop/release acceptance.

#### Integration verification (2026-10-19 UTC)

- After removing the unused geometry module and resize fallback, all 57
  affected Claude adapter tests pass on Linux. These include source-byte
  budgets, local-login separation, exact slots and tool-authority rejection.
- All 13 installed-Claude 2.1.300 contracts pass against synthetic loopback
  upstreams. The added boundary case forwards an original 8001-by-1 PNG and
  preserves provider 400/413 errors instead of reporting a local mismatch.
  This tests error propagation; it does not make a real-provider capacity claim.
- The Mac's running EMP PATH selects a trusted native Claude Code 2.1.303.
  Using that same CLI selection, the PR's two synthetic image-history checks
  pass: preprocessed screenshots and multi-turn HTTP/WebSocket tool-result
  continuation, each covering Opus and Sonnet. macOS denies external outbound
  connections during these checks, with a negative connection probe first;
  only loopback fixtures are allowed. No real-model calls or runtime updates
  are involved. This is additional evidence to the author's 2.1.289 record,
  not a rerun of the full Mac workspace suite listed above.
