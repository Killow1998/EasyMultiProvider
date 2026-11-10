# State lifecycle map

This is a source-level inventory of EMP's application lifecycles. It maps 71 lifecycle families and groups
related instances, rather than counting every boolean as a state machine.
It covers the nine Rust crates and the production WebUI. Test fixtures,
Shadow demonstrations, generated assets, and dependency internals are excluded.
The inventory is a navigation aid, not an exhaustive proof of reachable states.

**Decision:** keep state local to its owner. Use an enum for mutually exclusive
phases, preserve independent counters and flags, and derive presentation from
the owner's result. Do not add a global state-machine framework or a second
store that mirrors the same state.

The tables use conceptual phase names unless shown in code formatting.
**I** means implicit in flags, optionals, collections, timers, or control flow;
**E** means explicit state/phase values; **M** combines the two. Explicit does
not mean every transition is enforced by a type.

## Runtime and installation

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| R01 | [Lifecycle](../crates/emp-app/src/lifecycle/startup.rs) | Configuration and lock → listener and workers → serving → shutdown | M. Worker and listener ownership end with the server; not with a WebUI tab. |
| R02 | [Integration manager](../crates/emp-integration/src/manager.rs), [storage](../crates/emp-integration/src/storage.rs) | Native → apply → applied lease → restore; mismatched ownership → conflict | M. Configuration ownership and runtime liveness are separate. Restore is not Quit. |
| R03 | [Runtime service](../crates/emp-app/src/services/runtime.rs), [runtime record](../crates/emp-integration/src/runtime.rs) | Configuration change → pending verification → probe / synchronization result | M. A saved setting is not proof that an already-running Codex adopted it. |
| R04 | [Shutdown preparation](../crates/emp-app/src/services/shutdown.rs) | Quit → quiesce → restore owned settings → exit; refusal → keep serving | I. Commit to shutdown only after prerequisites succeed. |
| R05 | [Connections](../crates/emp-app/src/lifecycle/connections.rs), [admission](../crates/emp-transport/src/admission.rs) | Acquire permit → active connection/body → release; quiesce blocks new work | I. Resource counters and shutdown eligibility are orthogonal. Preserve RAII. |
| R06 | [Tray actions](../crates/emp-app/src/tray/actions.rs) | Idle → action in progress → integration / browser / quit result | I. Tray and WebUI invoke the same services; do not create tray-owned integration state. |
| R07 | [Update manager](../crates/emp-state/src/update/manager/operations.rs), [drain](../crates/emp-app/src/services/updates.rs) | Check → available → download → verify → wait for active work → install; error → retry | M. Download progress, release cache, active-work count and drain state have different lifetimes. |
| R08 | [Update worker](../crates/emp-state/src/update/worker.rs) | `waiting_for_exit` → `replacing` → `starting` → `waiting_for_startup` → `complete`; failure → restore / rollback / recovery | E. Persisted phases survive process replacement. Keep separate from the WebUI polling lifecycle. |
| R09 | [Startup recovery](../crates/emp-app/src/startup_recovery.rs), [reset transaction](../crates/emp-app/src/startup_recovery/reset.rs) | Unreadable configuration → recovery UI → staged rebuild → install / rollback; interrupted rebuild → resume | M. Staging files and recovery markers are transaction state, not disposable UI flags. |

