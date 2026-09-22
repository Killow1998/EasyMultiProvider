# Rust backend remake plan

Status: in progress on remake4rust; execution plan revised on 2026-09-22.

Reference: the current, clean Python EMP checkout is
47a8a0a5e06f82e3fd488fe00c4704fcf0b06f78 (EMP 0.11.6).
Codex consumer compatibility remains pinned to 0.155.0.

Target: replace the complete Python backend with a native EMP executable.
Keep existing HTML, CSS, JavaScript, images, API contracts, state formats and
user workflows. Python remains the production service and development oracle
until final acceptance; it is not a runtime dependency of the Rust release.

The execution order is **root → major branches → branch details → complete
acceptance**. Reuse working code and connect complete operations before
polishing isolated helpers or redesigning library internals. This replaces the
previous seven-stage execution order. Historical evidence at the end does not
define the current next action.

## Verified starting point

This inventory comes from source, Git state and disk inspection on 2026-09-22.
No new compilation, runtime test or benchmark was performed for this plan.

| Area | Observed state | Action |
| --- | --- | --- |
| Workspace | Nine crates, including uncommitted emp-integration | Reuse the workspace; do not restart the rewrite |
| Application entry | main.rs has 7,351 lines, including inline tests | Extract responsibilities and tests, not just rename it to one large server.rs |
| Native forwarding | 10a479c connects complete Responses; 9229a6e adds SSE/WS and tests | Preserve implementations and regression fixtures |
| History and switching | 442e959 adds history/context, compaction and endpoint tests | Finish consumer verification, not another helper rewrite |
| Management | Config/catalog/discovery, account/quota/history/events, migration and search handlers exist | Complete every Python operation |
| Integration draft | Manifests, catalog_api.rs, main.rs and emp-integration have uncommitted work | Preserve and review; reload/verify currently return a snapshot with runtime not_checked, so lifecycle parity is not established |
| Frontend | HTML and vision image hashes match Python; web_contract tests compare served HTML bytes | Keep one asset source and run the same frontend against Rust |
| Storage | About 37 GiB in target; about 14 GiB in debug/incremental and 23 GiB in debug/deps (rounded) | Build storage is not installed application size; clean and control artifacts |
| Verification | Previous results are retained below; latest draft is not fully verified and has no new cross-platform run | Code or tests existing does not establish complete product parity |

Progress means a named Python workflow works through Rust with the same
output, failures and persistent effects. Line counts and test counts do not
provide a meaningful completion percentage.

## Product boundaries that remain unchanged

- Codex owns conversation threads and durable history. EMP reads bounded visible
  history when needed but does not create a second transcript database.
- EMP owns routing, capability projection, context safety, subscription account
  selection, external-provider adaptation, diagnostics, and integration leases.
- The Web UI and its endpoint shapes remain the product interface during the
  backend rewrite.
- A request keeps one immutable route, credential, proxy, and deployment
  identity for its lifetime. Configuration changes affect later requests.
- One WebSocket connection and one streamed turn are owned by one backend. The
  migration never splits a live chain between Python and Rust.
- Differential tests use synthetic credentials and local deterministic
  upstreams. They never duplicate paid generation, token refresh, quota reset,
  or update installation against a real service.

## Frontend and backend boundary

The browser consumes HTTP JSON, SSE, downloads and existing model endpoints.
Rust implements that contract. Decoupling does not require two processes,
different origins, a frontend framework, a Node build or new CORS rules.

- Keep easy_multi_provider/web/index.html (including its CSS/JS) and images
  unchanged, at their current paths during backend replacement.
- Put asset serving behind the app's web module. Embedding existing bytes into
  one executable is compatible with frontend/backend separation.
- Preserve methods, paths, JSON fields, status codes, headers, cookies, events,
  downloads and observable state changes.
- A missing Rust API is backend work. Do not hide UI controls or return dummy
  success to conceal it. UI redesign is outside this migration.
- Use the unchanged browser as an acceptance consumer alongside Codex.

## Target module tree and dependencies

This is the target ownership layout, not a claim that these files exist.
Create modules when moving implementation, without empty scaffolding.

    EMP
    ├── unchanged Web UI ── existing HTTP/SSE API
    ├── Codex 0.155.0 ── Responses HTTP/SSE/WebSocket
    └── emp-app: composition root and consumer interfaces
        ├── main.rs           process entry and exit code only
        ├── lib.rs            application exports
        ├── cli.rs            arguments, desktop launch, doctor/restore
        ├── app.rs            service construction and dependency wiring
        ├── lifecycle.rs      start, background jobs, drain, stop, recovery
        ├── web.rs            existing asset bytes and serving
        ├── http/
        │   ├── server.rs     listener and connection lifecycle
        │   ├── request.rs    parsing, decoding and admission
        │   ├── auth.rs       session/bootstrap, caller and Host/Origin
        │   ├── response.rs   framing and public errors
        │   └── routes.rs     method/path dispatch
        ├── api/
        │   ├── responses.rs native/external request orchestration
        │   ├── streaming.rs SSE relay, terminal events and cancellation
        │   ├── websocket.rs turns, incremental state and recovery
        │   ├── compact.rs   history/context and compaction endpoint
        │   ├── search.rs    native search endpoint
        │   ├── config.rs    config and provider/model edits
        │   ├── catalog.rs   catalog, discovery, metadata, model refresh
        │   ├── accounts.rs  import/removal and account views
        │   ├── quota.rs     refresh/reset/history/events
        │   ├── migration.rs portable import/export
        │   ├── integration.rs enable/restore/reload/verify
        │   ├── runtime.rs   scan/select and compatibility
        │   ├── observability.rs diagnostics, usage, limits, client events
        │   └── updates.rs   update control and quit
        └── tests/            fixtures and behavior tests by feature

