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
| R02 | [Integration manager](../crates/emp-codex/src/configuration/manager.rs), [storage](../crates/emp-codex/src/configuration/storage.rs) | Native → apply → applied lease → restore; mismatched ownership → conflict | M. Configuration ownership and runtime liveness are separate. Restore is not Quit. |
| R03 | [Runtime service](../crates/emp-app/src/services/runtime.rs), [runtime record](../crates/emp-codex/src/configuration/runtime.rs) | Configuration change → pending verification → probe / synchronization result | M. A saved setting is not proof that an already-running Codex adopted it. |
| R04 | [Shutdown preparation](../crates/emp-app/src/services/shutdown.rs) | Quit → quiesce → restore owned settings → exit; refusal → keep serving | I. Commit to shutdown only after prerequisites succeed. |
| R05 | [Connections](../crates/emp-app/src/server/connection.rs), [admission](../crates/emp-app/src/services/connection_admission.rs) | Acquire permit → active connection/body → release; quiesce blocks new work | I. Capacity and business occupancy have separate lifetimes. |
| R06 | [Tray actions](../crates/emp-app/src/tray/actions.rs) | Idle → action in progress → integration / browser / quit result | I. Tray and WebUI invoke the same services; do not create tray-owned integration state. |
| R07 | [Update manager](../crates/emp-updater/src/manager/operations.rs), [drain](../crates/emp-app/src/services/updates.rs) | Check → available → download → verify → wait for active work → install; error → retry | M. Download progress, release cache, active-work count and drain state have different lifetimes. |
| R08 | [Update worker](../crates/emp-updater/src/worker.rs) | `waiting_for_exit` → `replacing` → `starting` → `waiting_for_startup` → `complete`; failure → restore / rollback / recovery | E. Persisted phases survive process replacement. Keep separate from the WebUI polling lifecycle. |
| R09 | [Startup recovery](../crates/emp-app/src/startup_recovery.rs), [reset transaction](../crates/emp-app/src/startup_recovery/reset.rs) | Unreadable configuration → recovery UI → staged rebuild → install / rollback; interrupted rebuild → resume | M. Staging files and recovery markers are transaction state, not disposable UI flags. |

Shutdown waiters register with `Notify` before checking the shutdown flag; the
quota sampler checks its condition and receives shutdown notification under the
same mutex. `ExitSignals` retains the runtime until its callback task is joined
and keeps signal receivers registered throughout shutdown.

### 退出排空模型

`ServerControl::stop` 首次收到退出信号时关闭准入、更新入口和管理事件源，并确定排空期限。`/api/quit` 先检查业务占用、恢复集成配置并完成退出回执的写入尝试，然后发信号。SIGTERM、控制台事件和更新交接通过同一控制入口发信号，恢复页在退出或重置回执写完后发信号。监听器 `serve` 独自判定排空结果，生命周期读取该结果并完成各自的配置、凭据与服务锁收尾。

准入为每个请求创建一个 `BusinessToken`。令牌只能移动，由当前处理者独占，处理取消时由析构释放。连接任务把令牌交给请求处理器，之后持有 `ConnectionSlot` 统计容量。中间的阻塞步骤接过令牌，返回时把它交回处理器。最终输出由工作线程生成时，线程接过令牌并交给输出端消耗。容量槽继续保留到连接收尾。

| 占用者 | 占用区间 |
| --- | --- |
| 普通请求 | 从准入到交付完整缓冲响应，或处理终止。 |
| 推理流 | 从准入到入队最后的 SSE 事件，或流处理终止。原样转发且无法识别终帧的正文在转发结束时释放。 |
| 报表流 | 从准入到入队 `result` 或 `error`，或查询终止。流建立前的提前返回按缓冲响应交付。 |
| Aero 委托 | 回调在委托键仍有效时登记自己的令牌，与父请求共用容量槽和更新许可。父请求交付错误响应后，仍在推理的回调继续占用业务。 |
| 已提交的阻塞工作 | 持有处理器移交的令牌，即使等待结果的处理器先结束。传输结算仍等待已提交的工作。 |
| WebSocket | 从预留或接管 WebSocket 名额到会话退出。 |
| 管理事件订阅 | 释放普通请求槽后占用独立订阅名额。排空开始即关闭事件源、唤醒订阅，并交付待发的退出事件。 |