## Identity, accounts and route selection

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| I01 | [Web session](../crates/emp-state/src/web_session.rs), [gateway key](../crates/emp-state/src/gateway_key.rs) | Bootstrap claim → management session; gateway credential → request authorization | I. Management authorization, gateway access and upstream login are distinct identities. |
| I02 | [Login credentials](../crates/emp-app/src/services/native_login/credentials.rs), [login service](../crates/emp-app/src/services/native_login.rs) | Subscription / user API key / EMP Local Key → explicit replacement → configuration update → runtime restart required | M. Keep/remove Other is an explicit action; changing login must not erase usage identity. |
| I03 | [Login session](../crates/emp-app/src/services/native_login.rs) | Idle → Pending → Completed / Failed / Cancelled; start again → Pending | E after this audit. Native and Other each own a session. Repeated status reads do not start login. |
| I04 | [Official login helper](../crates/emp-codex/src/quota/process/subscription_login.rs) | Start child → publish browser/device challenge → collect result → cancel/drop child | M. Helper completion precedes credential installation; the service owns installation. |
| I05 | [Accounts](../crates/emp-app/src/services/accounts.rs), [quota commands](../crates/emp-app/src/services/quota/commands.rs) | Import / refresh / delete → identity and credential checks → persist → invalidate affected observations | I. Background results belong to the account that initiated them. |
| I06 | [Catalog refresh queue](../crates/emp-app/src/services/account_catalog/refresh_state.rs) | Request → pending → running → publication → sleep; source change queues another pass | I. `pending` and `running` may both be true. Do not collapse them into one exclusive phase. |
| I07 | [Catalog poll state](../crates/emp-app/src/services/account_catalog/refresh_state.rs) | Missing / expired → poll → fresh; failure → backoff; identity generation change → invalidate | I. Per-account freshness is separate from the global worker queue. |
| I08 | [Auto Review feedback](../crates/emp-app/src/services/auto_review/state.rs) | Eligible → unavailable result → cooldown → expiry or success → eligible | I. Feedback owns routing eligibility; telemetry does not. |
| I09 | [Aggregate routing](../crates/emp-app/src/services/aggregate_routing.rs), [affinity ledger](../crates/emp-state/src/usage/ledger/affinity.rs) | Unbound conversation → select source → sticky account → unavailable → select replacement | I. Source policy applies at assignment; ordinary turns retain affinity. |
| I10 | [Image routing](../crates/emp-app/src/services/native/images/routing.rs), [affinity](../crates/emp-app/src/services/native/images/routing/affinity.rs) | Select image account → bind turn → reuse / reject unavailable destination | I. The account supplying image quota is not necessarily the Runtime's login account. |

## Migration

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| M01 | [Preview registry](../crates/emp-app/src/services/migration/preview.rs) | Decode → session-bound preview token → select → consume or expire | I. Preview does not publish credentials. File and LAN import converge here. |
| M02 | [Migration service](../crates/emp-app/src/services/migration.rs) | Validate selection/conflicts → commit selected records → update integration/catalog → result | I. Committing is a mutation; closing the preview is not permission to replay it. |
| M03 | [LAN sender](../crates/emp-app/src/services/migration/lan.rs), [wire](../crates/emp-app/src/services/migration/lan/wire.rs) | Waiting → receiver bound / confirmation → approval → sending → sent / failed; deadline → expired; cancel → drop sender | M. `approved` is an internal distinction while public status remains `confirmation`. Bundle ownership is consumed once. Keep this mapping documented. |
| M04 | [LAN receive operation](../crates/emp-app/src/services/migration/lan/wire.rs) | Present proof and nonce → pending approval → receive bytes → preview; rejection/expiry → error | M. Pending approval is not a failed password. Receiver identity persists across polls. |