main.rs should normally stay under 100 lines, with no domain handlers or inline
test suites. Length is a review signal, not a reason to split a state machine
arbitrarily. Require explicit interfaces and imports: no shared mega-module,
blanket visibility expansion, or wildcard parent imports as the final design.
Group shared state by its owning service; handlers receive only dependencies
they need rather than unrestricted mutable access to every subsystem.

Keep the existing reusable crates:

| Crate | Ownership |
| --- | --- |
| emp-core | Capabilities, catalog values and immutable route resolution |
| emp-state | Config, accounts, vault, migration, transactions and locks |
| emp-protocol | Native/portable Responses, Chat/Anthropic, tools, reasoning and terminal state |
| emp-transport | Pools, proxy/TLS, compression, WS framing, admission and failure evidence |
| emp-history | Continuity, checkpoints, context guard and compaction decisions |
| emp-codex | Native catalog/identity, quota RPC/history and concrete history readers |
| emp-router | Upstream routing, protocol negotiation, retries and native requests |
| emp-integration | Existing draft for integration leases, config transactions and recovery |
| emp-app | Consumer APIs, composition and process lifecycle |

Domain crates never import emp-app. API modules call services, not sibling HTTP
handlers. Retry decisions stay with existing router/transport owners. HTTP and
WS share request/history preparation where Python behavior agrees, while
keeping transport state explicit. Only composition/lifecycle starts background
jobs. Preserve unknown native JSON fields.

Usage/diagnostic persistence and update mechanics still need backend work.
Start with cohesive modules; introduce emp-observability or emp-update only
when actual dependency boundaries justify a crate. Additional architecture
must not block working product behavior.

## Compatibility oracle

The `contracts/` tree records four independent contract levels:

1. **Bytes:** exact assets, preserved pass-through frames, canonical hash inputs,
   fixed framing and error bodies.
2. **Events:** ordering, IDs, indices, reasoning/answer separation, tool
   lifecycle, usage and terminal truth.
3. **API:** methods, paths, query handling, authentication, status, headers,
   JSON distinctions, downloads and state transitions.
4. **State:** cross-language reads/writes, merge rules, locks, permissions,
   schemas, rollback and preservation of unrelated data.

Each fixture names its source test, reference revision, transport, dialect,
normalization and expected side effects. The harness has a Python adapter, a
Rust adapter and independent consumers: the unchanged browser code, raw
HTTP/SSE/WebSocket clients, and the pinned official Codex 0.155.0 binary.

Fresh ciphertext, compressor bytes, TLS records and TCP segmentation are not
stable application contracts. They are compared after decoding or by semantic
properties. Generated identifiers and timestamps use injected deterministic
sources or narrowly documented relationship checks.

State compatibility covers config and secret files, account model caches,
native/generated catalogs, integration/search/runtime records, web sessions,
`usage.sqlite3`, its legacy backup, `quota_history.sqlite3`, and
`api_prices.json`. SQLite compatibility means schemas, rows, queries and
transactions rather than page-identical files. Python JSON serialization used
by ETags, fingerprints, usage IDs and match keys remains exact.

## Execution order

### Root — make the application small, owned and runnable

1. Preserve the interrupted Git diff. Move inline tests out of main.rs without
   changing assertions or Python fixtures.
2. Extract entry/CLI, composition, lifecycle, HTTP and assets; move handlers
   into their owners. Replace global reach-through with explicit dependencies.
   Keep functional parity fixes reviewable separately from structural moves.
3. Connect all already-implemented native/external endpoints through the new
   dispatch, preserving credential, history and retry ownership. The root must
   run real operations, not remain an empty skeleton.
4. Review the integration draft in place and retain working lease logic.
   Snapshot-only operations are not implementations of reload or verification.
5. Record and clean rebuildable target artifacts once. Use one target and
   consistent feature/profile flags; reduce dependency debug information and
   use limited application debug information in dev/test. Retain a useful
   incremental cache rather than cleaning every test. Measure after rebuilding
   before claiming savings. Preserve source, fixtures, installed programs,
   user state and toolchains.