最终输出入队即完成业务交接。SSE 终帧先预留队列位置，缓冲响应先构造整包。交接在业务计数锁内消耗令牌，在归零时记时，并在同一临界区发布输出。写入确认和工作线程收尾继续进行。业务归零会唤醒所有已登记的等待者。等待者先登记通知，再检查两个池的业务计数。`/api/quit` 的预检查和期限快照读取同一份业务计数，预检查豁免退出请求自身。

普通请求和 WebSocket 各自在更新业务计数的同一把锁内记录最近一次归零的单调时刻。监听器判定时，任一池仍有业务，或最近一次归零晚于截止时刻，退出结果就是 `ServerStopped`。业务恰好在截止时刻归零时可以正常退出。监听器何时获调度、发送和收尾用了多久，都不改变这个判定。

排空时钟封装在 `server` 内部。生产时钟从排空开始计时，期限为 30 秒。测试可以独立推进单调时刻和触发截止事件。到期判定后，监听器取消仍在进行的传输，按实际写入结果结算并等待已提交的工作。取消使用接纳时已拥有的套接字，唤醒挂起的读写和探测，连接准入只需要一个描述符。

Normal exit restores owned native routing while the listener drains admitted
handlers, then joins workers and flushes rotated credentials before releasing
the service lock. An unsaved rotation makes shutdown fail. Only the
updater's own handoff preserves integration, while update and rollback retain
the current browser session and tab.

Recovery holds `state/service.lock`, rejects configuration symlinks/reparse
points and keeps the lock inode in place during file moves. A missing key with
existing credentials opens recovery; external-key overrides are handled by
their owner rather than local-state reset.

Recovery writes its pending marker before moving files, reuses the transaction
archive with separate attempt directories after interruption, and resumes
recovery on partial state. Cleanup removes only the generated transaction
directory after commit, without following contained symlinks; the recovery
listener also leaves an occupied service port with its current owner.
Recovery actions have no request body and reject ambiguous framing that could
leave bytes to be interpreted as another command.

Desktop browser children may outlive EMP; a separate waiter reaps them after
the launch deadline. With an explicit CLI command, Windows attaches to the
parent console and preserves inherited redirected handles.

Configuration recovery attempts both optional preference journals even if one
conflicts or fails; comparisons ignore whitespace-only decoration while preserving
comments, including leading comments when the first managed field is removed.
Integration leases accept versions 2, 3 and 4; writes use version 4 for an active
image-MCP override and otherwise version 3 for older EMP readers.

## Identity, accounts and route selection

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| I01 | [Web session](../crates/emp-storage/src/web_session.rs), [gateway key](../crates/emp-storage/src/gateway_key.rs) | Bootstrap claim → management session; gateway credential → request authorization | I. Management authorization, gateway access and upstream login are distinct identities. |
| I02 | [Login credentials](../crates/emp-app/src/services/native_login/credentials.rs), [login service](../crates/emp-app/src/services/native_login.rs) | Subscription / user API key / EMP Local Key → explicit replacement → configuration update → runtime restart required | M. Keep/remove Other is an explicit action; changing login must not erase usage identity. |
| I03 | [Login session](../crates/emp-app/src/services/native_login.rs) | Idle → Pending → Completed / Failed / Cancelled; start again → Pending | E after this audit. Native and Other each own a session. Repeated status reads do not start login. |
| I04 | [Official login helper](../crates/emp-codex/src/quota/process/subscription_login.rs) | Start child → publish browser/device challenge → collect result → cancel/drop child | M. Helper completion precedes credential installation; the service owns installation. |
| I05 | [Accounts](../crates/emp-app/src/services/accounts.rs), [quota commands](../crates/emp-app/src/services/quota/commands.rs) | Import / refresh / delete → identity and credential checks → persist → invalidate affected observations | I. Background results belong to the account that initiated them. |
| I06 | [Catalog refresh queue](../crates/emp-app/src/services/account_catalog/refresh_state.rs) | Request → pending → running → publication → sleep; source change queues another pass | I. `pending` and `running` may both be true. Do not collapse them into one exclusive phase. |
| I07 | [Catalog poll state](../crates/emp-app/src/services/account_catalog/refresh_state.rs) | Missing / expired → poll → fresh; failure → backoff; identity generation change → invalidate | I. Per-account freshness is separate from the global worker queue. |
| I08 | [Auto Review feedback](../crates/emp-app/src/services/auto_review/state.rs) | Eligible → unavailable result → cooldown → expiry or success → eligible | I. Feedback owns routing eligibility; telemetry does not. |
| I09 | [Aggregate routing](../crates/emp-app/src/services/aggregate_routing.rs), [affinity ledger](../crates/emp-storage/src/usage/ledger/affinity.rs) | Unbound conversation → select source → sticky account → unavailable → select replacement | I. Source policy applies at assignment; ordinary turns retain affinity. |
| I10 | [Image routing](../crates/emp-app/src/services/native/images/routing.rs), [affinity](../crates/emp-app/src/services/native/images/routing/affinity.rs) | Select image account → bind turn → reuse / reject unavailable destination | I. The account supplying image quota is not necessarily the Runtime's login account. |