## Requests, transports and model execution

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| Q01 | [Request observation](../crates/emp-app/src/services/observation/request.rs) | Authenticate → read → route → validate → prepare history/destination → execute → downstream receipt | E. Diagnostic phases describe execution; they must not become retry policy. |
| Q02 | [Request outcome](../crates/emp-app/src/services/request_outcome.rs) | Attempt starts → first output / usage observations → finalize or interrupted drop | I. Upstream completion, downstream delivery and billing are not the same terminal event. |
| Q03 | [Activity](../crates/emp-app/src/services/activity.rs) | Request / upstream work begins → counts active → finish/drop decrements | I. Preserve guard-owned cleanup rather than mirroring it in frontend flags. |
| Q04 | [Native HTTP](../crates/emp-router/src/native_http.rs), [failure policy](../crates/emp-transport/src/failure.rs) | Send → retryable pre-output failure → bounded retry; output started → stream → terminal/failure | M. Do not replay generated output or tool activity as if nothing happened. |
| Q05 | [Native WebSocket](../crates/emp-app/src/api/websocket/native_socket.rs), [cooldown](../crates/emp-app/src/services/native_connections.rs) | Connect → reuse continuation → transport failure → HTTP fallback / cooldown → future WebSocket eligibility | I. Connection reuse, response identity and fallback eligibility are separate state axes. |
| Q06 | [WebSocket control](../crates/emp-app/src/api/websocket/control.rs) | Response created → delivering → terminal; interruption → stop delivery → emit interruption once; next request retained | I. A completed upstream response does not imply interruption was delivered downstream. |
| Q07 | [Disconnect monitor](../crates/emp-app/src/services/disconnect.rs) | Probe connection → execute upstream → disconnect/cancel → terminate owner and join worker | M. Keep cancellation at the I/O owner; adapters do not independently restart work. |
| Q08 | [WebSocket transport](../crates/emp-transport/src/websocket), [pump](../crates/emp-transport/src/websocket_pump) | Handshake → partial frame → complete message → acknowledge write → close/error | M. Fragmentation, compression and connection close are transport state, not route state. |
| Q09 | [Response events](../crates/emp-router/src/response_events.rs), [Chat stream](../crates/emp-protocol/src/chat_stream.rs), [Anthropic stream](../crates/emp-protocol/src/anthropic_projection/stream.rs) | Created → text/reasoning/tool blocks and deltas → item done → terminal → exhausted | M. Preserve protocol-specific state. A shared enum would hide real protocol differences. |
| Q10 | [Tool catalog](../crates/emp-protocol/src/tool_bridge/catalog.rs) | Declare → replace / remove → redeclare while folding history in order | I. Definitions change over time; historical calls/results retain identity. |
| Q11 | [Claude process](../crates/emp-app/src/services/claude_cli/process.rs), [cancellation](../crates/emp-app/src/services/claude_cli/cancellation.rs) | Start → stream/observe → budget, cancel, exit or output result → reap | M. Child lifetime and output validity are separate responsibilities. |
| Q12 | [Claude TUI turn pool](../crates/emp-app/src/services/claude_cli/tui.rs), [session](../crates/emp-app/src/services/claude_cli/tui/session.rs) | New user turn → canonical trusted workspace → MCP command ready → submit context → stream public CPA text → validated tool dispatch → await Codex results → resume → complete/drop | M. Codex owns canonical history; temporary tool round trips do not create a second history authority. |
| Q13 | [Image MCP](../crates/emp-app/src/cli/image_mcp.rs) | Initialize → tool request worker → result; cancellation/EOF → terminate pending work | I. Request IDs own work; ping remains independent of generation. |
| Q14 | [Realtime](../crates/emp-app/src/api/realtime/mod.rs), [sideband](../crates/emp-app/src/api/realtime/sideband.rs) | Create call → sideband relay → close/error | I. Keep call resource lifetime distinct from ordinary Responses turns. |
| Q15 | [SSE parser](../crates/emp-transport/src/lib.rs) | Partial bytes → lines → event boundary → JSON frame / error / finish | I. Parser buffering is independent of Q09 response completion. |