Root acceptance: explicit ownership, a thin entry and a running Rust server
that still serves unchanged assets and existing HTTP/SSE/WS operations.
The implementation and local evidence are recorded below. Continue completing
the missing management operations and runtime/integration behavior.

### Major branches — complete backend operations

After the root works, complete these branches in order. This is the whole
backend checklist, not permission to stop after its first row.

| Branch | Reuse | Complete before acceptance |
| --- | --- | --- |
| Model traffic and conversation | Native/external Responses, SSE/WS, projection, history/context and compaction | Codex tool turns, incremental/full recovery, cancellation, thread/subagent propagation, native↔external and long↔short switches, history continuity, compression and search through real endpoints |
| Existing management UI | State, catalogs/discovery, accounts, quota/reset/history/events and migration | Every browser operation, including metadata/vision fixture, account model refresh, capabilities, limits, config side effects, usage/scan and diagnostics/client events; no production API placeholders |
| Runtime, integration and lifecycle | Integration draft, quota subprocess support and server lifecycle | Real enable/restore/reload/verify, runtime scan/select, search integration, offline doctor/restore, no-argument desktop startup, quit, drain, restart/crash recovery and update worker |
| Distribution | Existing packaging/update behavior and artifact identities | Native Linux/Windows/macOS packages, assets, Python→Rust update/rollback and Rust→Rust update, without a Python runtime requirement |

Derive each branch's operation list from Python server.py, main.py, browser
call sites and existing tests. Map methods/paths, CLI actions and background
tasks to Rust owners and test evidence while implementing. An operation
without evidence stays open; do not build a separate documentation system first.

Reuse test_server, test_codex_*, test_*integration*, test_quota*, test_usage*,
test_diagnostic*, test_self_update, test_linux_update and
test_packaged_self_update fixtures and expectations. Existing protocol and
transport oracles remain connected after extraction.

### Branch details — close actual behavioral differences

Finish normal operations before expanding rare-input matrices or tuning
abstractions. Then close observed gaps in errors/status, headers/ETags,
expiry/reset transitions, reconnect ordering, persistence and platform behavior.

Credential leakage, history loss, duplicate generation/reset, wrong retries,
tool pairing errors and mixed reasoning/answer content immediately block the
affected operation. Record other differences here and close them before final
acceptance without delaying independent work or normalizing them away.

Earlier non-blocking gaps needing revalidation: legacy Retry-After dates and
malformed upstream JSON wording during plaintext collaboration restoration.
Older notes about missing native streaming and history preflight are
superseded by current commits; verify those implementations instead of
repeating their original implementation tasks.

### Whole-product acceptance and release

- Every Python production endpoint, CLI action and background task has a Rust
  owner and behavioral evidence. All unchanged UI operations work.
- Differential fixtures compare output, error type, HTTP status/headers,
  event order/terminal state, retry count/decision and persistent effects.
  Normalize only genuine nondeterminism; deterministic UUID/hash inputs and
  relationships between generated IDs remain exact.
- Real Rust processes pass HTTP/SSE/WS and pinned Codex 0.155.0 consumer flows:
  history, tools, subagents, compression and every model-switch direction.
  Live model checks use the lowest supported reasoning intensity and preserve
  the user's exclusion of hakimi.
- Linux x64, Windows x64, macOS Intel and Apple Silicon packages start and
  pass install/update/recovery checks. Cross-compilation alone is insufficient.
- Performance gates below pass against Python before improvement claims or
  replacing the production service.

Python continues serving throughout implementation. Final cutover is separately
reversible and follows established release authorization. Development tests
never overwrite production state or installations.

## Fast feedback and build storage

- New behavioral acceptance tests use Python drivers to launch the actual
  Python/Rust processes, send the same inputs through public endpoints and
  compare outputs, errors, retry counts, terminal events and persistent effects.
  Reuse existing Python scenario bodies/assertions; internal Rust structure is
  not the test target. Retain useful existing regressions without adding
  implementation-mirroring tests.
- During edits, run focused checks/tests for affected owners. Reuse fixtures;
  do not add tests merely proving files moved or run the workspace after every
  helper change.
- After a complete operation group is wired, run fmt, warnings-denied clippy
  and workspace tests with the live Python oracle. Repeat broad checks only
  when new changes or failures justify them.
- Make small reviewable local commits. Push meaningful accumulated work to
  remake4rust; do not dispatch CI or repeatedly update PRs for small changes.
  Cross-platform CI is for substantive acceptance points.
- Current workflows have PR triggers and main pushes; inspect the real trigger
  context before assuming a branch push cannot trigger CI.
- target is disposable development output, not package size or runtime memory.
  Record source, rebuilt target, release binary and package sizes separately.
  Keep routine feature/profile flags consistent. Full cleaning is deliberate,
  not part of every test loop. Avoid duplicate concurrent builds.

## Security and reliability invariants