Session rotation persists the replacement before discarding the current
in-memory session, retaining existing logins if persistence fails. Gateway
credentials authorize forwarded API clients, while browser management requires
its origin-scoped session; credential export consumes a confirmation issued
by a separate request.
Tray Reopen issues a fresh single-use link without persisting another
credential or making the initial ten-minute startup link reusable.

Quota cleanup finishes the helper and terminates its children even after a
successful launcher exit, then signals pipe readers to stop before joining them
so detached descendants cannot hold shutdown or refresh locks indefinitely.
Windows helpers start suspended, join the owned job before running, and resume
their sole initial thread through a thread snapshot because `std::process::Child`
retains the process handle rather than the initial thread handle.

## Migration

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| M01 | [Preview registry](../crates/emp-app/src/services/migration/preview.rs) | Decode → session-bound preview token → select → consume or expire | I. Preview does not publish credentials. File and LAN import converge here. |
| M02 | [Migration service](../crates/emp-app/src/services/migration.rs) | Validate selection/conflicts → commit selected records → update integration/catalog → result | I. Committing is a mutation; closing the preview is not permission to replay it. |
| M03 | [LAN sender](../crates/emp-app/src/services/migration/lan.rs), [wire](../crates/emp-app/src/services/migration/lan/wire.rs) | Waiting → receiver bound / confirmation → approval → sending → sent / failed; deadline → expired; cancel → drop sender | M. `approved` is an internal distinction while public status remains `confirmation`. Bundle ownership is consumed once. Keep this mapping documented. |
| M04 | [LAN receive operation](../crates/emp-app/src/services/migration/lan/wire.rs) | Present proof and nonce → pending approval → receive bytes → preview; rejection/expiry → error | M. Pending approval is not a failed password. Receiver identity persists across polls. |

### Durable credential formats

Vault files retain `easy-multi-provider-v1\n` followed by Fernet ciphertext.
Migration bundles retain `EMP-MIGRATION\x01\n`, the
`easy-multi-provider-migration` envelope and a 16-byte salt. Imports accept v1
with fixed scrypt `N=16384,r=8,p=1`; current exports use v2 with
`N=131072,r=8,p=1`. The decrypted payload stays at version 1. Each envelope's
KDF parameters must match its version; bundle input cannot choose the cost.
Credentials are re-encrypted under the destination vault on import. Private
files/directories retain Unix 0600/0700 or the restricted Windows ACL/owner
rules, including symlink/reparse-point checks and transactional rollback.
Transaction snapshots revalidate the opened file's metadata after checking its
path, rejecting substituted links or special files before reading their contents.
After saving the new configuration, deletion of deactivated credential files
is best-effort. A deletion failure leaves the save successful.

