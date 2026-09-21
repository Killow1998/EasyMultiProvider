# Rust backend remake plan

Status: active on `remake4rust`

Baseline: EMP 0.11.6 at `47a8a0a5e06f82e3fd488fe00c4704fcf0b06f78`

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
  the live Python oracle. Import merge behavior and Windows ACL/reparse-point
  hardening remain.
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

Recorded Linux baseline on 2026-09-21: 1,265 tests ran in 62.022 seconds;
all passed with 28 conditional skips. The run used the existing locked virtual
environment, an isolated temporary `CODEX_HOME`, and local-only socket access.

Target: replace the Python backend with one native `EMP` executable while
preserving the existing Web UI, user configuration, encrypted secrets,
migration bundles, state databases, update/rollback behavior, and the observable
Codex 0.155.0 contract. Python remains a development oracle until the final
cutover; it is not a runtime dependency of the Rust release.

This is a behavior migration rather than a file-by-file translation. A stage is
accepted only when its observable contract is exercised against both
implementations or an independent consumer. Rust's language-level performance
advantages are not evidence of a product improvement by themselves.

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

## Workspace architecture

The final build produces one binary named `EMP` from a small acyclic workspace.

| Crate | Responsibility |
| --- | --- |
| `emp-core` | Shared value types, capabilities/provenance, catalog construction, immutable route resolution, identities, public failures |
| `emp-state` | Configuration persistence, vault, `.emp` migration, file transactions and compatible locks |
| `emp-protocol` | Responses dialects, Chat Completions and Anthropic projection, tools/collaboration, SSE and terminal state machines |
| `emp-transport` | HTTP pools, proxies, platform TLS, compression, WebSockets, request admission and transport evidence |
| `emp-history` | History interfaces, continuity, portable checkpoints, context guard, destination compaction and bounded caches |
| `emp-codex` | Integration/search leases, runtime observation, concrete history readers, quota RPC, native identity |
| `emp-observability` | Safe journal, measurements, quota history, usage ledger/scanning, pricing and analytics |
| `emp-router` | Request orchestration, protocol negotiation, retry policy, native connection planning and summary calls |
| `emp-update` | Release verification, staging, drain, replacement, restart acknowledgement and rollback |
| `emp-app` | CLI, HTTP/WebSocket server, management routes, assets, lifecycle and composition |

`emp-core` has no filesystem, network, process, or global-configuration access.
`emp-history` defines reader and summarizer interfaces; concrete Codex readers
live in `emp-codex`, and summary execution lives in `emp-router`. Only `emp-app`
constructs services and starts background work.

Protocol values retain unknown JSON fields where Codex can add data. A closed
enum must not silently discard native response items or metadata.

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

## Migration stages

### 1. Frozen oracle and foundation

- Record the Python and Codex revisions, environment, full test result and
  platform skips.
- Inventory endpoints, state formats and test ownership.
- Land the Cargo workspace, shared interfaces and differential driver.
- Prove Fernet/migration interoperability and compressed-WebSocket, proxy and
  platform-TLS feasibility before those dependencies become architecture.
- Keep Python as the installed/default executable.

Exit: both workspaces build; foundation tests pass; baseline evidence is
reproducible; every current behavior has an owner or fixture plan.

### 2. State-compatible local control plane

- Port configuration normalization/merge, vault and migration formats, atomic
  writes, Python-compatible locks, catalog construction and ETag generation.
- Port browser sessions, integration/search recovery and offline CLI actions.
- Serve the unchanged assets and implemented management endpoints from Rust.

Exit: Python↔Rust vault and migration round trips pass; legacy state is readable;
config and rollback scenarios preserve unrelated fields; the unchanged UI works
for implemented operations; no Codex history or native auth is written.

### 3. External request/stream vertical slice

- Resolve an external route, translate Responses to Responses, Chat Completions
  or Anthropic, call the upstream, and translate JSON/SSE back to Codex.
- Support downstream Responses HTTP and WebSocket with complete supplied
  histories.
- Preserve tools, reasoning, refusal, usage, terminal classification, failure
  codes, bounded retry and cancellation.

Exit: differential protocol/transport fixtures pass; actual socket pool, proxy,
TLS, size-limit and slow-reader cases pass; pinned Codex 0.155.0 completes tool
round trips; the first comparative benchmark report is accepted.

### 4. History and context

- Port Codex App Server/SQLite/rollout readers, exact side-chat parent checkpoint
  recovery, portable checkpoint projection, context estimation/calibration,
  destination compaction and provider replay.

Exit: native↔external and short↔long context switch matrices pass, including
pending tools, server/client tool search and cache invalidation. Rust performs no
Codex-history writes.

### 5. Native subscription path

- Port credential/account ownership, compressed native WebSockets, incremental
  response chains, HTTP/zstd fallback and metadata/header fidelity.

Exit: owner isolation, connection reuse, peer-close ordering, HTTP fallback,
full-request recovery and native model identity pass local socket fixtures and
the pinned Codex consumer suite.

### 6. Complete control plane

- Port quota and reset RPC, usage scanning/pricing, diagnostics, discovery,
  background jobs and update staging/worker behavior.

Exit: every production endpoint has an implementation and contract evidence;
restart, disconnect, fault-injection and recovery tests pass.

### 7. Release cutover

- Produce Linux x64, Windows x64, macOS Intel and macOS Apple Silicon packages
  with the existing five public asset names.
- Verify Python→Rust update success, Rust startup failure→Python rollback, and
  Rust→Rust update on every target with an active lease and existing vault.
- Remove Python from the runtime package only after compatibility and
  performance gates pass.

Exit: zero unimplemented production endpoints, four-platform package evidence,
all contract lanes green, and measured non-inferiority or improvement.

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

After the foundation commit, G5F3 work packages receive disjoint crates and
fixture directories: state, protocol, transport, history, Codex control,
observability, app API, and update/package. Point owns shared manifests and
interfaces, `emp-router`, composition, fixture normalization, integration
decisions, review, commits and pushes.

Point personally reviews retry counts and ambiguous sends; native metadata and
model fidelity; history identity and side-chat recovery; compaction and cache
keys; tool/reasoning separation; account ownership; quota reset/idempotency;
state rollback; usage attribution; and capability precedence.

Every accepted implementation commit contains one observable behavior and its
evidence. A mismatch is investigated against the frozen contract before either
implementation changes. Cutovers remain separately revertible.