- Vault files remain `easy-multi-provider-v1\n` plus Fernet ciphertext.
- Migration files remain `EMP-MIGRATION\x01\n`, schema 1, a 16-byte salt,
  scrypt `N=16384,r=8,p=1`, and a Fernet payload in the Base64 envelope.
- Unix private directories/files remain 0700/0600; symlinks are rejected;
  Windows uses tested user-restricted ACL and reparse-point rules.
- Rust locks contend with Python `flock` and Windows one-byte locks on the same
  paths during the migration window.
- Diagnostic fields are allowlisted before serialization and credential-shaped
  strings are redacted. Prompts, tool values, headers, tokens and ciphertext do
  not enter logs.
- Management remains protected by the session cookie; proxy calls accept only
  the current native bearer identity. Host and Origin checks remain strict.
- Proxy credentials are sent only to proxies, including CONNECT. Loopback
  upstreams bypass proxies.
- Updates retain official asset identity, allowed HTTPS redirect hosts, exact
  size and SHA-256 checks, a 512 MiB ceiling, restricted extraction, sibling
  staging and rollback. Protected Linux installs remain package-manager owned.

Request admission preserves the current 64 MiB baseline, 16 MiB growth quantum,
1 GiB hard limit, eight-times expanded-memory reservation and 512 MiB system
headroom. Memory pressure returns HTTP 503 / WebSocket 1013; hard size failures
return HTTP 413 / WebSocket 1009. SSE lines/events remain bounded at 1 MiB and
pre-output buffers at 256 events / 1 MiB.

Retries form one reviewed decision table across HTTP, streams, protocol
negotiation and native WebSockets. Partial WebSocket send counts as sent. The
only after-send native fallback remains peer-initiated close 1009 before a valid
response event. Incremental recovery requests a full request from Codex and
never forwards a delta over a fresh transport.

## Performance acceptance

Release builds are compared with the locked Python source environment and the
packaged Python executable on the same host. Reports include warmup,
distributions, CPU, RSS/private bytes, descriptors, task/thread count and
upstream request count.

| Workload | Required gate |
| --- | --- |
| 1 KiB–1 MiB requests at concurrency 1/8/32/64 | throughput at least 95% of Python; p50/p95/p99 local overhead at most 110%, with a 1 ms noise floor |
| Scheduled text/reasoning/tool SSE and WS | no loss/reordering; p95 added delay at most 5 ms normally and 20 ms at 128 streams |
| 16–128 MiB histories with identity/gzip/zstd | identical admission and content; CPU at most 110%; peak memory at most 105% plus 8 MiB |
| 224 idle WebSockets with management traffic | management p95 at most 100 ms on the reference host; no unexpected admission failures |
| Cancellation at each transport phase | resources released p99 within one second; no child or reservation leaks |
| Large usage/history scans | identical totals/checkpoints; elapsed time at most 110%; no forwarding stall |
| Startup/shutdown and one-hour churn | readiness within Python plus 100 ms; documented shutdown; stable post-warmup resources |

These gates prove non-inferiority. A claim of improvement requires a repeatable
material gain in a named metric, such as at least 20% lower preparation CPU or
peak memory, or at least 15% higher sustained throughput, while every
compatibility gate passes. Local overhead does not establish model TTFT, token
throughput, answer quality, or resolution of an upstream 5xx incident.

## Review and task ownership

Point owns root extraction, manifests/interfaces, review, integration tests,
commits and pushes. Each implementation task is a complete operation with its
Python reference, observable contract and isolated file ownership.

When user-authorized G5F3 delegation is available, assign disjoint work in the
current branches after root interfaces are clear. Run one G5F3 call at a time
until its concurrency limit is known. On 429, Point immediately takes over
without waiting or repeating the failed call. Ask Atria high only about a
concrete unresolved protocol/state-machine question; independent work continues.

Point reviews credential ownership, retries/ambiguous sends, native metadata,
history/subagent identity, compaction, tools/reasoning, reset idempotency,
rollback and usage attribution. Check discrepancies against Python before
changing expectations.

Commits use the current GitHub account as primary author with:
Co-authored-by: Point <point@local.invalid>

Preserve interrupted or unrelated work. Report completed user operations and
verified evidence, not helper counts, speculative percentages or unmeasured
performance gains.

## Implementation evidence — 2026-09-22

- The entry is now nine lines. Application construction, lifecycle, HTTP
  framing/authentication/dispatch, browser assets, domain APIs and services
  have separate modules. Config/vault, transport, accounts/quota and integration
  state have explicit owners. The listener is owned by lifecycle rather than
  shared with every handler; account identity and notifications no longer
  depend on catalog or quota handlers. The longest production app module is
  the WebSocket turn state machine, rather than a renamed monolithic entry.
- Interrupted integration work was retained and connected. SIGTERM/Ctrl-C and
  authenticated quit now run owned-lease restoration. The browser still uses
  the original asset bytes. No Python production code or installation changed.