## Requests, transports and model execution

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| Q01 | [Request observation](../crates/emp-app/src/services/observation/request.rs) | Authenticate → read → route → validate → prepare history/destination → execute → downstream receipt | E. Diagnostic phases describe execution; they must not become retry policy. |
| Q02 | [Request outcome](../crates/emp-app/src/services/request_outcome.rs) | Attempt starts → first output / usage observations → finalize or interrupted drop | I. Upstream completion, downstream delivery and billing are not the same terminal event. |
| Q03 | [Activity](../crates/emp-app/src/services/activity.rs) | Request / upstream work begins → counts active → finish/drop decrements | I. Preserve guard-owned cleanup rather than mirroring it in frontend flags. |
| Q04 | [Native HTTP](../crates/emp-router/src/native_http.rs), [failure policy](../crates/emp-router/src/failure_policy.rs) | Send → retryable pre-output failure → bounded retry; output started → stream → terminal/failure | M. Do not replay generated output or tool activity as if nothing happened. |
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

The management SSE connection releases its integration admission permit before
entering the stream, so it cannot block native restoration. Realtime sideband
shutdown joins the socket owner before inspecting the final confirmation
receipt, preserving the distinction between queued and delivered writes.

## History, parsing and accounting

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| H01 | [History scan](../crates/emp-codex/src/sessions/read/scan.rs), [replay](../crates/emp-codex/src/sessions/read/replay.rs) | Read records → identify turn/window/ordinal boundaries → materialize selected prefix or fail | I. Do not confuse complete file traversal with complete recoverable history. |
| H02 | [History preparation](../crates/emp-context/src/preparation.rs), [tool pairs](../crates/emp-context/src/tool_pairs.rs) | Resolve anchor → reconstruct visible context → pair tool calls/results → destination-ready input | I. Preparation consumes Codex history; it does not own a replacement transcript. |
| H03 | [Compaction](../crates/emp-context/src/context/compaction.rs), [service](../crates/emp-app/src/services/compaction.rs) | Budget context → choose complete units → compact → checkpoint / error | I. Context limits and legal tool-pair boundaries both constrain transition. |
| H04 | [History repair transaction](../crates/emp-codex/src/sessions/repair/transaction.rs), [selection](../crates/emp-codex/src/sessions/repair/selection.rs) | Inspect → select → stage → prepare → commit / preserve original; unavailable writer → skip | M. File locks and staged records protect existing user history, not theoretical duplicate checks. |
| H05 | [Collaboration recovery](../crates/emp-app/src/services/collaboration/state.rs) | Observe provenance → recover on demand → cache scoped plaintext → expire/evict | I. Origin retention and recovered-result retention intentionally differ. |
| H06 | [Usage scanner](../crates/emp-codex/src/usage_history.rs), [usage service](../crates/emp-app/src/services/usage.rs) | Idle → scan/index progress → persist cursor/result → idle; stop → end worker | M. Scan progress, available old results and pricing completeness are independent. |
| H07 | [Model observations](../crates/emp-router/src/model_observation.rs) | Observe headers / first event → collect model, effort and timing → reconcile terminal metadata | I. Conflicting observations and missing observations are different results. |
| H08 | [Chat text](../crates/emp-protocol/src/chat_text.rs) | Accumulate marker prefix / backticks → emit text, enter/leave code span, or reject tool markup | I. Stream chunk boundaries must not become user-visible content boundaries; code fences are parser state. |

Paginated history replays ancestors oldest first within frozen byte and ordinal
bounds; only the newest eligible compaction can seed reverse replay, which
preserves prefix control/model state, checks ordinals through the anchor, and
uses full replay for opaque checkpoints, missing anchors or invalid seed/suffix
state, while legacy non-paginated histories may use visible summaries.
Rollback precedes failed-turn filtering and clears the inherited active turn;
exact checkpoint lookup and replay share fork bounds, and later segments check
checkpoint ambiguity while the client supplies the post-checkpoint tail.

History repair keeps replacement arrays empty in checkpoints and writes the
canonical response items immediately afterward to bound checkpoint size.
Windows publication uses `MOVEFILE_WRITE_THROUGH`; its directory-sync operation
has no separate flush.

## Background publication and caches