## History, parsing and accounting

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| H01 | [History scan](../crates/emp-codex/src/history/scan.rs), [replay](../crates/emp-codex/src/history/replay.rs) | Read records → identify turn/window/ordinal boundaries → materialize selected prefix or fail | I. Do not confuse complete file traversal with complete recoverable history. |
| H02 | [History preparation](../crates/emp-history/src/preparation.rs), [tool pairs](../crates/emp-history/src/tool_pairs.rs) | Resolve anchor → reconstruct visible context → pair tool calls/results → destination-ready input | I. Preparation consumes Codex history; it does not own a replacement transcript. |
| H03 | [Compaction](../crates/emp-history/src/context/compaction.rs), [service](../crates/emp-app/src/services/compaction.rs) | Budget context → choose complete units → compact → checkpoint / error | I. Context limits and legal tool-pair boundaries both constrain transition. |
| H04 | [History repair transaction](../crates/emp-codex/src/history_repair/transaction.rs), [selection](../crates/emp-codex/src/history_repair/selection.rs) | Inspect → select → stage → prepare → commit / preserve original; unavailable writer → skip | M. File locks and staged records protect existing user history, not theoretical duplicate checks. |
| H05 | [Collaboration recovery](../crates/emp-app/src/services/collaboration/state.rs) | Observe provenance → recover on demand → cache scoped plaintext → expire/evict | I. Origin retention and recovered-result retention intentionally differ. |
| H06 | [Usage scanner](../crates/emp-codex/src/usage_history.rs), [usage service](../crates/emp-app/src/services/usage.rs) | Idle → scan/index progress → persist cursor/result → idle; stop → end worker | M. Scan progress, available old results and pricing completeness are independent. |
| H07 | [Model observations](../crates/emp-router/src/model_observation.rs) | Observe headers / first event → collect model, effort and timing → reconcile terminal metadata | I. Conflicting observations and missing observations are different results. |
| H08 | [Chat text](../crates/emp-protocol/src/chat_text.rs) | Accumulate marker prefix / backticks → emit text, enter/leave code span, or reject tool markup | I. Stream chunk boundaries must not become user-visible content boundaries; code fences are parser state. |

## Background publication and caches

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| B01 | [Management events](../crates/emp-app/src/services/management_events.rs) | Revision change → pending snapshot → subscribers observe → close | I. Revisions signal invalidation; subscribers read authoritative state. |
| B02 | [System proxy cache](../crates/emp-transport/src/system_proxy/cache.rs) | Cold → reading → fresh → expired; read failure retains prior valid snapshot | I. A read in progress and a usable cached value can coexist. |
| B03 | [Provider balance](../crates/emp-app/src/services/provider_balance.rs) | Supported provider → fetch → balance or mapped error → refresh | M. Unsupported, unknown and zero balance are distinct. |
| B04 | [CPA quota](../crates/emp-app/src/services/cpa_quota.rs), [history](../crates/emp-app/src/services/cpa_quota/history.rs) | Discover support → fetch pool/account observation → persist trend; stale/error → subsequent refresh | M. Unsupported quota APIs must not create usable-looking meters. |
| B05 | [Pricing ledger](../crates/emp-state/src/usage/ledger.rs) | Unpriced record → price available → calculate and store; stop → leave pending records | I. Missing price does not mean zero cost. |
| B06 | [Release cache](../crates/emp-state/src/update/manager/catalog.rs) | Missing/stale → request or conditional request → retain/update notes; failure → keep usable cache | I. Cached notes and a pending update check can coexist. |
| B07 | [Local Claude quota](../crates/emp-app/src/services/claude_cli/quota.rs) | Identify login → begin observation generation → accept matching usage → persist; owner change → reset current observation | I. Late results cannot become the new login's quota. |
| B08 | [Diagnostic journal](../crates/emp-state/src/diagnostics/journal.rs) | Open part → append sanitized records → rotate/prune → continue | I. Bounded passive evidence storage; it never authorizes request retries. |
| B09 | [Report fact cache](../crates/emp-state/src/usage/ledger/read_cache.rs) | Empty → load timestamped facts → reuse or extend range; database version changes → rebuild; reader disconnect → interrupt/rollback | I. One disk-backed window per projection; aggregates and UI filters do not own cache state. The connection guard removes the cancellation hook before reuse. |

