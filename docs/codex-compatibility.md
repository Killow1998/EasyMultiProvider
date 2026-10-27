# Codex maintenance window

EMP follows the latest stable Codex minor series and its previous nine series.
Patch releases count within their series; alpha and beta releases do not move
the window. The engine used by the CLI, App or IDE is the compatibility input,
independent of the host application's version. This policy does not introduce a
global version rejection: individual operations still check their interfaces.

## Current acceptance on 2026-10-26 UTC

The maintenance target is **0.155–0.164**, following the official stable
[0.164.0 release](https://github.com/openai/codex/releases/tag/rust-v0.164.0)
(2026-10-23 04:48:10 UTC). Series 0.154 remains in the historical record below.

The EMP 0.13.14 implementation, validated before tagging at commit `5054928`,
passed the created-history
contract with official Linux engines **0.155.1, 0.156.1, 0.157.1, 0.158.0,
0.159.3, 0.160.1, 0.161.0, 0.162.1, 0.163.0 and 0.164.0**. Each passed
51 assertions: real MCP before/after external compaction; parent, early/later
forks and grandchild; selected-history migration; native resumes; exact native
settings and backup preservation; and an unchanged second restore. Downloaded
archives and executables were verified against official digests; installed
engine hashes and the candidate binary hash were recorded. Engines and fixture
state were removed after each run. All requests used synthetic credentials and
loopback upstreams.

Codex **0.164.0** also passed **49 native API contracts**, including seven
real-engine checks. The new check switches between models with changed Code Mode
instructions, switches to direct tools, restores Code Mode and resumes the saved
model in a new process. All five turns complete through Chat Completions,
Anthropic Messages and external Responses. The actual rollout must contain
incremental declarations and a typed removal notice. Existing checks cover
catalog refresh, proxy bypass, steering, WebSocket interrupt/continuation,
same-named Tool Search and standalone image generation/editing. The related
protocol, router and Claude CLI suites pass **57**, **8** and **60** tests.

### Code Mode collaboration

The official standalone Linux **0.164.0** CLI and matching
`codex-code-mode-host` passed nine additional cases: external Responses,
Chat Completions and Anthropic Messages, each with cross-service subagents
Off, On-demand recovery and On. The real engine executes `spawn_agent`,
`send_message`, `followup_task` and `wait_agent` through Code Mode. Acceptance
requires the selected child model's actual completion, delivery of its latest
result to the parent, and that result surviving parent resume in a new process.
The upstreams and credentials are synthetic; no subscription inference is used.

Saved typed `agent_message` items preserve plaintext task/message content and
the correct sender/recipient, matching upstream
[#55633](https://github.com/openai/codex/pull/55633). On-demand recovery therefore
makes **zero recovery calls** for these messages. Off still rejects the external
child before any external inference and returns the settings guidance. On also
passes; this check does not measure its speed. Older encrypted messages still
use the existing on-demand recovery path.

Run from the repository root:

```sh
python3 -B -m scripts.code_mode_collaboration_acceptance \
  --codex /path/to/codex --emp /path/to/EMP --output /path/to/evidence
```

Install the matching official `codex-code-mode-host` beside the standalone CLI: the CLI
archive alone does not include it and Code Mode can otherwise fall back to direct
tools. The harness fails closed on that fallback, isolates homes and loopback
listeners, and records binary hashes, requests, events and typed agent messages.
It uses `wait_agent` to await parent delivery: a child's completed turn alone
does not establish that its final message has reached the parent's next input.
Child acceptance also waits for the matching `turn/completed` event and saved
turn status to agree; an immediate history read can precede the final rollout write.

### Installed App and IDE engines (2026-10-26 UTC)

The public EMP **v0.13.16** Windows x64 package and Intel macOS package were
verified against their release digests and run in disposable native environments.
The probes used the engines already installed with each App/IDE, with matching
Code Mode hosts:

| Platform and entry point | Engine | Passed cases |
| --- | --- | --- |
| Windows x64 App | 0.164.0-alpha.11.1 | 9 collaboration + 3 tool-catalog revisions |
| Intel macOS App | 0.164.0-alpha.11.1 | 9 collaboration + 3 tool-catalog revisions |
| Windows x64 IDE | 0.154.0-alpha.6.2 | 3 legacy model-switch/resume |
| Intel macOS IDE | 0.155.0-alpha.16.3 | 3 legacy model-switch/resume |

Each App collaboration set covers three protocols and all three subagent modes,
including actual child completion, parent delivery and resumed history. Each
catalog case changes the Code Mode definition, removes it for direct tools,
redeclares it and resumes in a new process; saved history must contain the
incremental declarations and typed removal notice. The older IDE engines run
their own model-switch/resume baseline without forcing 0.164-only features.
The Windows 0.154 IDE result remains historical coverage and does not move the
stable maintenance window.

The native adapter reuses the existing collaboration assertions with native pipe
reading and subprocess launch. Its SQLite reader now closes explicitly so Windows
can remove the disposable history; a focused collaboration rerun exits cleanly.
These are native engine/package checks with synthetic credentials and loopback
upstreams, without real model calls or the Linux harness's memory bound. Existing
App sessions and EMP installations remain untouched.

### Incremental catalog updates

Codex 0.164 enables incremental tools by default. Its
[catalog implementation](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/core/src/context/world_state/top_level_tools.rs)
appends revised definitions and removal notices to existing history. EMP now
resolves that catalog chronologically: later declarations replace the same
identity, removed tools stop being offered, and a later declaration can restore
them. Historical calls/results and namespace aliases remain intact. Conflicting
definitions in the same batch still fail before forwarding. Endpoints supporting
in-place loading retain unchanged loading boundaries; obsolete declarations are
removed only from the projected request, never from saved history.

Removal parsing uses Codex's per-content classification when present. For custom
providers, Codex's
[client](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/core/src/client.rs)
strips that internal metadata; EMP then recognizes the complete official
developer-notice format. User messages, quoted fragments and differently
classified content cannot remove tools. The local Claude CLI response validator
uses the same removal parser and rejects removed tools or superseded tool kinds.

Async compaction and App Server WebSocket compression remain upstream
`UnderDevelopment` features, disabled by default in the
[0.164 feature catalog](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/features/src/lib.rs).
The existing background-compaction Beta setting and acceptance scope are
unchanged. App Server compression is separate from EMP's Responses transport.
The upstream context-estimation fix does not change EMP's TPS or usage formulas.

### Prerelease protocol check (2026-10-27 UTC)

Official Linux [**0.166.0-alpha.2**](https://github.com/openai/codex/releases/tag/rust-v0.166.0-alpha.2)
passed three targeted App Server checks through
the current EMP candidate. This is prerelease evidence; the stable maintenance
window remains **0.155–0.164**.

| Upstream change | Observed result and EMP boundary |
| --- | --- |
| Flat `ThreadItem` with `startedAtMs` / `completedAtMs` | Three actual turns, including resume in a new process, retain replies and valid lifecycle times. The payload stays flat. EMP history does not deserialize an upstream `ThreadItem` enum. |
| `ThreadUsage.nativeUsageUsdMicros` | The actual App Server preserves absent/null, zero and nonzero backend values independently of credit/USD estimates. Four account-usage reads leave EMP's three inference usage records and estimated USD unchanged. EMP bills its existing inference receipts; it does not import or add these account totals. |
| Negotiated App Server Zstd | A request without server acknowledgment and a legacy client both retain JSON text. With both sides enabled, initialization and configuration reads use binary messages and one persistent stream per direction; reconnect starts fresh streams. Enabling the feature does not change EMP's stdio helper protocol. |

Compression is still `UnderDevelopment` and default-off in this tested engine.
It belongs to App Server RPC; EMP's Responses inference transport is unchanged.
The check adds no runtime codec, billing data source, setting or dependency.
It uses synthetic credentials, a local backend and a network namespace without
external access. It does not run real subscription inference.

The reusable harness requires Python 3.14 and the optional `websockets==17.2`
test client, installed in a disposable environment. Run from the repository root
inside the isolated test network:

```sh
python3 -B -m scripts.codex_upstream_acceptance \
  --codex /path/to/codex-0.166.0-alpha.2 \
  --emp /path/to/candidate/EMP --output /path/to/evidence
```

The output records the exact engine and candidate hashes, account-usage cases,
message encodings and bill totals. The official downloaded engine is temporary;
it is not installed over the user's CLI or used to advance the stable window.

## Acceptance on 2026-10-16 UTC

The maintenance target at that checkpoint was **0.154–0.163**, following the official stable
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

### Standalone image tools

Codex's standalone `image_gen.imagegen` extension sends JSON to
[`images/generations` and `images/edits`](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/codex-api/src/endpoint/images.rs).
EMP accepts both beneath `/v1` and forwards the original image model and payload
to the subscription backend. Each image request uses `x-codex-image-turn-id` to
associate with its originating HTTP or WebSocket inference. **Settings → Routing
settings** controls both Auto Review fallback and image account selection. Images
default to **Follow conversation**; **Specific account** selects Native or an
imported subscription, and **Automatic** selects an eligible subscription using
shared and image-specific quota snapshots. A known positive balance precedes an
unknown one. Disabled/invalid accounts, known Free plans and exhausted capacity
are excluded from automatic selection. Expired quota windows no longer block it.

The image account is frozen for the turn, even if preferences or quota snapshots
change during subsequent inference frames. Known server-side image references
retain their originating account across turns; automatic routing rejects unknown
references with guidance to attach the local image file. Reference metadata and
turn associations are bounded to 4096 entries each and expire after 24 hours;
credentials and image bytes are not cached here. Missing turn associations,
disabled pinned accounts and references from conflicting accounts fail explicitly.
Image limit headers and 429 responses inform selection for later turns. No
quota-driven replay or cross-account retry is made after an image request is sent.

The image contracts cover Native/imported account selection,
authentication boundaries, unchanged upstream errors and image quota headers,
no retry after 404/429, failed credential refresh preserving 401, and image usage
receipts with duplicate upstream request IDs counted once. Six routing contracts
exercise fixed/automatic selection, independent Auto Review settings, successful
and failed quota feedback, expired windows, cross-turn references and unavailable
pinned accounts. Browser E2E verifies bilingual controls, persisted settings,
failed-save recovery, nested navigation, narrow layouts and the unified Shadow.
The actual Linux Codex **0.163.0** extension was also verified generating/editing
a synthetic PNG, saving it and passing it into the next inference. All accounts,
upstreams and images in these checks are synthetic.

Run the real extension check explicitly with `EMP_TEST_CODEX_CLI` pointing to the
chosen engine and `cargo test -p emp-app --lib native_api_contract::images --
--include-ignored`. No image-specific Codex configuration or model remapping is
required.

### Exploratory async compaction

Async compaction is outside EMP's routine compatibility suite and release gate
while the upstream feature is under development. A stable Codex CLI version
alone does not make this feature stable. Retain the manual harness and existing
results for reproducibility; defer further async-specific adaptation and
steering/account/routing combinations until the feature itself becomes stable,
then reassess whether to include it in EMP's maintained test scope.

EMP exposes an opt-in Beta switch for Codex 0.163+ without expanding that routine
acceptance scope. The setting preserves a user-specified trigger percentage
(default 65% of the full window), and its help distinguishes the trigger from
compression retention. After saving, restart Codex or the App/IDE Codex backend
to reload the configuration; EMP itself need not restart. Restore Native and
Quit restore the pre-EMP value, also requiring Codex to reload it. Existing
configured feature values are retained until the user explicitly changes them.

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

## Candidate idle-history restoration on 2026-10-21

The current unpublished candidate adds idle-history conversion to explicit
Restore Native, normal Quit and offline Restore. It retains targeted migration
and the fast configuration-only OS termination path. Busy lineages and
uncoordinated/unfinished history are deferred without changing their bytes.
Pending conversion remains accessible in the native WebUI and survives restart.

Focused history contracts and the disposable
[`e2e_native_history_restore.py`](../scripts/shadow_frontend/e2e_native_history_restore.py)
cover both directions of active forks, independent idle conversion, restart,
backups, opaque-state/tool-pair preservation, config conflicts and idempotence.
This is the candidate lifecycle contract; the dated ten-series results retain
their original contract and do not represent a new ten-engine run.

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

Run the real-engine Tool Search contract with an official Linux 0.163+ binary:

```sh
EMP_TEST_CODEX_CLI=/path/to/codex cargo test -p emp-app --lib \
  installed_codex_discovers_and_calls_same_named_tools_across_protocols \
  -- --ignored --nocapture
```

The contract creates disposable Codex homes, randomized loopback EMP/upstream
ports and local read fixtures. It disables plugins and uses synthetic keys; it
neither uses the installed EMP service nor consumes real provider quota.

For the catalog update/removal and saved-model resume contract, use an official
Linux 0.164+ binary:

```sh
EMP_TEST_CODEX_CLI=/path/to/codex cargo test -p emp-app --lib \
  installed_codex_switches_and_resumes_incremental_tool_revisions \
  -- --ignored --nocapture
```

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