- The Python E2E driver launches two real processes against one loopback fake
  upstream. Existing Chat regression scenario bodies/assertions run unchanged
  over HTTP. Checks cover bootstrap/config/assets, native/Chat/Anthropic JSON,
  compressed input, SSE reasoning/refusal/tools/usage, WS turns, a 503 without
  replay, quit, and SIGTERM with exact Python/Rust restored TOML comparison.
- These endpoint tests found and fixed generic Rust 503 wording and an extra
  blank line left by repeated insertion/removal of integration fields.
- Local formatting, workspace clippy with warnings denied and workspace
  all-target tests passed with the live Python oracle enabled. Existing
  integration and app endpoint regressions were preserved. No new CI run was
  dispatched; this is not new cross-platform or performance evidence.
- Cargo clean removed 38.8 GiB of logical artifacts; the worktree excluding
  target measured 12 MiB. After rebuilding and the workspace checks, target
  measured about 2.8 GiB, versus about 37 GiB allocated before cleanup.
  Dev/test retain limited app debug symbols and omit dependency debug symbols.
  This measures local build storage, not release size or runtime memory.

Remaining product acceptance work includes complete management operations,
runtime synchronization and offline CLI parity, usage/diagnostic persistence,
service ownership/draining, update/rollback packaging and consumer/performance
verification. The full rewrite is not complete.

The next local control-plane work replaces line-based TOML edits with
toml_edit, preserving quoted keys, multiline instructions, comments and nested
configuration in real Python/Rust enable/restore round trips. The service now
holds Python's state/service.lock for its lifetime and uses the same lease.lock
path as the Python application. A competing Python or Rust process is rejected
without changing the running owner's config. The unchanged UI's vision image
and request-limit APIs are connected and compared through authenticated HTTP.
All 15 Python process scenarios, app/integration regressions, and focused
warnings-denied clippy pass. This work remains local; no CI was requested.

Offline doctor/restore now run from the Rust executable. Python-driven checks
compare human and JSON output, process exit codes, repeated restore, relative
state-directory resolution, durable runtime records and private permissions.
Both implementations consume the same Python-created integration lease.
Integration and runtime persistence now reuse emp-state's atomic writer rather
than a second temporary-file implementation. User-selected paths are resolved
before acquiring service ownership, matching Python's startup path handling.
CLI help/desktop defaults and live runtime synchronization still need work.

Runtime verification now queries the actual shared Codex control WebSocket,
including paginated model lists and model names/descriptions. Enable, restore,
reload and verify persist the resulting observation separately from the
configuration lease; reload follows Python's read-only observation behavior.
Empty model pickers are rejected before changing Codex configuration, and
native-auth integrations use the dynamic catalog. Python-driven process tests
verify matching/stale catalogs and the empty-picker rejection through real
HTTP and control sockets. Seventeen process scenarios and the additional
empty-picker scenario passed; workspace tests with the live Python oracle,
formatting and warnings-denied clippy passed locally. Runtime installation
discovery/selection remains separate outstanding work. Next, reuse the actual
Codex 0.155.0 consumer scripts against the Rust process before completing the
remaining backend operations. No production service was switched or CI run.

The existing official Codex consumer scripts now accept EMP_RUST_BINARY and
launch the Rust process while keeping their upstream fixtures and observable
tool/output assertions. Actual Codex 0.155.0 exposed missing external tool
namespace/discovery adaptation, rejected empty startup turn IDs, repeated
failed WebSocket upgrades, and omitted model metadata on reused connections.
These paths now preserve request-local aliases and restore client tool-search
items, relay native prewarm, retain native incremental sockets, and use a
bounded shared fallback cooldown. Native malformed JSON events follow the
same skip behavior as Python; invalid UTF-8 remains an error. Reused handshake
metadata repeats only model headers, never stale turn state.

The two external CLI/tool-discovery scenarios and all fifteen native CLI
metadata scenarios pass, including real HTTP upgrade rejection, server search,
policy failures without generation replay, and actual upstream model reroutes.
Nineteen Python/Rust process scenarios pass, including a three-protocol tool
namespace/history/choice round trip. Workspace all-target tests with live
Python oracles and warnings-denied clippy pass locally. tools/test_codex_runtime.py
is the isolated consumer entry point; tests use the lowest advertised effort.
Runtime discovery/selection and the remaining management/lifecycle operations
are next. These results do not establish full migration or performance parity.

Codex installation scanning and source selection now use bounded version
probes and a 60-second inventory cache. Known app/plugin, managed-package,
editor and PATH layouts are separate from shared-runtime observation. Target
preferences persist through restart while quota helpers independently choose
a compatible installation. A real-process Python/Rust fixture compares scan,
multiple installation priorities, target selection, invalid selections and
restart output using the same executable files. Focused E2E, formatting,
warnings-denied clippy and the full workspace with live Python oracles passed.
The platform layouts are implemented but Windows/macOS execution still awaits
cross-platform acceptance. Next: connect context capability status and the
remaining management observability, preserving existing frontend assets.