## WebUI lifecycles

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| U01 | [Management client](../crates/emp-app/web/management-client.js), [page](../crates/emp-app/web/index.html) | Bootstrap → signed in → fetch; session rejection → clear; EMP exit → stopped | I. API caller authentication failure is not necessarily an upstream provider failure. |
| U02 | [Management event client](../crates/emp-app/web/management-events.js) | Start → await leadership / follow → connect → stream → backoff; stop → abort and release | I. `isRunning` means EMP has not exited, **not document visibility**. Hidden pages still receive shutdown. |
| U03 | [Modal host](../crates/emp-app/web/index.html), [interactions](../crates/emp-app/web/interactions.js) | Closed → open child → back to parent / close; focus and pointer events update interaction | I. Back restores the immediate parent; late responses must not mutate a replacement dialog. |
| U04 | [Model manager](../crates/emp-app/web/model-manager.js), [editor](../crates/emp-app/web/model-editor.js) | Open → load → edit draft → save; route/dialog replacement → abort/ignore old read | I. Draft ownership is local; saved configuration is authoritative. |
| U05 | [Native login UI](../crates/emp-app/web/native-login.js), [account import](../crates/emp-app/web/account-import.js) | Choose destination/method → start → poll → complete/fail/cancel → retry/back | M. UI polls I03/I04; it must not infer credential installation from merely seeing a device code. |
| U06 | [Migration UI](../crates/emp-app/web/migration.js) | Choose mode → draft/file/password → preview → confirm → commit; back restores draft | I. Abortable preview and non-abortable commit have deliberately different lifetimes. |
| U07 | [LAN migration UI](../crates/emp-app/web/migration-lan.js) | Send setup → code/approval/result; receive code → poll → preview/confirm | M. Send and receive are independent workflows. Derive controls from their own operation state. |
| U08 | [Updates UI](../crates/emp-app/web/updates.js) | Cached notes → check/poll → install → reconnect → updated page or timeout/rollback | M. Request generation protects the current dialog. Install disconnect is not an immediate failure. |
| U09 | [Report query](../crates/emp-app/web/report-query.js), [statistics](../crates/emp-app/web/statistics.js), [bills](../crates/emp-app/web/session-bills.js), [calls](../crates/emp-app/web/call-reports.js) | Filter/query → loading/progress → result/error; new query → abort and supersede | I. Shared query ownership already solves stale reads without a global store. |
| U10 | [Account details](../crates/emp-app/web/account-details.js), [usage report](../crates/emp-app/web/usage-report.js) | Open/read cached result → background query → render matching owner → save/remove/close | I. Cached data may remain useful during refresh. Do not equate loading with no data. |
| U11 | [CPA quota UI](../crates/emp-app/web/cpa-quota.js), [provider balance UI](../crates/emp-app/web/provider-balances.js), [trend](../crates/emp-app/web/quota-history.js) | Schedule/manual refresh → per-provider busy → matching result → idle; visibility affects polling | I. Here document visibility really does control polling; it is not the U02 condition. |
| U12 | [Notifications](../crates/emp-app/web/notifications.js), [service errors](../crates/emp-app/web/service-errors.js) | Show → count down / pause while reading → dismiss; inbox unread → read/clear | I. Animation time, unread history and current notification are different state axes. |
| U13 | [App icon](../crates/emp-app/web/app-icon.js), [motion](../crates/emp-app/web/motion.js) | Preview → save/cancel; visible active animation → paused/hidden/reduced motion | I. Preview and persistent choice remain separate; no new UI motion is needed. |
| U14 | [Tutorials](../crates/emp-app/web/tutorials.js) | Select tutorial → load/show animation → select another / close / open larger view | I. Selection and resource lifetime belong to the tutorial view. |
| U15 | [Settings](../crates/emp-app/web/settings.js), [routing](../crates/emp-app/web/routing-settings.js), [aggregate](../crates/emp-app/web/aggregate-settings.js), [decision](../crates/emp-app/web/decision-settings.js), [connections](../crates/emp-app/web/service-connections.js) | Open draft → edit → validate → save → publish result or retain draft | I. Settings values are configuration, not independent lifecycle phases. Keep per-form save ownership. |
| U16 | [Recovery UI](../crates/emp-app/web/recovery.html) | Load recovery state → ready → confirm → busy → finished / retryable error | I. Confirmation and terminal restart/exit are distinct; no normal management session is assumed. |

