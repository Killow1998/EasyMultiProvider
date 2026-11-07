# Codex maintenance window

EMP follows the latest five stable Codex minor series: the latest stable series
and its previous four series. This replaces the ten-series policy on 2026-10-28 UTC.
Patch releases count within their series; alpha and beta releases do not move
the window. The engine used by the CLI, App or IDE is the compatibility input,
independent of the host application's version. This policy does not introduce a
global version rejection: individual operations still check their interfaces.
Older series leave routine maintenance; their dated acceptance records remain.

## External WebSocket continuation (2026-11-04 UTC)

The development candidate based on v0.13.25 removes repeated history resends
from stateless external tool loops. It projects an empty completion ID on the
downstream WebSocket, using Codex's existing full-input path. Active response IDs,
internal accounting IDs and Native WebSocket incremental state are retained.
No conversation-history cache is added. HTTP/SSE response IDs are unchanged.

Real Linux Codex engines **0.162.1, 0.163.0, 0.164.0, 0.165.0 and 0.166.0**
each passed Chat Completions, Anthropic Messages and external Responses tool loops
against isolated EMP and simulated upstreams: **15 protocol sessions, 75 model
requests, 30 actual dynamic-tool invocations and zero history rejections**.
Each protocol session records exactly 50 input / 10 output fixture tokens and
five distinct original accounting response IDs. Tool discovery, ordered history
and both actual tool results reach the final model request.

Disabling only the completion-ID projection makes the same Codex 0.166 test fail:
five model requests produce four history rejections. Restoring it removes them.
The 50 ordinary Native API contracts also pass, including incremental continuation,
HTTP recovery and interruption. The installed Claude CLI's multi-turn HTTP and
WebSocket checks retain exact tool results and multiple images against a simulated
CPA endpoint. These tests use synthetic credentials and do not spend model quota.

```sh
EMP_TEST_CODEX_CLI=/path/to/codex cargo test -p emp-app --lib \
  installed_codex_external_tool_roundtrips_do_not_request_history_replays \
  -- --include-ignored --nocapture
```

## Chat output acceptance (2026-11-02 UTC)

The local candidate based on `70cf0e1`, still reporting EMP 0.13.24, was tested
with the installed Codex 0.166.0 Linux engine. This is separate from the
published 0.13.24 executable. The maintenance window is unchanged.

Chat Completions text accompanying structured tool calls now completes with
`phase: "commentary"`; standalone answers complete with `phase: "final_answer"`.
HTTP, SSE output-item completion and the terminal response carry the same phase.
Text deltas remain incremental; the converter does not buffer an entire answer
to choose a phase. Codex persists both phases correctly in its actual rollout.

Native DeepSeek DSML envelopes in the text channel now produce a protocol error
instead of leaking tool parameters or claiming successful completion. The text
boundary retains split marker prefixes across SSE chunks. Backtick-quoted code
examples and ordinary `<think>` / `<tool_call>` documentation remain text.
Only structured tool calls are executed; malformed text is not reinterpreted as
an executable command. Existing reasoning-field suppression remains unchanged.

- All `emp-protocol` tests pass. The focused `user_journeys chat_` checks pass
  five actual-EMP process journeys, covering HTTP/SSE phases, tool-result
  continuation, reasoning/usage separation, refusal and damaged tool output.
- Real-provider candidate checks complete **50 model requests across 30 user
  turns**, including GLM Flash and DeepSeek code/tool interactions. A dedicated
  progress-message case persists **four commentary and four final messages** for
  GLM Flash, and **five commentary and four final messages** for DeepSeek. No
  raw reasoning or tool markup appears in these samples. Every request is
  classified as a model test. The earlier published-build GLM samples are
  retained separately rather than relabeled as candidate results.
- `cargo clippy -p emp-protocol -p emp-app --all-targets -- -D warnings`, Rust
  formatting and diff checks pass. Existing EMP instances were not restarted;
  installed login and encrypted credential files remain unchanged.

## Current target and acceptance (2026-11-06 UTC)