## Historical verification evidence

These dated records describe their recorded revisions only. Statements such
as “next” or “remaining” below are historical; the inventory and execution
order above govern current work.

Progress evidence on 2026-09-21:

- Foundation CI run `35625874619` passed the Rust format/lint/test lane, the
  complete Python suite, and the pinned Codex protocol lanes on Linux, macOS,
  and Windows.
- `emp-state` now proves both-direction interoperability for the existing
  Fernet vault and scrypt/Fernet `.emp` envelope with synthetic Python fixtures
  and a live local Python oracle. It also implements private master-key files,
  atomic encrypted file writes, bounded reverse-order rollback, symlink and
  Unix ownership/permission checks. The Rust workspace now runs on Linux,
  macOS, and Windows in CI. Route presentation, subscription search, runtime
  source selection, and catalog ETag helpers are also checked against a live
  Python oracle. Provider identifiers and pasted external API URLs now share
  the Python contract as well, including automatic origin/request-URL cleanup,
  explicit provider paths, loopback HTTP, IPv6, and Unicode paths. Rust also contends
  on the same POSIX `flock` and Windows one-byte range as Python before changing
  integration state. Full provider records now preserve Python defaults,
  protocol/auth constraints, protocol observations and explicit boolean
  capabilities under a live differential oracle. Primitive model values now
  preserve Python's modality bounds, Unicode normalization, Codex projection,
  concrete protocol filtering and reasoning-effort ordering. Model capability
  provenance now also preserves top-level-over-nested precedence, raw-key
  explicitness, default source/confidence selection and supplied observations
  under frozen and live Python oracles. Context calibrations preserve the
  eight-record bound, validation order, Python numeric coercion, provenance
  defaults and accepted ISO timestamp forms. Complete model records now compose
  those primitives in Python's validation order, including raw-key presence,
  the legacy output-limit alias and Python conversion failures. Pure top-level
  configuration composition now also preserves entity order, uniqueness,
  prefix conflicts, provider references and tail-control normalization. The
  pathless Web-update merge preserves credentials, runtime/path selections,
  discovery state, calibrations and capability provenance under frozen and live
  Python oracles. Managed account and provider credential paths now use the same
  config-relative, home-relative, strict-false resolution and final-symlink
  checks as Python. Configuration loading now preserves missing-file defaults,
  normalization, path canonicalization and Python-visible failure classes.
  Configuration saving now normalizes before persistence, encrypts new provider
  keys, preserves masked managed keys, cleans obsolete managed secrets, writes
  private files atomically and participates in a caller-owned rollback without
  committing it early. Its serialized configuration and decrypted secret match
  the live Python oracle. Rust can now decrypt and validate Python `.emp`
  bundles, merge providers, models and presentations without deleting local
  entries, identify or rename subscription accounts, re-encrypt imported
  credentials with the destination vault, and roll every file back on a failed
  commit. The complete provider/account path also matches the live Python
  oracle. Export now filters all seven non-empty native/subscription/external
  category combinations, carries only their route and family dependencies,
  emits portable paths, includes available native login state, and encrypts
  every selected credential in a Python-readable bundle. Private-state writes
  now reject Windows reparse points, assign a protected current-user-only DACL
  and owner, and validate that exact shape before loading a key. The persistent
  30-day browser session is byte/schema compatible with Python, and the Rust
  local server now enforces the same one-use bootstrap, Host/Origin boundary,
  cookie attributes, restart reuse and expiry rotation while serving the
  unchanged Web UI.
  Account metadata normalization also matches Python's sorting, UTF-8 byte
  limits, truthiness, quota preservation and exact validation order.
- Runtime compatibility run `35636981488` passed the complete Python suite,
  Rust workspace, live state/configuration oracles, and pinned Codex protocol
  lanes on Linux, macOS, and Windows after the accepted-socket BSD fix.
- Runtime compatibility run `35641778568` passed all seven Linux/macOS/Windows
  jobs with the cross-language integration lock and complete provider oracle.
- Runtime compatibility run `35643375207` passed all seven jobs after account
  metadata normalization was added to the live Python/Rust oracle.
- Runtime compatibility run `35644867552` passed all seven jobs after the
  startup-output integration test was made independent of pipe-reader timing.
- Runtime compatibility run `35648039395` passed all seven jobs with model
  capability provenance in the live cross-language oracle.
- Runtime compatibility run `35650033347` passed all seven jobs with context
  calibration normalization and expanded Python ISO timestamp compatibility.
- Runtime compatibility run `35651621964` passed all seven jobs with complete
  model-record normalization enabled against the live Python oracle.
- Runtime compatibility run `35653303723` passed all seven jobs with complete
  top-level configuration normalization enabled against the live Python oracle.
- Runtime compatibility run `35655058469` passed all seven jobs with pathless
  Web-update merging enabled against the live Python oracle.
- Runtime compatibility run `35657782552` passed all seven jobs after private
  path canonicalization and configuration loading, including the corrected
  Windows path form.
