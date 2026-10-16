# Codex maintenance window

EMP follows the latest stable Codex minor series and its previous nine series.
Patch releases count within their series; alpha and beta releases do not move
the window. The engine used by the CLI, App or IDE is the compatibility input,
independent of the host application's version. This policy does not introduce a
global version rejection: individual operations still check their interfaces.

## Current acceptance on 2026-10-16 UTC

The maintenance target is **0.154–0.163**, following the official stable
[0.163.0 release](https://github.com/openai/codex/releases/tag/rust-v0.163.0)
(2026-10-15 21:30:38 UTC). Series 0.153 remains in the historical record below.

The current EMP 0.13.4 source, including the update-page reuse fix, passed the
on-demand migration contract with these official Linux engines:

| Engines | Accepted paths |
| --- | --- |
| 0.154.0, 0.155.1, 0.156.1, 0.157.1, 0.158.0, 0.159.3, 0.160.1, 0.161.0, 0.162.1, 0.163.0 | Real MCP before/after external compaction; parent, early fork, later fork and grandchild; configuration-only shutdown/restore; explicit selected-history migration; all four native resumes; backup/content preservation; idempotent second restore |

Each engine passed all 51 behavior assertions in the created-history harness.
Official archive digests, executable hashes and before/after hash preservation
were checked. Every run used fresh homes, synthetic credentials, randomized
loopback listeners and a 1536 MiB address-space limit per child. Installed Codex,
the running EMP and user configuration were not changed.

Codex **0.163.0** additionally passed all 34 native API contracts, including the
five opt-in real-engine checks: online catalog/ETag refresh in one process,
localhost-only proxy bypass, external streaming steering, native WebSocket
interrupt/continuation and same-named Tool Search across four routing paths.
The transport checks include incomplete response ID/usage preservation and
deduplication, plus twenty incremental large-context turns for Native and
imported accounts with one warmup, no HTTP fallback and no full-history recovery.

### Exploratory async compaction

Async compaction is outside EMP's routine compatibility suite and release gate
while the upstream feature is under development. A stable Codex CLI version
alone does not make this feature stable. Retain the manual harness and existing
results for reproducibility; defer further async-specific adaptation and
steering/account/routing combinations until the feature itself becomes stable,
then reassess whether to include it in EMP's maintained test scope.

The exploratory real-engine async-compaction harness passed four cases with the
feature explicitly disabled or enabled: disabled control; adoption of a pending
result by explicit compaction with only one upstream compaction request; a new
turn preserving its suffix and discarding the older result; and normal process
shutdown while a background request is pending. All four Codex processes exited
with status zero. V2 compaction uses a Responses `compaction_trigger` here;
watching only `/responses/compact` misses that request. The
[feature remains under development and defaults off](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/features/src/lib.rs).

This establishes the tested Linux engine paths against synthetic upstreams.
Real-provider ciphertext and Windows/macOS/App/IDE journeys have separate
acceptance scope.
Release PR lists and commit ancestry alone must not substitute for checking the
tagged source and running the selected engine.

## Source acceptance on 2026-10-09

The maintenance target at this check was **0.153–0.162**, based on the official
[0.162.0 release](https://github.com/openai/codex/releases/tag/rust-v0.162.0).
The following checks used the local EMP v0.12.14 development source and official
Linux Codex binaries, with disposable homes and synthetic loopback providers.

| Engine | Accepted paths |
| --- | --- |
| 0.153.4, 0.154.0, 0.155.1, 0.156.1, 0.157.1, 0.158.0, 0.159.3, 0.160.1, 0.161.0, 0.162.0 | Real MCP tool invocation before/after compaction; external compaction; parent, early fork, later fork and grandchild resume; exact native configuration restoration; idempotent second restore |
| 0.162.0 | Additional: external streaming steering; native WebSocket interrupt and same-turn continuation; deferred Tool Search and same-named namespace tools through Chat, Anthropic, external Responses and native HTTP forwarding |

All ten series passed the common Linux fixture contract. Windows, macOS, App
and IDE bundled engines need their own acceptance evidence; Linux fixtures do
not establish real-provider acceptance.

## On-demand migration acceptance on 2026-10-12 UTC

Version 0.13.0 changes native restoration to
configuration-only. Historical ten-series results above describe the earlier
restore-time migration contract; they are not automatic acceptance of this new
contract across those engines.

Codex 0.162.1 passed the updated isolated Linux created-history fixture: real MCP
before/after external compaction, parent and three bounded descendants, explicit
single-thread migrations, native resumes and idempotent configuration restore.
The same engine passed online subscription catalog discovery and loading a new
model after a response ETag change within one process. All credentials and
upstreams were synthetic, with external network proxies blocked. Windows source
behavior still needs a candidate-binary acceptance; production is unchanged.

## 2026-10-14 development checks and 0.163 watchlist

At this check, the official stable release was [0.162.1](https://github.com/openai/codex/releases/tag/rust-v0.162.1)
(2026-10-09 19:44:51 UTC); [0.163.0-alpha.11](https://github.com/openai/codex/releases/tag/rust-v0.163.0-alpha.11)
was a prerelease (2026-10-14 03:27:29 UTC). The maintenance window was
**0.153–0.162**. GitHub commit comparisons confirm all three changes below are
ancestors of alpha.11; this is source provenance, not engine acceptance.
This dated watchlist is historical; async follow-up is now deferred under the
current policy above.

| Upstream change | EMP assessment and required evidence |
| --- | --- |
| [Incomplete response usage, #53670](https://github.com/openai/codex/pull/53670), `816fa70` | Preserve response ID, valid reported usage and terminal reason through SSE/WebSocket. Avoid duplicate usage accumulation; the upstream change is not evidence of duplicate provider billing. |
| [Background compaction lifecycle, #53800](https://github.com/openai/codex/pull/53800), `9e03960` | Enabled by `AsyncCompaction`; test pending-result adoption against steering, newer tool outputs, cancellation and account/routing changes with the actual prerelease engine. Existing foreground compaction tests do not cover that lifecycle. |
| [Daemon feature overrides, #53751](https://github.com/openai/codex/pull/53751), `e762c05` | Applies to explicit daemon restart and persisted feature toggles. Check the actual App/IDE backend and overrides; it is not a reason for EMP to restart a user's running client. |

The local development candidate now writes Native subscription refreshes to
EMP's own `easy-multi-provider/native-catalog.json`. Codex's model cache remains
read-only to EMP. The newer native source is selected by file modification
time; equal/unknown times favor the preserved snapshot. Caches marked with an
EMP merged ETag are excluded from native input. A missing Codex cache no longer
hides an available native snapshot. This changes source selection, not Codex's
cache schema or the user's existing files.

Focused Linux fixture evidence for this candidate:

- Catalog contracts: 29 passed; snapshot selection/ownership: 3 passed. The
  new byte-preservation assertion failed on the old native refresh writer and
  passes with the fix while newly fetched and retained models remain routable.
- Native transport contracts: 29 passed. Incomplete WebSocket responses retain
  usage and response ID and keep the previous successful continuation base.
  The additional SSE repeated-response test passes: the same response ID is
  relayed twice but usage is accumulated once; missing/malformed counts are not
  estimated. These checks required no production protocol change.
- Existing large-context continuation fixtures pass for Native and imported
  accounts: 14,686,529 initial bytes, twenty incremental turns, one WebSocket,
  one warmup, zero HTTP fallbacks and zero full-history recoveries. Rejection,
  cancellation, state-loss recovery and account/route isolation fixtures pass.
- Real installed Linux Codex **0.162.1** passes online catalog discovery and
  same-process ETag refresh using disposable homes and synthetic upstreams.

All these are local candidate checks, not a released compatibility expansion.
The new restore contract's complete ten-series rerun, actual 0.163 asynchronous
compaction, real-provider large-context warmup, Windows/macOS and App/IDE
acceptance remain pending. No real credentials, paid models or running user
clients were used by these fixtures.

## Protocol decisions

- **Windows loopback discovery:** the development candidate advertises
  `http://localhost:<port>/v1` to Codex while retaining the numeric loopback
  listener. The reported installed Windows engine `0.162.0-alpha.17.2` failed
  discovery through `127.0.0.1` under its existing proxy settings but succeeded
  through `localhost` in a synthetic isolated probe. Startup upgrades only the
  previous exact `127.0.0.1` URL on the same port when the owned fields are still
  applied; other listeners and external edits remain conflicts. Native and
  API-only catalog modes preserve original settings across repeated recovery
  and shutdown. No system proxy, registry, environment or client process changes
  are made by this product fix. Linux Codex 0.162.1 passes live catalog discovery,
  synthetic response routing and same-process ETag refresh with an unreachable
  proxy and a `localhost`-only exception. This does not cover proxy policies
  that explicitly proxy localhost, modified localhost resolution or a packaged
  Windows build; candidate-binary acceptance remains pending.
- **Provider capabilities:** EMP's normal integration changes the existing
  Codex endpoint and catalog settings. It does not replace them with a new
  custom provider. In 0.162, omitted capability overrides retain provider
  defaults, including native OpenAI remote compaction. No extra custom-provider
  capability declaration is needed for this integration. See
  [Codex's capability defaults](https://github.com/openai/codex/blob/rust-v0.162.0/codex-rs/model-provider/src/capabilities.rs).
- **Retry advice:** synthesized SSE and WebSocket errors expose
  `error.headers["Retry-After"]`, while retaining the legacy seconds field.
  Converting a pre-output native failure to HTTP reads this header first,
  including case-insensitive names, then falls back to the legacy field.
  Numeric and HTTP-date advice share the existing bounded parser. HTTP clients
  receive the actual `Retry-After` header. Other upstream headers are not copied.
  See [Codex's streamed error reader](https://github.com/openai/codex/blob/rust-v0.162.0/codex-rs/codex-api/src/sse/responses_error.rs).
- **Tools:** the real-engine fixture exercises tool input/results through
  compaction and forks. Source contracts additionally exercise 0.162
  `additional_tools`, client Tool Search input/results, repeated definitions and
  equal function names in distinct namespaces through Chat, Anthropic and
  external Responses projection. Streaming restoration suppresses only search
  argument deltas and retains ordinary function deltas. Real native HTTP tests
  verify that incremental catalog/history items pass through unchanged. The
  real-engine contract additionally runs deferred Tool Search in Codex 0.162.0:
  search, discover `alpha.read` and `beta.read`, execute both local read tools,
  return their results, and finish the same turn. It covers Chat, Anthropic,
  external Responses and native HTTP forwarding with Responses Lite incremental
  catalogs. Actual request receipts verify search item types and paired history.
  These checks do not establish every incremental event combination or real
  upstream-model behavior.
- **Instruction roles:** Responses Lite developer/system messages are projected
  to ordered Anthropic system blocks. They retain instruction priority instead
  of being rejected or downgraded to user messages. Projection failures retain
  their content-free reason in EMP errors.

Use the [native restore harness](../scripts/native_restore_acceptance/README.md)
for isolated behavior checks. Record the actual engine version and tested paths
when moving the maintenance window.

Run the real-engine Tool Search contract with an official Linux 0.163.0 binary:

```sh
EMP_TEST_CODEX_CLI=/path/to/codex cargo test -p emp-app --lib \
  installed_codex_discovers_and_calls_same_named_tools_across_protocols \
  -- --ignored --nocapture
```

The contract creates disposable Codex homes, randomized loopback EMP/upstream
ports and local read fixtures. It disables plugins and uses synthetic keys; it
neither uses the installed EMP service nor consumes real provider quota.

## Runtime discovery and update acceptance

The existing inert installation matrix covers CLI, App and IDE paths on Windows,
Linux, Intel macOS and Apple Silicon macOS. Host metadata is checked separately
from the selected engine version; these checks never execute the inert files.
Passing this matrix does not establish inference in every bundled engine.

Updater fixtures cover interrupted downloads, the existing three-retry budget,
stage/error receipts surviving cleanup, actual process handoff and preservation
of configuration and encrypted credentials. Native Windows/macOS system-proxy
behavior and the next official-package update need host-specific acceptance.
Do not replace that acceptance with a private build installed on a user's host.