| ID | Owner | Phases / transition triggers | Representation and boundary |
|---|---|---|---|
| B01 | [Management events](../crates/emp-app/src/services/management_events.rs) | Revision change → pending snapshot → subscribers observe → close | I. Revisions signal invalidation; subscribers read authoritative state. |
| B02 | [System proxy cache](../crates/emp-transport/src/system_proxy/cache.rs) | Cold → reading → fresh → expired; read failure retains prior valid snapshot | I. A read in progress and a usable cached value can coexist. |
| B03 | [Provider balance](../crates/emp-app/src/services/provider_balance.rs) | Supported provider → fetch → balance or mapped error → refresh | M. Unsupported, unknown and zero balance are distinct. |
| B04 | [CPA quota](../crates/emp-app/src/services/cpa_quota.rs), [history](../crates/emp-app/src/services/cpa_quota/history.rs) | Discover support → fetch pool/account observation → persist trend; stale/error → subsequent refresh | M. Unsupported quota APIs must not create usable-looking meters. |
| B05 | [Pricing ledger](../crates/emp-storage/src/usage/ledger.rs) | Unpriced record → price available → calculate and store; stop → leave pending records | I. Missing price does not mean zero cost. |
| B06 | [Release cache](../crates/emp-updater/src/manager/catalog.rs) | Missing/stale → request or conditional request → retain/update notes; failure → keep usable cache | I. Cached notes and a pending update check can coexist. |
| B07 | [Local Claude quota](../crates/emp-app/src/services/claude_cli/quota.rs) | Identify login → begin observation generation → accept matching usage → persist; owner change → reset current observation | I. Late results cannot become the new login's quota. |
| B08 | [Diagnostic journal](../crates/emp-storage/src/diagnostics/journal.rs) | Open part → append sanitized records → rotate/prune → continue | I. Bounded passive evidence storage; it never authorizes request retries. |
| B09 | [Report fact cache](../crates/emp-storage/src/usage/ledger/read_cache.rs) | Empty → load timestamped facts → reuse or extend range; database version changes → rebuild; reader disconnect → interrupt/rollback | I. One disk-backed window per projection; aggregates and UI filters do not own cache state. The connection guard removes the cancellation hook before reuse. |

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
| U15 | [Settings](../crates/emp-app/web/settings.js), [routing](../crates/emp-app/web/routing-settings.js), [aggregate](../crates/emp-mod-aggregate/web/aggregate-settings.js), [decision](../crates/emp-mod-decision/web/decision-settings.js), [connections](../crates/emp-app/web/service-connections.js) | Open draft → edit → validate → save → publish result or retain draft | I. Settings values are configuration, not independent lifecycle phases. Keep per-form save ownership. |
| U16 | [Recovery UI](../crates/emp-app/web/recovery.html) | Load recovery state → ready → confirm → busy → finished / retryable error | I. Confirmation and terminal restart/exit are distinct; no normal management session is assumed. |

Report filters cancel the current read before debounce; closing account
details detaches the view without canceling its shared account read, and a
usage event during that read schedules one follow-up. Migration preview
cancellation leaves a committed import's result available; LAN Back restores
the form after consuming its one-use code, and tutorial selection replaces
the image owner to isolate late load/error callbacks.

Failed report reads retain prior data, missing measurements stay distinct
from measured zero, and preset periods refresh toward now while custom periods
remain fixed. Context help shows the catalog's usable window (95% by default),
with the compaction trigger expressed as a budget rather than a compression
ratio.

Service editors keep saved keys on the server and exclude their masked
placeholder from submitted values; an unchecked image capability stays unknown
until confirmed. Quota/login errors retain their recovery lifecycle and take
priority over transient request errors.

Persistent notification actions stay until their owner withdraws them;
grouped requests retain their layout even when one remains, while passing
notices finish before persistent content returns. The notification animation
owns dismissal timing and pauses it while read; `quiet` avoids repeating text
already announced by the dialog status live region.

Release notes use maintained language sections and preserve rich/nested
content during aggregation. UI effects use one 15 fps owner for persistent
textures and align sprite clipping with device pixels at fractional zoom.

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
| History / protocols / accounts | Tests in `emp-context`, `emp-protocol`, `emp-codex`; [aggregate contract](../crates/emp-app/src/tests/aggregate_contract.rs) |