- Runtime compatibility run `35659424531` passed all seven jobs with
  transactional configuration saving and its live Python save oracle.
- Runtime compatibility run `35661360516` passed all seven jobs with complete
  migration import, account identity handling and rollback coverage.
- Runtime compatibility run `35662851694` passed all seven jobs with portable,
  encrypted migration export and category-dependency coverage.
- Runtime compatibility run `35667325806` passed all seven jobs with Windows
  current-user ACL/owner enforcement, reparse-point rejection and portable
  private-state tests on Linux, macOS and Windows.
- Runtime compatibility run `35667534902` passed all seven jobs with the Rust
  management bootstrap and persistent Web-session boundary, including live
  Python interoperability and the unchanged browser asset bytes.
- Stage 3 now has complete-request and incremental-stream routing for external
  Responses, Chat Completions, and Anthropic Messages through the native Rust
  HTTP client. Live Python differential tests compare projected requests,
  ordered response events, usage, terminal boundaries, ordinary JSON fallback,
  malformed/incomplete streams, and network-chunk independence. The transport
  enforces one-MiB SSE events, a 64-MiB total stream bound, first-event/idle
  timeouts, cancellation by drop, route-scoped pools, proxy isolation, TLS and
  no transport replay. On 2026-09-21 the full local Rust workspace passed fmt,
  clippy with warnings denied, every test target, and all configured live Python
  oracles. Cross-platform run `35686632489` then passed the complete Python and
  pinned-Codex lanes on all three systems plus the Linux Rust workspace; the
  macOS and Windows workspaces exposed one shared test-CA merge failure. The
  configured-roots-only verifier retained production platform trust, but run
  `35688802145` showed the trusted request still returned a network failure on
  macOS and Windows. The fixture now removes DNS override from its success path,
  uses a loopback IP SAN and sends its complete generated chain; this passed
  locally and awaits a later milestone CI run for cross-platform evidence. Pure
  immutable route resolution now also
  matches the live Python oracle for explicit, Subscription-prefix, unique
  forward and implicit-native selection, including exact 404/503 outcomes,
  endpoint/deployment identities and unresolved `auto` protocol state. The
  Rust executable now accepts authenticated complete and streamed
  `/v1/responses` requests for resolved external models, reads the live native
  Codex bearer or existing browser session, enforces the shared request budget
  and content-decoding boundary, hydrates provider credentials only in a
  request-local snapshot, uses the native connection pool, and projects Chat,
  Anthropic or Responses results back to the Codex Responses contract. The
  downstream SSE boundary buffers lifecycle-only events until visible output,
  tool activity or a terminal event; restores HTTP status and retry metadata
  for pre-output failures; flushes every later event; emits a terminal
  `response.failed` after post-output transport/protocol failures; and drops the
  upstream response when the downstream socket closes. Real loopback smokes
  cover all three external protocols, incremental delivery before upstream EOF,
  pre/post-output failure boundaries and disconnect cancellation. SSE framing
  and activity classification are also compared with the live Python oracle.
  Automatic external protocol selection now follows Python's candidate order,
  consumes only observations whose endpoint/deployment/model identities still
  match, and falls back for complete or streamed requests only after an
  explicit pre-output 404/405/415/501 rejection. Candidate order has a live
  Python oracle and real loopback tests prove Chat-to-Responses fallback. With
  one local build job and low process priority, the 27-test `emp-app` binary
  suite, focused Router oracle and warnings-denied crate clippy pass. External
  complete and streamed requests now also make Python's single route-local
  pre-output retry for HTTP 504 or an unclassified HTTP 429 whose retry delay is
  at most five seconds; explicit quota/capacity failures, free-route 429s and
  longer cooldowns remain visible to Codex. Successful complete and streamed
  auto-protocol turns now atomically persist the selected protocol and its
  endpoint/deployment/model identity on both the provider and explicit model,
  reload the normalized configuration, and preserve access to the encrypted
  provider credential. No CI was dispatched for these local slices. Native
  and per-account model resolution now reads the same original/preserved
  catalog as Python, validates account-cache owner and base-URL identity,
  applies account-specific context clamps, and has a live Python oracle plus a
  real endpoint smoke. Native and imported quota refresh now use an isolated
  Codex app-server home, safe JSON-RPC projection, trusted executable checks,
  and the authenticated management endpoint. Native auth is never modified;
  an imported token rotated before an authentication failure is encrypted back
  to the vault and reused for the one refresh-token retry. Quota reset now
  validates and preserves the client UUID through the consume RPC, accepts only
  Python's allowlisted outcomes, and refreshes the quota snapshot separately so
  a refresh failure cannot invite another redemption. Quota refreshes now also
  write Python-compatible five-minute history buckets to the same bounded,
  private SQLite format; plan changes, swapped window positions, old-schema
  migration, duplicate-account ownership, and the authenticated history API
  have live-oracle and endpoint coverage. A bounded four-worker sampler now
  refreshes unique owners every five minutes, while the four-slot authenticated
  SSE endpoint sends revision events, keep-alives, refresh error changes, and
  prompt shutdown. Native HTTP/WebSocket generation and pinned-Codex tool round
  trips remain.
