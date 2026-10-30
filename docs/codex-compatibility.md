# Codex maintenance window

EMP follows the latest five stable Codex minor series: the latest stable series
and its previous four series. This replaces the ten-series policy on 2026-10-28 UTC.
Patch releases count within their series; alpha and beta releases do not move
the window. The engine used by the CLI, App or IDE is the compatibility input,
independent of the host application's version. This policy does not introduce a
global version rejection: individual operations still check their interfaces.
Older series leave routine maintenance; their dated acceptance records remain.

## Automatic effort candidate (2026-10-29 UTC)

The unreleased candidate adds optional per-inference effort selection within one
user task. It defaults off. Real Codex **0.165.0** with a simulated upstream
passed Native/imported-subscription × SSE/WebSocket: four cases, 24 total main
and classifier requests. Each case executes real shell tools and transitions
Low → Xhigh → Low after a failure and successful repair. Separate contracts
cover full/delta/replayed history, retry reuse, disabled/unsupported requests,
invalid-classification fallback and parent-linked usage. The entry point is
`python3 -m scripts.codex_feature_acceptance --suite adaptive` with the existing
`--codex`, `--emp`, `--root` and `--output` arguments.

A live pilot used one Native subscription, GPT-6-Astra and a GPT-6-Luna Low
classifier on six deterministically sampled
[HumanEval+ Mini v0.1.10](https://github.com/evalplus/humanevalplus_release/releases/tag/v0.1.10)
tasks (seed 20261029). Each run submits Python to a network-isolated test tool,
then continues in the same task. The selected tasks contain 81 test cases per
arm. These are small function tasks, not repository-scale debugging.

| Run | Passed tasks | Mean task time | Total input tokens | Total output tokens |
| --- | ---: | ---: | ---: | ---: |
| Fixed Low | 6/6 | 7.25 s | 4,253 | 617 |
| Fixed Xhigh | 6/6 | 6.97 s | 4,324 | 689 |
| Initial automatic pilot | 6/6 | 13.22 s | 7,959 | 1,281 |
| Automatic with constrained JSON, later follow-up | 6/6 | 12.29 s | 8,311 | 866 |

The first three arms were interleaved by task. The final row reran only the
automatic arm later the same day after replacing plain-text classification with
a constrained enum. All 12 follow-up decisions selected Low without fallback;
the initial pilot had one invalid selection and retained Xhigh. Token totals
include classification; all calls reported usage. Times include classification,
main inference and local tool execution. Total tokens do not imply equal cost
across the main and classifier models.

This pilot shows classification overhead outweighing any saving on these small
tasks. It does not establish a quality or speed benefit on difficult coding work.
Keep the option off by default. The opt-in live harness is
[`scripts/adaptive_effort_benchmark.py`](../scripts/adaptive_effort_benchmark.py);
run `python3 -m scripts.adaptive_effort_benchmark --help` for its inputs.
The `--allow-live-calls` flag acknowledges subscription usage. Frontend acceptance
covers Chinese/English at 420/1280 widths, persistence, disabling and call details.
The candidate has not been published or deployed to the running EMP instance.

### Repository-task pilot (2026-10-29 UTC)

A separate live comparison used the same Native account and GPT-6-Astra, with
fixed Low, fixed Xhigh, or per-inference GPT-6-Luna Low classification capped at
Xhigh. All reported execution tiers were `default`. Three selected
[SWE-bench Lite](https://huggingface.co/datasets/princeton-nlp/SWE-bench_Lite)
SymPy cases required inspecting and editing the base repository through shell
tools. Models received the original issue and base source, without the official
solution or test patch. Their workspaces had no network access.

| Case and behavior | Fixed Low: seconds / tool calls | Fixed Xhigh: seconds / tool calls | Automatic: seconds / tool calls |
| --- | ---: | ---: | ---: |
| 17630: multiplying blocks containing ZeroMatrix loses matrix shape | 71.2 / 8 | 112.9 / 10 | 105.8 / 7 |
| 21379: substitution involving Piecewise raises PolynomialError | 48.8 / 4 | 128.7 / 8 | 73.6 / 5 |
| 21627: checking a cosh expression's zero assumption recurses | 82.1 / 8 | 125.7 / 7 | 87.4 / 6 |
| Mean time / total tool calls | 67.4 / 20 | 122.4 / 25 | 88.9 / 18 |

Each arm passed all three cases and their 140 listed FAIL_TO_PASS/PASS_TO_PASS
checks. Before model testing, the unmodified repositories failed 2, 1 and 1
checks respectively; the official fixes passed all checks. A separate final
grader restored the scoring test files and applied the official test patch.
Candidate diffs changed production source only. This targeted runner is not the
official SWE-bench Docker harness, and these three cases are all from one project.

The initial nine runs rotated mode order by case. Case 17630's Xhigh run hit an
incompatible legacy pytest entrypoint. Its entire three-mode block was repeated
later with the same explicit instruction to use the validated `run_tests` tool;
the table uses that complete follow-up block. The initial results remain recorded
separately. Each retained cell contains one run. Time includes classification,
main inference and tools, and excludes setup and the independent final grade.

| Retained three-case arm | Input tokens | Output tokens | EMP API-equivalent estimate |
| --- | ---: | ---: | ---: |
| Fixed Low | 117,097 | 2,562 | USD 1.2426 |
| Fixed Xhigh | 320,121 | 6,206 | USD 3.5115 |
| Automatic, including classification | 171,490 | 3,608 | USD 1.4837 |

Automatic selection chose Medium 12 times and High 9 times, with no fallback.
Its 21 classifier calls consumed 38,024 tokens and 58.486 seconds, averaging
2.785 seconds per inference. Selected effort is recorded separately from the
upstream response's root effort, which remained Xhigh while configuration
updates changed the requested effort in history. No generated patch failed the
grading tool before passing; bug reproductions and shell exits are recorded
separately and are not counted as failed repair cycles.

On these tasks, automatic mode took 27.4% less time than fixed Xhigh but 31.9%
more than fixed Low. It used fewer tool calls, but classification added waiting.
The pilot demonstrates per-inference selection, not a success-rate advantage
over Low or a general coding-performance ranking. Keep the feature off by
default. All 12 runs, including the excluded initial block, consumed 904,535
input and 16,978 output tokens; the EMP API-equivalent estimate is USD 9.0483,
not a cash charge to the subscription. No product code or live EMP setting was
changed for this stage.

## DeepSWE repository comparison (2026-10-29, 12:55–15:07 UTC)

This later experiment uses two tasks from
[DeepSWE](https://github.com/datacurve-ai/deep-swe/tree/0b9fabbb63b9104d678fe965e1632f2dd9eaa2ea),
each repeated three times per mode: **18 completed tasks**. It is separate from
the earlier HumanEval and SymPy pilots above. The main model is GPT-6-Astra on
one Native account; automatic mode uses GPT-6-Luna Low, with an Xhigh ceiling
and Xhigh as the configured fallback. Normal execution receipts report the
`default` tier. Mode order rotates across repetitions.

The tasks are `csstree-shorthand-expansion-compression` (implement CSS shorthand
expansion and compression, including layered backgrounds and custom syntax) and
`anko-typed-variable-bindings` (extend parsing, scopes and assignment-time type
checks). Their base commits are respectively
`88e3d965c0b1628642a30a841745b410d6835052` and
`3f269a72ff69398b1250c584171f32d12c0d8085`.

All modes use one custom Responses HTTP/SSE shell harness through an isolated
EMP candidate. This is not an official leaderboard run or a timing measurement
of the full Codex CLI harness. Workspaces have no network; Node 20.20.2 and Go
1.26.10 are pinned. Models see the original task and base repository, with
visible tests and their own added tests. The unmodified official verifier runs
afterward against a fresh base plus the generated patch; hidden tests and the
reference solution are never supplied to the model. Both pristine baselines
fail every new check while preserving old checks; both reference patches pass
everything. The frozen manifest and EMP binary remain unchanged throughout.

| Task | Mode | Fully passing tasks | New checks passed in repeats 1 / 2 / 3 | Mean task seconds | Mean seconds including final grading | Tool calls in repeats 1 / 2 / 3 |
| --- | --- | ---: | --- | ---: | ---: | --- |
| CSSTree, 79 new checks | Low | 0/3 | 74 / 73 / 74 | 127.9 | 134.2 | 7 / 6 / 8 |
| CSSTree, 79 new checks | Xhigh | 3/3 | 79 / 79 / 79 | 907.4 | 913.3 | 23 / 23 / 21 |
| CSSTree, 79 new checks | Luna automatic | 0/3 | 74 / 74 / 74 | 350.2 | 355.3 | 17 / 18 / 14 |
| Anko, 9 new test groups | Low | 0/3 | 5 / 5 / 5 | 111.0 | 123.2 | 9 / 9 / 8 |
| Anko, 9 new test groups | Xhigh | 2/3 | 5 / 9 / 9 | 754.2 | 767.2 | 28 / 21 / 22 |
| Anko, 9 new test groups | Luna automatic | 0/3 | 5 / 5 / 5 | 196.8 | 209.4 | 9 / 10 / 13 |

Task time includes classification, inference and tool execution, excluding
dependency preparation and final hidden grading. The same 60-tool and
90-minute task limits apply to every mode; all completed tasks end naturally.
Every attempt preserves all **16,715 CSSTree** or **94 Anko** old checks.
Generated patches change implementation, related tests and documentation, with
no dependency or verifier edits. Tool failures include exploratory command and
environment failures; they are not interchangeable with implementation defects.

The failed checks concentrate on specific contracts:

- CSSTree Low and automatic miss the canonical `background-size` initial string
  `auto auto`, producing the CSS-equivalent `auto`; five assertions cover this
  same difference and its round trips. Low repeat 2 also emits `list-style`
  components in a different order from the required canonical order.
- Anko failures return a non-nil value alongside a correct type-mismatch error.
  Four new groups enforce nil on these error paths. This is not a failure to
  reject the invalid assignment. Xhigh repeats 2 and 3 meet this contract.

Across the six completed automatic tasks, 87 classifications choose Medium 51
times, High 25 and Low 8. The other three decisions fall back to Xhigh: two
timeouts and one invalid selection. There is no deliberate Xhigh selection.
These classifications add **219.440 seconds and 165,630 known tokens**, about
**13.4%** of automatic task time. Automatic averages **273.5 seconds**, versus
**119.5 Low** and **830.8 Xhigh**: 2.29 times Low and 32.9% of Xhigh, with full
success **0/6, 0/6 and 5/6**, respectively for automatic, Low and Xhigh.

| Completed-task mode | Known input + output tokens | EMP API-equivalent estimate |
| --- | ---: | ---: |
| Low | 418,428 | USD 4.8561 |
| Xhigh | 4,942,501 | USD 46.1198 |
| Automatic, including classification | 1,274,501 | USD 11.3474 |

One Anko automatic attempt ends with upstream `server_is_overloaded` after
227.393 seconds and 11 tool calls. Its trace, patch and partial grade are retained;
only this interrupted trial is rerun from a pristine base. It is excluded from
the completed-task quality/time table and adds 108,454 known tokens and USD
1.0044 to the expenditure record. Across all 19 attempts, known usage is
**6,579,197 input + 164,687 output tokens**, including 973,824 cached input and
38,982 reasoning tokens within those respective totals; the known API-equivalent
estimate is **USD 63.3276**, not a subscription cash bill. Three timeout/overload
receipts lack complete usage and are not treated as zero. Early local catalog
preparation failures are recorded separately; one initial error receipt was
unavailable and is not assigned invented usage.

Keep automatic effort **default off**. Per-inference switching works, but this
classifier has not preserved Xhigh's completion quality or beaten Low's speed
on these two tasks. A faster classifier alone addresses only its measured wait;
the selection policy needs evaluation on separate tasks before a quality/speed
benefit is claimed. No product code, publication or live runtime changed during
this comparison.

## Later Auto policy experiments (2026-10-29 UTC)

Subsequent public-repository experiments compare Luna Low, Terra Low, Sol Low
and Jev classification with contemporary fixed controls. Category-based routing
does not consistently preserve completion quality. On Tengo, fixed Low passes
both repeats in 130/122 seconds; Jev takes 386/514 seconds with the same checks
passing. Full-history and cache-key probes do not establish a consistent
classifier benefit. These results remain separate from the earlier studies.

A later selective rule keeps Xhigh unless Jev proposes Low with a routine score
of at least 0.80. The threshold is fixed once, not calibrated accuracy. Four
complete development pairs on CSSTree and Anko, run at **21:18–21:51 UTC**, give:

| Metric across the four matched pairs | Fixed Xhigh | Selective Jev |
| --- | ---: | ---: |
| Tasks passing all new and original checks | 4/4 | 4/4 |
| Task time, including classification | 2,364.165 s | 2,246.958 s |
| Native input plus output tokens | 2,741,580 | 2,690,237 |
| Total tokens including classification | 2,741,580 | 3,052,729 |
| Native API-equivalent estimate | USD 26.636168 | USD 25.486074 |
| Additional Jev API-reported charge | — | USD 0.015110466 |

One scheduled CSS pair has a retained main-model 503 interruption. It is excluded
from this complete-pair comparison, not erased from expenditure or attempt
records. Its preregistered replacement runs two workers; scheduled pairs run
four. Quality failures are never excluded or selectively rerun.

These observations give 5.0% less time and 4.3% less Native API-equivalent cost,
with 11.3% more total tokens. Native estimates are not subscription cash bills;
cache reuse and stochastic trajectories also differ between arms. Most accepted
Low decisions occur during verification or completion. The two development tasks
do not establish general quality or savings, so keep Auto **default off** and
retain this rule in the experiment only. No hidden final auditor, discarded-output
retry or production routing change was added.

The marginal/selective receipt audit verifies 389 successful Native responses:
Standard execution tier, recorded usage and metadata matching selected effort.
The simplified shell-tool benchmark remains distinct from official CLI engine
acceptance. Source hashes, individual grades, failures and known/unknown costs
are retained in the dated local maintenance evidence.

## Current target and acceptance (2026-10-28 UTC)

The maintenance target is **0.161–0.165**, following the official stable
[0.165.0 release](https://github.com/openai/codex/releases/tag/rust-v0.165.0)
(2026-10-27 20:09:38 UTC). GitHub identifies it as the latest release with
`prerelease=false`.

| Target series | Existing evidence |
| --- | --- |
| 0.161–0.164 | Dated Linux engine acceptance below; App/IDE checks are listed separately with their exact engine versions. |
| 0.165 | Official Linux 0.165.0 engine passed the local EMP 0.13.17 candidate contracts recorded below. |

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