## Changes selected in this audit

1. **I03:** replace `pending: Option<Pending>` plus nullable JSON `outcome` with
   `LoginState::{Idle, Pending, Completed, Failed, Cancelled}`. A session cannot
   simultaneously retain a pending helper and a finished result. Preserve wire
   status names, helper teardown, Native/Other separation and credential writes.
2. **U02:** rename the injected `visible` predicate to `isRunning` in production
   and its test caller. Its actual argument is `!management.isStopped()`. A
   document-visibility interpretation would incorrectly suppress shutdown
   delivery to hidden tabs. No connection behavior changes.
3. Extend the existing login browser journey with helper failure followed by
   retry. It now covers cancellation and failure preserving Native credentials,
   successful replacement, Other import, and file/paste deduplication.

The absence of a framework is intentional. R02/R08/H04 cross durable transaction
boundaries; Q09 contains different upstream protocols; I06/B02/U12 contain
independent state axes. Converting all of them to one giant enum would increase
coupling and create migration obligations without changing a proven behavior.

## Observable contracts for humans and agents

- **UX:** show the owner's actual phase and next valid action. Keep pending
  operations distinct from failure, zero, unsupported and completed states.
  Navigation must preserve drafts or finish an explicitly submitted mutation.
- **DX:** find the owning module in the tables before editing state. New mutually
  exclusive phases belong in that owner. Independent flags must explain their
  distinct lifetime; do not derive business state from logs or UI labels.
- **AX:** use status/result endpoints to observe completion. A returned task/code
  is not a completed mutation. Poll the existing operation rather than repeating
  `start`, import, install or model execution after a transport interruption.
  Honor terminal status and cancellation at the owning process/connection.

## Verification entry points

These are existing regression entry points, not a claim they were all rerun.
Use the group affected by a change; do not rebuild every platform for a naming
or local-state refactor.

| Area | Existing behavior checks |
|---|---|
| Login / identity | [Browser login destination journey](../scripts/shadow_frontend/e2e_login_destination.py), [Local Key journey](../scripts/shadow_frontend/e2e_native_local_key.py), native_login Rust round-trip test |
| UI request/navigation ownership | [Lifecycle contracts](../crates/emp-app/tests/web_lifecycle_contract.cjs), [navigation E2E](../scripts/shadow_frontend/e2e_navigation.py), [completion E2E](../scripts/shadow_frontend/e2e_modal_completion.py) |
| Shutdown / shared event connection | [Desktop tray E2E](../scripts/shadow_frontend/e2e_desktop_tray.py), [shutdown tests](../crates/emp-app/src/lifecycle/shutdown/tests.rs) |
| Migration | [Navigation E2E](../scripts/shadow_frontend/e2e_migration_navigation.py), [LAN wire tests](../crates/emp-app/src/services/migration/lan/wire/tests.rs) |
| Updates / recovery | [Update E2E](../scripts/shadow_frontend/e2e_updates.py), [cache E2E](../scripts/shadow_frontend/e2e_update_cache.py), [startup recovery E2E](../scripts/shadow_frontend/e2e_startup_recovery.py) |
| Responses / cancellation / image tools | [Native API contracts](../crates/emp-app/src/tests/native_api_contract), [MCP verification](../scripts/verify_image_mcp.py) |
| History / protocols / accounts | Tests in `emp-history`, `emp-protocol`, `emp-codex`; [aggregate contract](../crates/emp-app/src/tests/aggregate_contract.rs) |

For this audit, the affected checks are the Rust login round trip, the twelve
WebUI lifecycle contracts, the six browser login journeys (synthetic accounts,
no model calls), candidate build, formatting and Clippy. This is not new
Windows/macOS or real-provider acceptance.