- On 2026-09-22, external discovery now covers generic OpenAI-compatible,
  Gemini, and Anthropic catalogs plus official metadata enrichment. Selected
  model merging preserves Python's capability-source precedence, manual
  fields, hidden/disabled state and validation errors under a live AppState
  oracle. Catalog composition replays the existing Python catalog suite for
  native, subscription and external ordering, visibility and presentations.
  The Rust server now exposes discovery preview/selection, catalog refresh,
  conventional and Codex-rich model lists, model lookup, management config
  reads and subscription model options. Live Python HTTP-handler comparisons
  cover JSON/status/ETag output; isolated loopback tests cover encrypted-key
  persistence across restart, native-cache preservation and rollback on a
  failed catalog write. Model/family management projections and subscription
  limits also replay existing Python UI/context tests. Local formatting,
  warnings-denied clippy and workspace tests pass with the Python oracle enabled.
  These additions have not yet received a new cross-platform CI run.
  Configuration writes now merge the browser payload through the existing
  secret/provenance rules, validate each native/account owner's subscription
  limit, persist encrypted credentials, and preserve the old in-memory state
  on a failed commit. Unchanged context settings remain valid after an upstream
  catalog limit shrinks, matching Python. Startup and saves migrate duplicate
  native visibility once; duplicate labels use identity overlap rather than
  the stricter credential-replacement comparison. Existing Python subscription
  tests, live AppState/HTTP-handler comparisons, restart checks and a failed
  secret/config commit fixture cover this slice. Native auth remains unchanged.
- Native forwarding primitives now cover the request history projection,
  response metadata allowlist, known model-alias headers, and request-local
  credential/context header selection. Native opaque state and response.model
  survive; complete foreign tool pairs lose only their optional item IDs,
  while ambiguous pairs fail with Python's projection error. EMP summaries
  decode with the same mixed Base64 alphabet and padding-bit behavior on
  both native and portable routes. Live differential tests replay the existing
  Python native model/history regressions, cover malformed history and Unicode
  model aliases, and compare 60 credential/context header cases. Local format,
  warnings-denied clippy and workspace tests pass with the Python oracle enabled.
  On 2026-09-22 those boundaries were connected into the executable for
  complete, non-streaming native `/v1/responses` requests. Real loopback tests
  start Rust EMP and verify zstd request bodies, upstream model selection,
  forward and vault-owned account credentials, thread/subagent headers, opaque
  request and response fields, response.model, rewritten model headers and the
  server-owned catalog ETag. Account 401 performs at most one serialized quota
  refresh, persists a rotated imported credential and retries with that owner;
  a failed refresh preserves the original 401 and makes no second request.
  Endpoint tests also cover the one-shot reasoning_effort fallback, direct
  429/504/context/forward-401 outcomes and one pre-header network retry.
  Collaboration preparation/restoration now matches the existing Python
  regression suite and additional malformed-container fixtures. Only marked
  plaintext calls change transport namespace; encrypted tasks stay unchanged,
  and ciphertext under EMP's plaintext namespace retains Python's ValueError
  boundary. Additional-tools schema IDs use the exact UUID5 hash input,
  including Python's JSON spacing, Unicode escaping and number formatting;
  these deterministic IDs are compared without normalization. Native zstd
  request encoding also passes both-direction Python/Rust decoding for empty,
  Unicode and 256 KiB fixtures. The HTTP response exposes header iteration for
  the native metadata filter. A live HTTP differential oracle sends the same
  fixtures through Python and Rust and compares wire requests, output/error
  payloads, selected headers, refresh counts and retry decisions, including a
  context marker beyond Python's 4 KiB error prefix. The complete local
  workspace passes formatting, warnings-denied clippy, all tests and configured
  live Python oracles. No new CI run has been requested.
  Remaining non-blocking parity gaps for the complete endpoint are Python's
  request-side context preflight callback, non-boolean `stream` truthiness,
  legacy non-RFC2822 Retry-After date forms and exact malformed upstream JSON
  wording when plaintext collaboration response restoration is enabled.
  Next: preserve native HTTP SSE streaming and cancellation, then native
  WebSocket/incremental request fidelity. Runtime/integration lifecycle effects
  of configuration writes also remain outstanding.
  The management UI is not yet fully functional; native streaming/WebSocket generation,
  history/model-switch/compaction/subagent consumer tests, complete packaging
  and comparative performance gates remain required before cutover.

Recorded Linux baseline on 2026-09-21: 1,265 tests ran in 62.022 seconds;
all passed with 28 conditional skips. The run used the existing locked virtual
environment, an isolated temporary `CODEX_HOME`, and local-only socket access.