The maintenance target is **0.163–0.167**, following the official stable
[0.167.0 release](https://github.com/openai/codex/releases/tag/rust-v0.167.0)
(2026-11-06 01:40:00 UTC). GitHub identifies it as the latest release with
`prerelease=false`.

| Target series | Existing evidence |
| --- | --- |
| 0.163–0.164 | Dated Linux engine acceptance below; App/IDE checks are listed separately with their exact engine versions. |
| 0.165 | Official Linux 0.165.0 engine passed the local EMP 0.13.17 candidate contracts recorded below. |
| 0.166 | Official Linux 0.166.0 engine passed the local EMP 0.13.20 maintenance candidate contracts recorded below. |
| 0.167 | Official Linux 0.167.0 engine passed the local EMP 0.13.26 maintenance candidate contracts on 2026-11-06 UTC. |

## Feature ownership and acceptance — Codex 0.167.0

The stable 0.166.0 → 0.167.0 review uses the [official changelog](https://learn.chatgpt.com/docs/changelog)
and tagged source. This stage maintains the existing feature boundary.

| Upstream change | EMP maintenance and observable result |
| --- | --- |
| [Memory default models #58091](https://github.com/openai/codex/pull/58091) | Default hints follow the trusted runtime. Unset 0.167 memory models resolve to GPT-6 Luna extraction and GPT-6.1 Terra consolidation; 0.163–0.166 retain their earlier defaults. Explicit original choices survive EMP overrides and exact restore. A separate real-engine case verifies extraction, consolidation, a later memory read and the ledger using the new defaults. |
| `codex agents --no-daemon`, unread agent results and backend/session switching with retained drafts | Client-owned agent-center and navigation behavior; EMP retains its existing request and history paths. Three protocols × three collaboration modes pass actual child execution, return and resume. |
| Terminal image thumbnails and clickable image paths | Client-owned rendering. Model catalogs retain text/image modalities and the existing external image projection. |
| API-key/cloud sign-in alternatives and optional GCP setup | Client-owned sign-in UI. Existing EMP authentication and explicitly selected upstreams remain unchanged. |
| [Experimental Windows/macOS system-proxy opt-in #58157](https://github.com/openai/codex/pull/58157) | This PR exposes the existing `respect_system_proxy` flag in the experimental menu; its false default stays unchanged. Two real-engine Native requests verify that EMP preserves the opt-in beta header only when enabled. Linux localhost-only proxy bypass also passes. OS proxy discovery remains a Codex client operation. |
| Inline patch approval and provider-switch history access | Keep approval/sandbox settings and ordered tool results. The history contract restores the original configuration exactly; three external tool protocols complete without history rejections. |
| [Responses Lite cancellation #57864](https://github.com/openai/codex/pull/57864) | `interrupt_on_cancel` defaults to false in 0.167. An isolated real-engine opt-in sends `response.interrupt`, receives its acknowledgement and ends the turn without replaying inference. Native and external steering also pass on 0.167; EMP leaves the client's feature choice intact. |
| [Retained-output context estimation #58404](https://github.com/openai/codex/pull/58404) | Two real-engine cases return a long retained output followed by failure or a completion without Usage. Each triggers exactly one V2 compaction containing that output, continues and resumes a new process. Missing token reports remain unknown in EMP's call records. |
| Lower idle allocator overhead in the standalone musl CLI | Codex binary optimization; EMP does not change its allocator or add a second runtime. |
| [Stable deferred tool world #58410](https://github.com/openai/codex/pull/58410) | The existing incremental tool bridge resolves namespace-qualified discovery, revisions and removed/redeclared tools. The 0.167 installed-engine external continuation run includes actual discovery and invocation of two same-named tools, across Chat, Anthropic and external Responses. |
| [Incremental Exec flag #58122](https://github.com/openai/codex/pull/58122) | This PR adds the `incremental_exec_tools` flag with a false default; it does not add a new wire representation. EMP retains the existing declaration/removal bridge and does not force the client flag on. |
| [Early child-edge persistence #58129](https://github.com/openai/codex/pull/58129) | The parent edge can exist before child metadata or history. The persisted edge format is unchanged. EMP's metadata-only reader already resolves such children, including later-arriving edges, without writing the Codex database. The session-directory contracts verify these cases. |
| [Paginated assistant text release #58751](https://github.com/openai/codex/pull/58751) | Codex releases superseded live text while persisted replay uses its ordinary reducer. Both long-output cases use paginated history and two assistant texts in one turn; compaction and process resume retain both texts. EMP continues to read persisted history. |
| [Negotiated Code Mode output truncation #58125](https://github.com/openai/codex/pull/58125) | Negotiation belongs to the Code Mode host connection. Model-facing results retain the existing structured text/image content shape. EMP keeps ordered results and uses its current projection; no extra truncation pass or output buffer is added. |
| [Provider request preparation #58121](https://github.com/openai/codex/pull/58121), [transient auth reload #58324](https://github.com/openai/codex/pull/58324) and [keyring fallback #58343](https://github.com/openai/codex/pull/58343) | These are client credential lifecycle changes. EMP preserves the current caller-key boundary, account identity and credential ownership; no new authentication fields appear in model requests. Existing login/restoration contracts remain the gate. |
| [Provider OAuth file storage #58219](https://github.com/openai/codex/pull/58219) | The optional provider grant store is `secrets/gateway_oauth.json`. Codex owns it; EMP does not replace it when applying or restoring its own Native integration. |
| [Turn-scoped standalone search #58189](https://github.com/openai/codex/pull/58189) and [native Guardian evidence #58040](https://github.com/openai/codex/pull/58040) | Search permissions and Guardian command evidence are prepared within Codex. The existing Native request path preserves their payload; Auto Review remains subscription-routed, and EMP does not change approval or network permissions. |
| [Audio file references #58430](https://github.com/openai/codex/pull/58430) | The protocol can represent uploaded audio references; Codex explicitly rejects them in unsupported runtime inputs. This representation change does not require a new EMP audio-upload feature. Existing supported input modalities and explicit unsupported-input errors remain. |

The final feature run completed at **06:24:13–06:25:16 UTC** on 2026-11-06.
The verified official Linux executable has SHA-256
`cdcb45aaa01d0f14e298985e09465aebccd47f64ecd298aea44814af7b62bbda`.
The unreleased EMP 0.13.26 candidate, based on `58d79c4`, has SHA-256
`5dbcf057474df4d91683b116e60cbef46b353c4c8ed4501c4dedac20794a76a2`.
It is separate from the installed/published executable.

| Contract | Result on 2026-11-06 UTC |
| --- | --- |
| Complete feature acceptance | Eight groups and 77 synthetic inference requests: 40 effort requests over HTTP/SSE and WebSocket, 21 memory requests, four fork requests and 12 Terra catalog requests. Fresh/legacy memory imports pass without inference. |
| Created history and native restore | 55 assertions: real MCP around compaction, parent/fork/grandchild resumes, per-thread dynamic tools, exact original configuration and permissions, unchanged backups and idempotent restore. |
| Code Mode | Nine three-protocol/three-mode cases pass; enabled modes complete children and process resume without encrypted-history recovery calls. |
| App Server protocol | Three turns plus process resume preserve item timestamps; four native-USD states remain separate from estimates; three Zstd negotiation states and reconnect pass. |
| Installed-engine external continuation and proxy | Chat, Anthropic and external Responses complete tool loops without history rejections. A localhost-only proxy exception retains local catalog access. |
| Additional lifecycle run, 07:34:34–07:34:39 UTC | Two retained-output cases make eight ordinary generation requests and two V2 compaction requests. Unknown usage, paginated history with superseded text and process resume pass on the same final candidate. Three installed-engine controls cover opt-in cancellation and Native/external steering. |
| Shared-fixture and system-proxy metadata regression | Forty HTTP/SSE and WebSocket requests preserve effort changes and continuation across Native, external Responses, Chat and Anthropic. Two additional Native requests verify the system-proxy beta-header opt-in. These Linux metadata checks do not read Windows/macOS proxy settings. |
| Preferences and browser behavior | Six preference contracts pass, including version-specific defaults and original-choice preservation. Bilingual narrow/desktop settings E2E and current-review navigation pass; both memory help bubbles expose the measured token-share reference. |
| Static checks | Strict affected-crate/all-target Clippy, Rust formatting and diff checks pass. |

All model/backend responses in these tests are synthetic and loopback-only.
The standalone engine was not installed over Xian's active CLI. The dated
0.163–0.166 evidence below retains its original engines, candidates and dates;
this stage adds the 0.167 Linux run rather than relabeling those old runs.

## Feature ownership and acceptance — Codex 0.166.0

The 0.13.21 release additionally fixes mixed text/image tool-result projection
for external Chat and Anthropic models and applies the existing raw-reasoning
output policy to every external Responses delivery. The 2026-10-31 regression
and real-Gemma receipts are recorded in [the switch failure record](claude-switch-failures.md#external-tool-images-and-raw-reasoning-2026-10-31-utc).
The dated engine runs below remain the evidence of their original candidates.

The 2026-10-30 UTC review compares the exact stable 0.165.0 and 0.166.0 tags.
EMP keeps the current feature boundary; client-owned changes remain in Codex.

| Upstream change | EMP maintenance and observable result |
| --- | --- |
| [Dynamic tool replacement #57553](https://github.com/openai/codex/pull/57553) | History repair now retains `dynamic_tools_updated` records instead of rejecting the rollout as unsupported. Existing compaction metadata remains intact. Four restored threads retain their own latest tool catalogs, including an empty catalog and a grandchild's independent tools. |
| [Reasoning usage on resume/fork #57839](https://github.com/openai/codex/pull/57839) | Preserve the optional `last_usage_includes_reasoning` field in token-count history. The repair contract checks its exact value; Codex owns the accounting change that prevents premature compaction. |
| [Memory consolidation service tier #57568](https://github.com/openai/codex/pull/57568) | Verify `[memories].consolidation_service_tier` independently of the main turn: omitted/default, Flex and Fast reach the upstream as default, `flex` and `priority`. Main-turn Fast remains independent. Native, imported and external launchers all complete extraction, consolidation and a later memory read. No new EMP setting is required. |
| Model catalogs and GPT-6.1 Terra | Default and originator-scoped installed-engine catalog checks pass. Native and imported Terra catalogs retain the pinned capabilities and all six efforts. Known model modalities also reach imported external catalogs; see the [registry merge contract](official-model-registry-spec.md#discovery-merge). |
| App Server item timestamps, native USD and negotiated Zstd | Three real-engine turns and process resume preserve flat items with lifecycle timestamps. Absent/null, zero and positive native USD remain distinct from estimated usage and do not add or reprice EMP ledger rows. Three compression negotiation states verify text fallback, persistent binary streams and fresh reconnect codecs. This is App Server transport, separate from Responses WebSocket. |
| TUI transcript selection, asynchronous question/reply Markdown, `queue --resume`, `doctor --network-only` and safety explanations | Codex-owned client features. EMP forwards the existing model requests; it does not duplicate the UI or command implementation. |
| Approval/permission resume and fork behavior | The restored fixture retains exact `approval_policy = "never"` and `sandbox_mode = "read-only"` settings. EMP does not write new permission defaults. |
| Composer image recovery, macOS standalone cleanup, descendant unarchive and App Server lifecycle fixes | Codex-owned storage/process behavior. Existing EMP image and history boundaries remain; these upstream fixes do not require another EMP session or process manager. |

The final feature run completed at **07:46:26–07:46:58 UTC**. The installed
official Codex 0.166.0 executable has SHA-256
`b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8`.
The local EMP 0.13.20 candidate, based on `c641b8b`, has SHA-256
`2495bd85ebaf5a247333212a7173a059bf65f82117ae701ff5f4e3af4ebb9031`.
It is separate from the published 0.13.20 executable.

| Contract | Result on 2026-10-30 UTC |
| --- | --- |
| Complete feature acceptance | Eight groups, 77 synthetic inference requests: 40 HTTP/SSE and WebSocket effort requests, 21 memory requests, four fork requests and 12 Terra requests; external memory import requires no inference. |
| Created history and native restore | 55 assertions pass on the final candidate, including MCP before/after compaction, four native resumes, per-thread dynamic tools, unchanged original configuration/backups and idempotent restore. The published 0.13.20 baseline reproduces `unsupported_rollout_record` on the new tool records before repair. |
| Capability discovery and persistence | 35 app catalog tests pass, including discovery → selection → published catalog → restart and offline upgrade of old HTTP/Claude CLI model-list caches. The focused discovery, registry and state contracts pass 33 tests. Live/manual/observed restrictions and saved context/output/effort settings survive. |
| Installed-engine catalogs and transport | Seven explicit real-engine contracts pass: default/scoped catalogs, localhost proxy bypass, steering, interruption and incremental tool/model changes. |
| Code Mode and terminal errors | Nine collaboration cases pass across three protocols and three modes. Four memory-selection cases and ten terminal model-not-found cases pass with one inference per case. |
| App Server protocol | Timestamp/USD checks and three Zstd negotiation states pass. |
| Static checks | Workspace/all-target Clippy with warnings denied, Rust formatting and diff checks pass. |

The last three protocol groups above used the earlier candidate SHA-256
`a6a9fe51cd991f875143061b55c51cd0332247f4cf9f6e9b0f454ab196ce484a`,
before the final legacy-Claude registry additions. The final 77-request run,
55-assertion history run and catalog tests use the final registry. These runs use
synthetic credentials, disposable homes and loopback upstreams, with **zero real
provider calls**. Installed services and account files remain unchanged. The
0.162–0.165 records below retain their original dates, engine versions and EMP
candidates; they are not relabeled as tests of this candidate.

The stable feature registry still marks async compaction and skill-catalog deltas
as `UnderDevelopment` and default-off. EMP's background-compaction Beta remains
unchanged. App/IDE and non-Linux installed-package evidence retain their own
dated records below.

## Feature ownership on 2026-10-28 UTC — Codex 0.165.0

The release adds the built-in Vertex AI provider (`model_provider = "gcp"`),
GPT-6.1 Terra, repository filtering and directory grouping for sessions,
experimental attached thread forks, and default support for changing reasoning
effort during a conversation on supported models. When memories are enabled,
fresh profiles without a v1 workspace default to memory v2; existing workspaces
and explicit version or transition opt-outs retain their selection. This does
not enable memories in profiles where they are disabled.

Targeted source review and the new engine run cover these boundaries:

| Upstream change | Verified behavior and scope |
| --- | --- |
| [Startup approval reviewer defaults #56090](https://github.com/openai/codex/pull/56090) | The TUI's launch-default change belongs to Codex. EMP does not write approval or sandbox policy. Created-history acceptance preserves the exact configured `approval_policy = "never"` and `sandbox_mode = "read-only"` through shutdown and native restore. |
| [Originator-scoped model catalogs #56284](https://github.com/openai/codex/pull/56284) and [model-owned history/context tools #56313](https://github.com/openai/codex/pull/56313) | Real-engine default-catalog refresh and both `codex_work_cca` / `chatgpt_cca` scoped-cache refresh pass. Scoped thread refresh does not update the default manager's cached `model/list`; the tests inspect the relevant cache. The catalog contract preserves sparse history, notes and context tool metadata, including explicit empty descriptions. |
| [Fresh memory v2 #56211](https://github.com/openai/codex/pull/56211) | Four initial engine cases cover memory selection. Subsequent feature acceptance below completes extraction, consolidation and reading through Native, imported-subscription and external-model launchers; external-memory import also preserves v1/v2 selection and project scope. |
| [Terminal model-not-found errors #56445](https://github.com/openai/codex/pull/56445) | Preserve `model_not_found` on HTTP 400/403/404 and streamed errors. Real-engine failures end as `modelNotFound` after one inference request. EMP's existing HTTP-error privacy boundary remains: provide a model-selection explanation without publishing raw provider error details. |
| Mutable reasoning effort | Native and three external protocols pass HTTP/SSE and WebSocket turns, including unchanged effort, model switching and process resume. The ledger uses current turn metadata before historical overrides or the initial root effort. |
| GPT-6.1 Terra | Native and imported catalogs preserve the pinned model's capabilities; all six advertised efforts complete. Ultra retains the requested label while sending the catalog's actual `xhigh` effort. |
| Attached thread forks | Attachment, saved history, restart and bill attribution pass using the existing lineage bridge. No second attachment index was added. |
| Vertex AI, repository filters and directory grouping | These remain Codex-owned provider/client features. EMP enable/restore preserves GCP selection, endpoint and auth command. Vertex is not introduced as an EMP service. |

The [0.165 feature registry](https://github.com/openai/codex/blob/rust-v0.165.0/codex-rs/features/src/lib.rs)
still marks async compaction `UnderDevelopment` and default-off. EMP's existing
background-compaction Beta setting is unchanged. No user feature settings or
installed EMP/Codex instances were changed.

## Feature acceptance on 2026-10-28 UTC — Codex 0.165.0

The subsequent run completed at **05:22:58–05:23:33 UTC**, using the same
official 0.165.0 Linux engine and matching Code Mode host. EMP is the local
0.13.17 candidate based on `632a32e`, with executable SHA-256
`62e70ebe06b24f64ed6f14ef2ec90d1699d193d149e59e2130157f1c2a62adaf`.
Its results are separate from the earlier core acceptance below. Eight groups
pass, with **77 synthetic inference requests and zero real provider calls**.
Disposable homes, random loopback upstreams and a dead outbound proxy isolate
the fixture from installed accounts and services; no network namespace is used.

| Feature | Observable result |
| --- | --- |
| Effort changes over HTTP/SSE and WebSocket | Native plus external Responses, Chat Completions and Anthropic Messages each complete `high → low → low → xhigh`, then `medium` after process restart and resume: 40 inference requests. The upstream sees the effective effort and completed ledger rows retain the current selection. WebSocket full-history handoffs complete without duplicate inference. |
| Complete memory v2 pipeline | Native, imported-subscription and external-model launchers each complete extraction, consolidation, both background jobs and a later memory read: 21 inference requests. The configured extraction model is used; extraction retains the current imported account or uses Native for an external launcher. Global consolidation uses the configured model and Native account. V2 summaries are written without creating v1 memory files, and the fixture configuration remains unchanged. |
| External memory import | Three real App Server import cases pass: fresh v2, existing v1 and explicit v2 with legacy memory present. Imported bytes and project scope survive, repeat detection excludes already imported entries, and legacy files remain unchanged. No inference request is needed. |
| Attached forks | Attached and ordinary forks preserve parent history and attachment after process restart. Ordinary listing excludes the attached child; attachment-aware listing and resume retain it. Four inference requests reconcile to the existing parent-lineage bill. |
| Terra catalog and effort | Native and imported catalogs preserve ten pinned capability fields, including the 500,000 context window, 872,000 maximum, default Low, Code Mode, Responses Lite and mutable effort. `low`, `medium`, `high`, `xhigh`, `max` and `ultra` each complete on both account paths: 12 inference requests. Ultra sends `xhigh` while its ledger label remains Ultra. This measures protocol behavior, not real model speed. |
| GCP configuration ownership | The existing integration contract now also verifies that `model_provider = "gcp"`, its endpoint and auth command survive enable and restore. GCP inference/auth remains Codex-owned; no Vertex transport or credential service is added. |

Two defects were reproduced before repair. Stage-one memory requests use
`request_kind = "memory"`; ignoring that field bypassed the configured extraction
route. Native incremental WebSocket turns can retain an initial High root while
the current selection stays Low without another configuration-update item;
reading only root/history incorrectly recorded High. EMP now recognizes the
canonical memory marker and records current per-turn effort metadata, with
historical/root fallback for older requests. The existing fixed logging
vocabulary remains; no connection-state cache or extra runtime dependency was
added.

The reusable entry point is
[codex_feature_acceptance](../scripts/codex_feature_acceptance/__main__.py):

```sh
python3 -B -m scripts.codex_feature_acceptance \
  --codex /path/to/verified/codex --emp /path/to/candidate/EMP \
  --root /path/to/disposable-home --output /path/to/synthetic-evidence \
  --native-catalog /path/to/rust-v0.165.0/codex-rs/models-manager/models.json
```

Use `--suite effort`, `memory`, `imports`, `forks` or `catalog` for focused
maintenance. The full app regression passes **369 tests**, with 30 opt-in tests
excluded; integration passes **14 tests**. Strict workspace/all-target Clippy,
formatting and diff checks pass. Small receipts retain timestamps and binary
hashes; downloaded engines and disposable state are removed. Installed App/IDE
and Windows/macOS results retain their own dated records below.

## Acceptance on 2026-10-28 UTC — Codex 0.165.0

The official Linux x86_64 musl engine and matching Code Mode host were checked
against GitHub's SHA-256 digests before execution. The EMP candidate is based on
`632a32e` with the local model-error fix; it is not the published v0.13.17 binary.
All inference used synthetic credentials, isolated homes and random loopback
endpoints. No real provider or subscription was called.

| Contract | Result |
| --- | --- |
| Created history and native restore | 51 assertions passed: MCP before/after external compaction; parent, early/later forks and grandchild; selected-history migration; four native resumes; exact configuration/backup preservation and idempotent restore. |
| Explicit installed-engine contracts | 10 tests passed: continuation and upload recovery, WebSocket recovery, images, default/scoped catalogs, localhost proxy bypass, steering/interruption, same-named tools and incremental tool/model switches. |
| Code Mode collaboration | Nine cases passed: Responses, Chat Completions and Anthropic Messages, each with `off`, `on_demand` and `on`; enabled children complete and resume, disabled children remain blocked. |
| Memory selection | Four real-engine cases passed: fresh v2, existing v1, explicit v2 and disabled memory. The selected marker reaches the upstream; the other version does not. |
| Terminal errors with the real engine | Ten cases passed: HTTP 400/403/404 and statusless SSE `error` / `response.failed`, through Native and external Responses. Each ends with `modelNotFound` and exactly one inference request despite client retry limits of two. |
| Public HTTP/WebSocket rejection contracts | 42 combinations passed: Native HTTP/SSE opening failures; external Responses, Chat Completions, Anthropic Messages and Auto with HTTP, SSE-opening and client WebSocket delivery. No inference retry or protocol fallback occurs after `model_not_found`; private upstream detail remains hidden. |
| Transport, router, catalog and integration crates | 326 tests passed; two opt-in cases remain ignored. |
| App regression and static checks | 369 tests passed; 30 opt-in cases are excluded from this default run, including the ten installed-engine cases passed separately above. Workspace/all-target Clippy with warnings denied, Rust formatting and diff checks passed. |

The new boundary harness is [codex_0165_acceptance.py](../scripts/codex_0165_acceptance.py):

```sh
python3 -B -m scripts.codex_0165_acceptance \
  --codex /path/to/verified/codex --emp /path/to/candidate/EMP \
  --output /path/to/synthetic-evidence
```

The existing created-history and Code Mode harnesses remain the acceptance
entry points for those behaviors. The Code Mode harness now sends the actual
engine version when requesting the model catalog.

These results are Linux fixture acceptance. Installed Windows/macOS packages and
App/IDE engines retain their separately dated records. The 0.161–0.164 evidence
below was produced with earlier EMP candidates; it was not rerun or relabeled as
today's candidate. Downloaded engines and temporary homes are removed after the
small result receipts are retained.

## Acceptance on 2026-10-26 UTC

The maintenance target at that checkpoint was **0.155–0.164**, following the official stable
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
the candidate at that checkpoint. This is prerelease evidence; the stable
maintenance window at that checkpoint remained **0.155–0.164**.

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

**Free/EMP-key image MCP acceptance — 2026-11-07 UTC:** The actual Linux Codex
**0.167.0** engine completes six image-generation/editing journeys: paid Native
uses its native image extension; Free Native and EMP Local Key use the single
`emp_images.imagegen` MCP tool. The MCP cases retain `openai` and current sign-in,
exercise the normal approval response, forward the current turn ID and use the
selected subscription's credentials. All six persist the expected PNG bytes.
MCP results and the next inference contain a file path without image Base64.
The measured `tools/list` declaration is **453 UTF-8 bytes** before Codex's own
wrapping. The helper uses the existing EMP executable, without an extra runtime.

Integration contracts cover original/inline MCP entries, unrelated servers,
version 2/3 leases, repeat activation, switching the helper off, exact restoration
and unchanged Provider/permission/sandbox settings. Earlier Linux 0.166 and Intel
macOS **0.167.0-alpha.3.2** native-extension probes are retained in dated evidence;
the new MCP path keeps the original Provider instead of migrating or forking chats.
See [image setup](gateway.md#codex-image-tools-with-an-emp-local-key).
Official **EMP 0.13.28** on Intel macOS was then accepted with the embedded
**0.167.0-alpha.3.2** engine: App Server resumed two existing `openai` chats and
generated one image in each. Both turns completed and displayed Markdown image
links. Each MCP result contained only the saved PNG path. Native Free authentication, configuration and permission settings
remained unchanged during the calls; no fork or history migration was used.

These checks supplement the earlier, dated 0.163.0 image acceptance below.

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
