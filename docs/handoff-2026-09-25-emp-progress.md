# EMP Handoff

## coz (Opus) progress log — started 2026-09-27 07:10 UTC

Latest first. "Committed" is not "accepted"; final acceptance runs after
integration. Commit trailer per Xian: `Co-authored-by: coz <coz@local.invalid>`.

### INCIDENT — native Codex login overwritten by tests (2026-09-27)

- `tests/test_server.py` writes `state.codex_home / "auth.json"`; with no
  integration manager `codex_home` resolves to the real `~/.codex`. coz ran
  those tests at 07:42 UTC (after `d232e38`) and 09:00 UTC (acceptance), so
  the real `~/.codex/auth.json` became the fixture (`native-secret`, no
  refresh token). Python 4200 kept running but every native request got
  upstream 401 from 07:42 UTC. No plaintext backup existed; Xian must run
  `codex login` again. Imported accounts (vault, e.g. `egg`) were unaffected.
- Fix: `tests/__init__.py` forces a temporary `CODEX_HOME` for the whole
  Python suite; full suite rerun 1442 OK with the real auth.json mtime
  unchanged. Never run the Python suite without this guard.

### Integration and acceptance — 2026-09-27 (security-fixes @ `60cd0be`)

State: all six branches integrated into `security-fixes`; full acceptance
passed except one check that needs the Python mirror (below). Nothing pushed,
no CI triggered, Python 4200 untouched (pid 2413880 still running).

- CONTEXT committed `7326389` on `sec/history-ctx` (scratch
  `tmp_baseline.rs` converted into the 216-case Python oracle test; original
  kept in `artifacts/security-recovery/drafts-history-ctx/` with the worker's
  doc notes).
- Trailer rewrite (trees verified identical, author h2q unchanged):
  rollout `2abc9fe`->`40a08be`, `b1c9b22`->`5958bd6`; transport
  `24d87f1`->`6c98831`, `bda4092`->`1991e25`, `85ed7b2`->`825e6ec`,
  `001d299`->`39b3af6`; ci `d7ebcc0`->`db8b382`, `2289db2`->`11d3f86`,
  `092b5f1`->`0ab30e7`, `f7ae1ce`->`e36ba06`. No Anthropic trailer remains.
- Merges: `6d85f19` state, `cfa0810` history-ctx, `0dfaca0` http (migration
  HTTP test resolved to header auth + confirmation + 12-byte password; STATE
  rotation test moved to `X-EMP-Session`), `cf42d31` rollout, `d667c59`
  transport, `6fb4103` ci. Handoff doc kept as coordinator version; each
  branch's copy saved in `artifacts/security-recovery/branch-handoff-docs/`.
- Independent review of HTTP/ROLLOUT/TRANSPORT/CI found nothing blocking;
  fixed on top: `6af1338` rollout (symlinked CODEX_HOME rejected every DB
  parent; child rollout opened before containment/FIFO check), `787c6be`
  transport (RFC 7692 0x00 padding after BFINAL, untyped `{"error":..}`
  context errors, IPv6 same-origin redirects, real cap test), `e30ab57` http
  (legacy UI detection with stored token, legacy quota events, failure-safe
  session rotation, update test on header sessions), `05432e7`
  `persist-credentials: false`, `11052a5` web contract tests for the new
  bootstrap, `b66c4bf` Python 12-byte test password.
- Acceptance found regressions vs baseline `2c35c8f` (baseline E2E 34/34
  OK, built from `git archive` in `.emp-baseline-2c35c8f`), fixed:
  `24de43c` calibration back to the Python contract (failure estimate is an
  input estimate; the branch subtracted reserves again, giving 865 instead
  of 1249 for Python-written calibration) and runtime trust (real umask-002
  npm Codex 0775 and sticky `/tmp` fixtures were rejected); `60cd0be` UI
  probe sends JSON (real Python answers a body-less POST with 400, so legacy
  detection never worked; harness mocked 404) and E2E harness moved to
  header sessions.
- Final evidence on `60cd0be` (per-worktree target, oracle env set):
  `cargo fmt --check` ok; strict workspace Clippy clean; workspace tests 502
  passed, 1 failed = `embedded_index_matches_current_python_release_bytes`
  (compares with the unmirrored live checkout); Python `unittest discover`
  1442 OK (75 skipped); `tests.test_rust_e2e` with release binary against a
  mirrored oracle copy (`.emp-oracle-mirror` = `python_archive` + the four
  reviewed Python files) 34/34 OK, covering browser bootstrap, HTTP, SSE,
  WebSocket, history, calibration and migration.

Unfinished / next:

1. Needs Xian's go-ahead: mirror `migration.py`, `quota_history.py`,
   `server.py`, `web/index.html` from `security-fixes` into the live
   `python_archive` checkout. index.html is read per request, so 4200's UI
   changes immediately (verified legacy-cookie path against the oracle); the
   `.py` files load only on the next 4200 restart, when the adoption
   migration moves the `egg` rows to the upstream identity. Then the byte
   test passes, and CI's pinned oracle ref (`3bcf72a`) must be updated after
   `python_archive` is pushed.
2. Rust 4201 (pid 2043811) still runs a deleted binary with a deleted config
   dir; its logs are saved in `artifacts/security-recovery/rust-4201-before-e2e/`.
   Acceptance used isolated processes instead. Replace it only with a fresh
   config (the old config exists only in its memory).
3. Not code-fixable here: release signing/Authenticode/notarization keys,
   GitHub `release` environment protection and attestation settings, Windows
   build verification.
4. Accepted residuals from review: zstd decompressed cap equals the 2 GiB
   plain cap (Python 128 MiB); only one pending export confirmation per
   session; `/healthz` and `/v1/models` now require a same-origin Host;
   session token lives in localStorage (no `script-src` CSP); Rust mode
   validation stricter than Python for later `session_meta`; failed session
   rotation restore path untested (cannot simulate without root).
5. Worktrees may be removed after Xian accepts the mirror decision; the
   `.emp-cargo.sh` wrapper now uses `flock -o` so panicking tests' orphaned
   `EMP serve` children cannot hold the cargo lock.

### Build-cache hazard (affects earlier worker evidence)

Worktrees sharing one `CARGO_TARGET_DIR` reuse each other's workspace-crate
artifacts: Cargo's metadata hash for path crates uses the workspace-relative
path, and fingerprints stay "fresh" against the other worktree's unchanged
sources. Observed directly: `emp-sec-state` linked `emp-history` compiled from
`emp-sec-history-ctx`. Test results recorded from the shared
`.emp-rust-target-root` are therefore not trustworthy evidence for a specific
branch. From now on use `/home/fumo/codex_ws/agent_dev/.emp-cargo.sh` (flock +
pinned 1.93.1 toolchain + per-worktree target
`.emp-rust-target-root/per-worktree/<worktree>`). Branch evidence is re-run
during integration.

### STATE — committed on `sec/state` (not yet integrated)

- `4a0c04c` migration v2 (fixed scrypt N=2^17, 12-byte export minimum, v1
  import with 8-byte legacy minimum, fixed-per-version KDF), provider key
  origin binding, overwritten-provider summary, bounded state/integration
  reads, private file/dir helpers (ancestor-link check removed so macOS
  `/var`/`/tmp` links work). Fixed the known failing origin test (rejected
  URLs now asserted separately). Python v1 fixture now derives v1 explicitly.
- `d232e38` quota history keyed by upstream identity (Rust + Python), no
  history deletion on credential removal; fixed a draft regression where
  Rust background sampling skipped every imported account (now uses duplicate
  status like Python); rotated-credential retry + in-memory pending copy used
  by the next check and cleared on delete/reimport; private temp CODEX_HOME
  files, stale-dir sweep; atomic Codex account `config.toml`.
- `debbda5` identity-verified, idempotent adoption of legacy `egg`/`@native`
  rows at sampler start and before deletion (Rust + Python).
- Evidence (per-worktree target): emp-state/emp-codex/emp-integration/emp-app
  283 passed 0 failed with `EMP_PYTHON_ORACLE_ROOT`/`EMP_PYTHON_INTEROP`;
  real Rust<->Python v2 exporter/importer interop test passes; strict Clippy
  clean on those crates; Python `tests.test_migration` 11/11,
  `tests.test_server.ServerAccountTests` 89/89 (worktree sources).
- Python changes are in `sec/state` only; NOT yet mirrored to the running
  `python_archive` checkout.

### Live egg trend restoration — done 2026-09-27 07:47 UTC

- Ownership verified from credentials, not names: old store
  (`EasyMultiProvider/state`) and live store (`state/live-20260921/state`) egg
  credentials share the same hashed chatgpt account id, user id and email;
  only the plan changed (prolite -> plus), matching sample plan types.
- Fresh snapshots: `artifacts/security-recovery/20260927T074659Z/quota-history/`
  (`live-before-restore`, `old-source`). Inserted the 406 old egg rows under
  the live key `egg` with INSERT OR IGNORE in one transaction (live 56 -> 462,
  quick_check ok). Python 4200 not restarted; its API returns 462 points,
  plans `pro_lite`+`plus`. After upgrade the adoption migration moves `egg`
  rows to the upstream identity automatically. Old-store `@native` (812) was
  not merged (no ownership evidence gathered for it).

## Active Opus takeover — 2026-09-27 07:06 UTC

This section is the current resume point. Xian requested a documented handoff
to Opus after Codex quota exhaustion. Four work areas have local implementation
commits; STATE and CONTEXT remain uncommitted and incomplete. Nothing from the
worker branches has been integrated into `security-fixes`, deployed, or pushed.
The records below the historical divider describe earlier stages, not current
acceptance. Do not restart the Rust rewrite or discard existing work.

Point checked commits, worktree diffs, worker records and tool outputs for this
handoff. No new implementation or tests were run during handoff preparation.
Worker verification below is recorded evidence, not an independent final review.

### Coordinator and recovery

- Integration checkout: `EasyMultiProvider-rust`, branch `security-fixes`.
  Before this documentation checkpoint its HEAD was `df37abc` (only coordinator
  docs beyond common source base `2c35c8f`). Opus should take over review,
  implementation completion, and integration using the existing worktrees.
- Six existing `emp-sec-*` worktrees are retained. Four are clean with local
  commits. STATE has 17 modified tracked files; CONTEXT has five modified
  tracked files and untracked `crates/emp-history/tests/tmp_baseline.rs`.
  The formal Python checkout remains clean at `3bcf72a` on `python_archive`.
- Baseline patches, HEADs, status, and untracked files were copied to ignored
  `artifacts/security-recovery/20260927T052409Z/` in the integration checkout.
  These are local recovery data; never publish credentials or private logs.
- A new complete snapshot of current dirty patches, untracked files, worker
  handoffs, HEADs and status is in ignored
  `artifacts/security-recovery/20260927T070554Z/`, including `manifest.json`.
  The earlier `20260927T052409Z/coordinator.json` contains local task IDs and
  terminal status. Preserve both snapshots until integration is validated.
- Each worker updates **its own worktree's copy of this existing handoff file**
  before substantial work and after each tested change. Record exact commits,
  commands/results, remaining work, and the next action. Point consolidates
  the results here; do not concurrently edit the coordinator's copy.
- New commits use the current GitHub author (`h2q`) and the exact trailer
  `Co-authored-by: Point <point@local.invalid>`. Do not rewrite prior commits,
  force-push, release, or start GitHub workflows from a worker.

### Runtime, models, and quota policy

- Python EMP is the control service on port **4200**. It must remain running.
  Rust on **4201** is a separate test service. Point may stop/restart Rust as
  needed; workers use their own temporary test listeners and never touch 4200.
- The three Luna app-server workers were verified as `gpt-6-luna` / `max`.
  HTTP→STATE and ROLLOUT→CONTEXT terminated around 07:00:40 UTC with
  `429 Too Many Requests` / retry exhaustion; TRANSPORT→CI completed at
  06:31:32 UTC. No new Luna turns were started for this handoff. No Cargo or
  rustc process was observed during the final handoff check. Before editing,
  recheck that no resumed worker is writing the same worktree.
- Opus is the user-selected next implementation model; it does not need to
  relaunch Codex or wait for Codex quota. The Luna rules below only apply if
  Xian later resumes Luna workers.
- When the current native account has about **10% or less remaining** in any
  applicable finite quota window, change every Luna task to
  `egg/gpt-6-luna`, still at `max`. Preserve task history and worktree state.
  Keep this account choice for subsequent tasks; do not spend reset credits.
- Coordinator checks quota before launch and periodically during supervision.
  An unavailable/stale quota is not evidence of headroom. If the egg route is
  unavailable, preserve progress and report the specific missing route rather
  than silently using a different model or account.
- Initial Python management API snapshot: native primary used 7%, secondary
  used 12% (remaining 93% / 88%). Xian subsequently reimported egg; both its
  credential-set flag and `egg/gpt-6-luna` catalog route are now present.
  No quota reset was performed.
- The later native 58%/83% and egg 99%/84% readings are historical, not current
  availability. Worker terminal records still show `gpt-6-luna`; a successful
  switch to egg was **not verified** before the 429 failures. Do not describe
  the threshold policy as an implemented automatic failover mechanism.

### Work ownership and sequence

Use the existing worktrees; do not create more worktrees or per-worktree Cargo
caches. The Luna queues have stopped at the states below. Opus may work directly
or use previously authorized nonconflicting delegation, within resource limits.

| Task | Worktree / branch | Current HEAD / local source commits | Verified checkpoint |
| --- | --- | --- | --- |
| HTTP | `emp-sec-http` / `sec/http` | `6cdbae7` | Clean; focused checks passed; integration review pending |
| ROLLOUT | `emp-sec-rollout` / `sec/rollout` | `2abc9fe` → `b1c9b22` | Clean; history contracts 29/29; integration review pending |
| TRANSPORT | `emp-sec-transport` / `sec/transport` | `24d87f1` → `bda4092` → `85ed7b2` → `001d299` | Clean; focused checks passed; Windows build unverified |
| CONTEXT | `emp-sec-history-ctx` / `sec/history-ctx` | `2c35c8f` + five modified files + one untracked fixture | Uncommitted; 216-case differential and 15 context tests passed; app check unfinished |
| STATE | `emp-sec-state` / `sec/state` | `2c35c8f` + 17 modified files | Uncommitted; known failed test and untested final Python migration edit |
| CI | `emp-sec-ci` / `sec/ci` | `d7ebcc0` → `2289db2` → `092b5f1` → `f7ae1ce` | Clean; local checks passed; no workflow was run |

### Completed branch work and evidence

- **HTTP (`6cdbae7`):** header sessions, bootstrap exchange, browser request and
  fetch-SSE integration, operation/session-bound export confirmation, safe
  dynamic handlers, response security headers, request deadlines, framing and
  account-path validation, desktop absolute paths. Worker records 21 session,
  6 migration/management, 7 realtime, 10 catalog/config, 13 native/Codex tests,
  quota/stream/desktop contracts, app warnings-denied Clippy, app fmt, JS syntax,
  DOM harness and handler scan passing. The UI retains a compatibility path for
  Python cookie auth. Migration v2 still depends on STATE; combined behavior
  has not passed integration. Read this branch's `HTTP worker result` section.
- **ROLLOUT (`b1c9b22`, plus `2abc9fe`):** prefix control scanning, bounded
  plain/Zstd source adapter, lineage validation, root/symlink/ambiguity checks,
  metadata/mode validation, partial-window and nonzero frozen-prefix cases.
  Final history contract suite passed 29/29; the decompressed byte-limit unit
  test passed separately. `cargo fmt -p emp-codex` and diff check passed.
  Source implementation and new tests require coordinator review before merge.
- **TRANSPORT (`85ed7b2`, plus prior two fixes):** bounded context classifier,
  completed event filter, account-prefix fallback protection, same-origin
  redirect policy and Windows registry absolute path. Native complete/stream
  Python oracle, route oracle, SSE, deflate, cancellation, HTTP client 10/10,
  discovery 3/3 and targeted classifier/path tests passed. Windows cross-check
  stopped in `aws-lc-sys` because GNU `cc` was used for an MSVC target; this is
  not Windows validation. `001d299` records the checkpoint.
- **CI (`2289db2`, plus `d7ebcc0`):** runtime candidate trust checks; pinned
  Actions, default-branch release gate, environment/provenance wiring. Runtime
  tests 8/8, packaging validator 5/5, YAML parsing and SHA-reference inspection
  passed locally. `actionlint` was unavailable. Windows ACL checks, protected
  environment settings and attestation enablement remain unverified. The
  updater checks GitHub metadata digests and does **not** verify provenance or
  an independent release signature. Do not mark these trust gaps complete.

### Exact unfinished resume points

**STATE first:** All changes are in `emp-sec-state`, including the Python
`easy_multi_provider/server.py`, `migration.py`, and `tests/test_server.py`.
They are NOT applied to the running `EasyMultiProvider/python_archive` checkout.

1. The new Python and Rust `quota_history_follows_upstream_identity_across_delete_and_reimport`
   tests were run; the Python test passed with the existing venv. The draft
   removes deletion of trend samples and keys new samples by upstream identity.
   Review identity separation and missing/expired-auth behavior before adoption.
   **Existing rows keyed `egg` / `@native` have no implemented migration to the
   new identity keys.** Merely keeping old rows will not make old graphs appear.
   Plan an identity-verified, idempotent migration/recovery; never guess ownership
   from a reused display name. Historical snapshots remain read-only.
2. A crate-scoped `emp-state` run ended with the known failure
   `changing_provider_origin_does_not_carry_over_a_stored_key` in
   `crates/emp-state/tests/web_update_merge_compat.rs:177`. The fixture calls
   `.expect("valid Web update")` on non-loopback HTTP, which existing validation
   correctly rejects. Keep HTTPS enforcement; distinguish rejected URLs from
   valid origin changes in the regression. Earlier sandbox fixture failures
   were retried with local-network access; this assertion remained.
3. The last code edit changed Python migration to write a v2 envelope with
   fixed scrypt N=2^17, 12-byte export passwords and v1/v2 import (8-byte legacy
   minimum). **No test ran after this edit.** Existing `_fernet` fixture calls
   may still assume v1. Rust's current `python_interop` check manually decrypts
   ciphertext; it is not proof that the real Python `read_bundle`/import accepts
   v2. Finish actual importer interoperability, legacy fixtures, invalid fixed
   KDF rejection and UI consistency without weakening old-data compatibility.
4. Token-rotation save-failure recovery, private temporary auth storage, atomic
   integration writes and cross-platform filesystem behavior still need review
   and completion; the quota process/integration files have not been changed
   by this worker. Do not claim the whole STATE assignment is implemented.

**CONTEXT second:** In `emp-sec-history-ctx`, five tracked files are dirty:
`context.rs`, `context/calibration.rs`, the app context service, the Python
oracle test and this worktree's handoff; `tests/tmp_baseline.rs` is untracked.

- The 216-case live Python differential found and fixed same-turn boundary
  precedence and tool-record JSON-spacing differences. It compares outcomes,
  bodies and every summary request. The complete targeted `python_oracle`
  binary passed 4/4; `cargo test --offline -p emp-history --lib context::`
  passed 15/15 (including calibration/deployment/output-budget tests).
- The last launched check was
  `cargo test --offline -p emp-app --lib services::context::tests::only_explicit_context_errors_produce_failure_observations`.
  Its tool output stopped during compilation before the 429. There is no
  recorded final exit status; rerun this focused check, not the entire workspace.
- Review performance/bounds and retained-history equivalence, then finish
  targeted formatting/Clippy and commit. Preserve or replace the scratch matrix
  only after retaining its useful evidence. Do not equate one passing wall-clock
  guard with a measured performance improvement.

### Opus execution order

1. Read this section, inspect current branch/status in each existing worktree,
   and verify no revived worker is editing. Use the saved patches as recovery,
   not as replacements for newer work. Do not launch new Luna turns.
2. Finish and locally commit STATE, then CONTEXT, using the concrete failures
   and pending checks above. Mirror required Python fixes deliberately into
   `python_archive` only after review; never restart Python 4200 during control.
3. Review and integrate all six branches into `security-fixes`. Preserve the
   coordinator's current handoff when resolving doc conflicts; retain useful
   worker verification instead of accepting a stale file wholesale. Both
   ROLLOUT and TRANSPORT alter `Cargo.lock` (zstd and regex edges); retain both.
   HTTP and STATE overlap in the account deletion/refresh-race tests; combine
   header-auth and retention behavior. No source integration has happened yet.
4. Run affected tests per change. Once all source is integrated, run final fmt,
   warnings-denied Clippy, workspace tests and main browser/HTTP/SSE/WS/history/
   compaction/model-switch/subagent E2E. Authenticate the updated browser flow;
   do not rewrite the oracle to silently accept intentional security changes.
5. Rebuild only isolated Rust test service/package; keep Python 4200 serving.
   Cross-platform evidence, trust prerequisites and any live-history recovery
   are distinct remaining acceptance items. No new CI run or release is
   authorized by this handoff. Preserve progress if an external prerequisite
   blocks one item; complete unaffected work and report the exact gap.

**HTTP assignment:** Own `emp-app` HTTP/auth/web, request lifecycle, desktop
launcher, the migration/quota management adapters, relevant app tests, and
`easy_multi_provider/web/index.html`. Complete bootstrap/header authentication
end-to-end, including all browser requests, downloads and quota events. Remove
dynamic-data interpolation into executable inline handlers. Export confirmation
must be session/operation-bound, short-lived and single-use; it is not a claim
of independent reauthentication. Complete safe path parsing, common response
headers, Host checks and total request read deadlines. Keep Codex caller auth
working. Coordinate the export v2 minimum (12 UTF-8 bytes) with STATE while
retaining legacy v1 imports (8 bytes). Do not redesign the UI or its layout.

**ROLLOUT assignment:** Own `emp-codex/src/history.rs`, `history/**`, its
contract tests and only necessary `emp-codex/Cargo.toml` dependency changes.
Review `2abc9fe`, then converge reverse/full parity using the existing walker,
shared control-state scan and replay engine. A checkpoint cannot discard
earlier turn-success, model, role/dedup, identity/mode or ordinal information.
Verify partial-window and non-zero frozen-prefix cases on identical inputs.
Validate SQLite/session history modes. Restrict ancestor lookup to legitimate
session roots without recursive symlink traversal or arbitrary first matches.
Add bounded `.jsonl.zst` full replay using Codex's uncompressed byte-offset
semantics. Reject invalid lineage bounds and distinguish depth from cycles.
Keep modules cohesive; do not create another giant history implementation.

**TRANSPORT assignment:** Own `emp-transport`, `emp-router`, `emp-protocol`,
`emp-core` and their relevant tests. Review the two existing commits; finish
the context-error draft and its incomplete call site. Bound error observation
cost without losing normal context errors or changing terminal/retry truth.
Check streaming order, compression handling, cancellation and pre-output-only
retry. Prevent unknown/disabled account prefixes silently falling into a
forward route; retain explicitly configured forwarding. Avoid forwarding
credentials across unapproved origins or redirect downgrades. Finish Gemini
metadata redirect handling and Windows registry tool path handling. Coordinate
origin/credential configuration changes with STATE rather than both editing it.

**CONTEXT assignment:** Own `emp-history/**` and app services
`{context,history,compaction}.rs`. Review and finish incremental compaction
estimation against the existing oracle, preserving retained items, tool pairs,
summary requests and error results. Calibration must distinguish input/output
budgets and actual deployment identity; repeated uncorroborated errors alone
must not shrink every session's window. Preserve hot reload semantics. Convert
useful existing baseline evidence into readable outcome tests, keeping recovery
copies of scratch until its useful content is retained.

**STATE assignment:** Own `emp-state/**`, except any explicitly coordinated
provider routing edit, plus `emp-codex/src/quota{.rs,/process.rs}`, app services
`{accounts,quota}.rs`, `emp-integration/src/lib.rs` and relevant tests. Review
the existing migration v2, fixed KDF cost, origin comparison and bounded-file
drafts. Preserve v1 import compatibility. Finish durable recovery when token
rotation succeeds but saving fails, private temporary storage and atomic config
writes. Avoid broad path checks that reject normal macOS/Windows installations.
Clear/rebind secrets only on a meaningful origin change, not `/v1` cleanup.
Record any intentional security contract changes instead of weakening the
archived Python oracle to hide a difference.

**New user-reported STATE priority — quota history retention:** Xian removed
egg after OAuth expired, then reimported it and found the trend empty. Both
Python `AppState._delete_account` and Rust `delete_account_state` call the
history store's delete operation. At discovery, the live store had only two new
egg samples; a separate older local store had 406 egg samples. Point saved
read-only SQLite snapshots under the ignored recovery directory before any
recovery. Do not automatically clear historical usage when removing credentials.
Reimporting the same upstream account should reconnect its history; reusing a
display name for a different account must not attach the prior account's data.
Use stable account ownership, keep history within its existing retention policy,
and preserve explicit deletion semantics as a separate operation if needed.
This fix includes the Python source in `EasyMultiProvider` and the Rust source,
with focused removal/reimport and identity-separation tests. Do not restart the
Python service. Live restoration remains undone: Opus must verify ownership,
preserve a fresh SQLite snapshot, and review the merge before touching live data.

**CI assignment:** Own `.github/**`, packaging scripts and
`emp-codex/src/runtime_inventory/**` with their tests. Review `d7ebcc0`; finish
runtime candidate validation without breaking supported installations. Verify
release gates and provenance configuration locally. Do not start workflows,
publish, change repository environments, or invent signing credentials.
Provenance generation is separate from updater signature verification; record
the remaining trust/configuration prerequisites explicitly.

### Verification and integration rules

- Review before editing. Preserve unrelated work. Only use local synthetic
  fixtures for boundary/failure checks; no third-party security probing.
- Run targeted tests for changed behavior. Do not run the full workspace per
  worker or after every small change. Point runs final format, strict Clippy,
  workspace and consumer E2E once the implementations are integrated.
- Serialize Cargo work to control storage/RAM:
  `flock -x /home/fumo/codex_ws/agent_dev/.emp-security-cargo.lock env CARGO_TARGET_DIR=/home/fumo/codex_ws/agent_dev/.emp-rust-target-root CARGO_BUILD_JOBS=2 cargo ...`
- Python oracle: `EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider`
  and `EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python`.
  Use the pinned Python semantics for unchanged workflows; document deliberate
  authentication/migration changes and use local Codex source for new history
  forms that the archived Python does not support.
- Workers commit reviewed source and their own progress record locally. Point
  reviews and integrates; no worker merges into another worker's branch.
- Earlier one-time Runtime/Package authorization was already used at
  `5fa6a64` and both passed. Latest `main@041f000` Runtime passed, while Package
  failed a Windows CRLF assertion; `685253a` fixes the assertion in this branch.
  There is no CI evidence for these pending security fixes. Do not trigger a
  new run without corresponding authorization.
- Final acceptance includes the working browser, authenticated management,
  history/compaction/model switching/subagent, cancellation, persistence,
  package lifecycle and required cross-platform proof. Model canaries use
  authorized accounts/providers and lowest reasoning for the *test* calls;
  implementation workers remain Luna **max**.

### Latest coordinator checkpoint

- Latest instruction is handoff to Opus after Codex quota exhaustion. Point
  prepared documentation only, without restarting workers, compiling, testing,
  changing product source, merging branches, recovering live data or starting CI.
- Completed and dirty worktree contents were saved in the 07:05:54 UTC recovery
  snapshot. The two unfinished workers' 429 errors and their last commands are
  reflected above. No completed integration or release is claimed.
- Python `emp-xian-local.service` (4200) and Rust `emp-rust-test-4201.service`
  were both observed active/running during this handoff. Earlier inspection
  found Rust's executable and temporary cwd marked deleted; recreate its
  isolated installation before a future restart. Protect Python 4200.
- Next action: Opus begins with STATE's failed origin-update fixture and
  unfinished migration/legacy-history handling, then CONTEXT's pending app
  check and commit, followed by branch review/integration and final acceptance.

---

# Historical EMP Handoff — 2026-09-25

## CI worker checkpoint — 2026-09-26

- Xian authorized continuation of the CI assignment after TRANSPORT. Worktree
  is `/home/fumo/codex_ws/agent_dev/emp-sec-ci`, branch `sec/ci`, at
  `d7ebcc0` (`Pin workflow actions to SHAs, gate releases on default branch`).
- The preexisting uncommitted runtime inventory draft is preserved across
  `discovery.rs`, `mod.rs`, `version.rs`, and `trust.rs`. This phase adds
  Unix writable-file and set-id rejection plus a trusted symlink discovery
  test. No workflow or packaging source was changed.
- Scope: complete safe runtime candidate validation without rejecting
  supported installations, inspect release gating and provenance locally,
  and record remaining trust/configuration prerequisites. No workflow,
  release, publish, push, environment change, or Python 4200 restart is
  authorized.
- The shared Cargo lock and target directory from the coordinator handoff were
  used. Final diff/status review and local commits are complete; next is
  coordinator review and consolidation.
- Baseline targeted checks passed: `cargo test --locked --offline -p emp-codex
  runtime_inventory` (7 unit tests; unrelated integration tests filtered) and
  `python3 -m unittest tests.test_release_packaging -v` (5 tests).
- Review confirms `version::observe` applies the trust check before probing;
  actual Codex process startup separately resolves and validates the selected
  path in `quota::process`. Rust's Windows `Command` preserves its `.bat`
  launch behavior, so PATH shims do not need a new shell wrapper here.
- Release workflow review confirms the default-branch ancestry check, a
  `release` environment reference, package manifest/checksum validation, and
  pinned build-provenance action with `id-token` and `attestations` write
  permissions. YAML parsing passed for both workflows and Dependabot; a local
  static audit found all 10 action references pinned to 40-character SHAs.
  `actionlint` is not installed, and no workflow was run. Repository
  configuration must require reviewers and restrict the `release` environment
  to approved refs; artifact attestations must also be enabled for the repo or
  organization. The local checkout cannot verify those settings. Updaters
  validate the release digest from GitHub metadata, but do not verify the
  generated provenance; a verifier and pinned workflow/source identity policy
  are still needed before provenance can authorize updates.
- Runtime trust limitation: Unix candidates are checked for absolute path,
  file type, owner, execute/write/set-id modes, and ancestor write modes.
  Windows candidates are currently checked for canonical absolute regular
  paths only; ACL ownership/writability is not inspected, matching the
  existing quota process validator. No Windows cross-compile was run here.
- No quota tool is exposed in this worker. The latest coordinator snapshot was
  native remaining 58%/83% and egg 99%/84%, above the switch threshold.
- CI implementation commit: `2289db2` (`Validate Codex runtime inventory
  candidates`), authored by `h2q` with the Point co-author trailer. The branch
  is local only; next is coordinator review/consolidation. No push or workflow
  run was performed.
- Post-implementation handoff checkpoint commit: `092b5f1`.
- Runtime hardening now rejects Unix executables with group/world write or
  set-id bits. The first post-edit targeted run found an incorrect new test
  expectation: PATH discovery intentionally preserves the symlink path rather
  than returning its canonical target. The implementation behavior is kept;
  the test now asserts that public path and still exercises trust validation.
- After that correction, `cargo test --locked --offline -p emp-codex
  runtime_inventory` passed 8/8 unit tests and the isolated rustfmt check passed
  for all four runtime inventory Rust files.

## Current branch and scope

- Repository: `/home/fumo/codex_ws/agent_dev/EasyMultiProvider-rust`
- Branch: `remake4rust`
- Base HEAD: `5fa6a64a8caf9496a8bdef435d3c8a69a203f421`
- Issue: [EasyMultiProvider #14](https://github.com/Killow1998/EasyMultiProvider/issues/14)
- Active task: Phase A repair for oversized Codex rollout history reconstruction.
- Working tree contains uncommitted changes in:
  - `crates/emp-codex/src/history.rs`
  - `crates/emp-codex/src/history/records.rs`

## Completed repair

The Rust reader no longer loads the entire rollout with `fs::read`. The original
128 MiB whole-file rejection is removed.

The reader now performs bounded forward streaming:

1. Canonicalize the rollout path and verify that it remains inside the Codex home.
2. Open the rollout once and use the same file handle.
3. Capture `captured_end` from that handle.
4. Keep a hard scan ceiling of 2 GiB.
5. First pass scans metadata, thread identity, successful turns, response roles,
   model context, ordinals, and exact compaction matches.
6. Second pass streams records before the computed history boundary and rebuilds
   visible history.

New bounds:

- `MAX_ROLLOUT_LINE_BYTES = 64 MiB`
- `MAX_ROLLOUT_SCAN_BYTES = 2 GiB`

A single JSONL line above 64 MiB returns `history_record_too_large` instead of
reading it unbounded. A rollout above 2 GiB still returns `source_too_large`.

## Semantics preserved

- Thread identity conflict and mismatch are still rejected.
- Failed turns remain excluded from visible history.
- Incoming-turn history boundary remains respected.
- Paginated mode still requires an ordinal.
- Exact compaction lookup still rejects missing or duplicate ciphertext.
- Partial trailing JSONL data is ignored as an unfinished prefix.
- The file handle is used with a frozen prefix rather than relying on repeated
  path metadata reads.

## Tests performed

All of the following passed:

- `cargo test -p emp-codex history -- --nocapture`
- `EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider cargo test -p emp-app history`
- Same Python environment variables with `cargo test -p emp-app conversation`

Added regressions cover:

- Rollout larger than the old 128 MiB limit, using bounded streaming.
- Oversized JSONL line returns `history_record_too_large`.
- Partial trailing JSONL is ignored.
- Duplicate exact compaction ciphertext returns `compaction_identity_ambiguous`.
- Paginated rollout without ordinal returns `ordinal_missing`.

`cargo fmt` passed with the isolated Rust toolchain after formatting the touched files.

## Latest verification

The isolated Rust toolchain was used for the CI-equivalent strict check:

```bash
PATH=/home/fumo/codex_ws/agent_dev/.emp-rust-toolchain/rustup/toolchains/1.93.1-x86_64-unknown-linux-gnu/bin:$PATH cargo clippy --locked --workspace --all-targets -- -D warnings
```

This passed.

The full workspace test suite was then run with:

```bash
export EMP_PYTHON_INTEROP=/home/fumo/codex_ws/agent_dev/EasyMultiProvider/.venv/bin/python
export EMP_PYTHON_ORACLE_ROOT=/home/fumo/codex_ws/agent_dev/EasyMultiProvider
cargo test --locked --workspace --all-targets
```

It reached 111/112 tests passing. The sole failure was
`tests::performance_contract::responses_endpoint_records_schema3_full_request_tps_and_preserves_stream_timings`,
which failed a timing assertion under parallel load. When rerun alone, the same
test passed. This is not related to the history reconstruction changes, but a
clean full-suite rerun is still recommended before commit.

A complete real 523 MB rollout end-to-end request remains to be retested.

## Not included in Phase A

These remain for Phase B/C and must not be mixed into the current small repair:

- Reverse JSONL scanner and paginated reverse checkpoint selection.
- Rollout lineage, history base, and multi-segment forks.
- `ThreadRolledBack` reconstruction.
- `thread_history_1.sqlite` projection acceleration.
- Compressed rollout segment support.
- Python-side equivalent repair or explicit Rust-only production cutover.

## OMP development setup

OMP was installed successfully:

```text
/home/fumo/.local/bin/omp
version: v18.3.1
```

OMP is configured to use EMP's external `chuang` provider directly. Credentials
were copied from EMP's local encrypted vault into the private OMP model config;
do not print or commit the key.

Config locations:

- `~/.omp/agent/models.yml`
- `~/.omp/agent/config.yml`

The configured model is:

```text
chuang/gemma-3.1
```

It is assigned to both the default and vision roles. OMP reports image input as
supported. The model config declares:

```yaml
input: [text, image]
```

Minimal end-to-end check passed:

```text
prompt: Reply with exactly: OMP-EMP-OK
result: OMP-EMP-OK
```

Use OMP from the EMP Rust repository for the next development session:

```bash
cd /home/fumo/codex_ws/agent_dev/EasyMultiProvider-rust
/home/fumo/.local/bin/omp
```

## Immediate next actions

1. ~~Run strict Clippy using the isolated toolchain.~~ Done: `-D warnings` clean.
2. ~~Run the full workspace suite with the Python oracle variables set.~~
   Done: all crates pass; the only failure inside full-workspace parallel
   runs is the pre-existing `performance_contract` timing flake (passes
   standalone and in crate-scoped runs).
3. ~~Re-test the real 523 MB rollout through the Rust service.~~ Done:
   200 OK in 4.4 s, 9,945 messages, no opaque tokens leaked, 223 MiB RSS.
4. ~~Review the diff and commit Phase A once all CI-equivalent checks
   pass.~~ Done: `2218a10 Stream Codex rollouts with bounded history
   reconstruction`.
5. ~~Start Phase B: bounded reverse scan and Codex-style checkpoint
   replay.~~ Done: `581c789 Add reverse scan and checkpoint replay for
   paginated resumes`.

## Phase B result and known limits

- Reverse locate scans doubling windows from the tail (16 MiB steps,
  64 MiB cap); a self-contained newest compaction (window-numbered,
  opaque-free `replacement_history`) becomes the replay base.
- All 63 compactions in the real 523 MB rollout carry an opaque
  compaction item, so today's real Codex files always take the
  full-scan fallback (~4.4 s end-to-end). Base replay wins only for
  opaque-free checkpoints; keep it for the resume contract when Codex
  stops emitting opaque references.
- Remaining Phase B ideas (not started): SQLite `threads`/turn
  projection pushdown for anchor lookup, and streaming the suffix
  replay directly into the provider request instead of materializing
  the full visible vector.

## Follow-up investigation: 429 retry policy vs OMP (oh-my-pi)

OMP retry (extracted from the bundled `pi-ai` runtime):

- `retry.enabled=true`, `maxRetries=10`, `baseDelayMs=500` with
  exponential `2**attempt`, per-delay cap 8 s, ±25% jitter,
  `maxDelayMs=300 s`.
- `Retry-After` honored in both integer-seconds and HTTP-date forms;
  retry fires when `Retry-After` is present OR the error class is
  Transient (`>=500`, `408`, `429`, network/timeout regex) OR
  UsageLimit (`429`/`402`, `CONCURRENT_LIMIT`).
- `ContextOverflow` is never retried; `retry.waitForUsageReset` sleeps
  until `x-ratelimit-reset-*`; usage-aware fallback reserves 10%
  (`usageReservePct`); `retry.fallbackChains` provides ordered
  role/provider/model fallback chains (`modelFallback=true` default).

EMP (crates/emp-transport/src/failure.rs,
crates/emp-app/src/services/failures.rs): a single retry
(`attempt in 0..2`), only 429 with reason `rate_limited` (never on
`:free` routes) or 504, `Retry-After <= 5 s`, sleeps exactly
`Retry-After`; 429 quota/capacity/balance reasons are terminal; no
backoff curve; protocol fallback instead of model fallback. EMP is
stricter mid-stream: retries only before any output/tool activity.

Parity gaps, cheapest first:

1. Honor `Retry-After` up to ~300 s instead of rejecting above 5 s.
2. Add exponential backoff + jitter when `Retry-After` is absent
   (500 ms base, 8 s per-delay cap) for 429/504/5xx before output.
3. Treat 429 `upstream_capacity` as failover-to-next-candidate rather
   than terminal when more candidates exist.
4. Optionally raise the external retry budget for non-streamed
   requests from 1 retry to 2–3.

## Follow-up investigation: metrics and pricing vs OMP

- TTFT: EMP records `ttft_ms`, `generation_ms`, `duration_ms`,
  `tokens_per_second` (schema 3 diagnostics); OMP exports only
  `gen_ai.response.time_to_first_chunk` via OTel. EMP surface is
  richer; nothing to port.
- Cost: OMP computes `per_million_rate * tokens / 1e6` with a 2x-input
  rule for 1h cache writes; EMP
  (crates/emp-state/src/usage/pricing.rs) uses decimal per-token rates
  with tier suffixes (`_priority`, `_flex`), threshold tiers
  (`_above_Nk_tokens`), 5m/1h cache-write split, and reasoning-token
  rates, producing `cost_nanos`. EMP is strictly richer; OMP has no
  equivalent to port.
- Routing: OMP resolves roles plus `retry.fallbackChains` (model and
  provider wildcards); EMP routes via per-request candidates and
  protocol fallback. Role-based chains are the only missing concept.

## Follow-up session results (2026-09-25, later)

1. OMP-aligned 429/504 retry shipped (`52be217`): `Retry-After` honored
   to 300 s, exponential backoff (500 ms base, 8 s cap, ≤25% jitter)
   when absent, capacity 429s retried alongside rate limits, quota
   exhaustion still terminal, free routes still never retry. Budget is
   3 pre-output attempts; mid-stream stays single-failure.
2. Gemma 3.1 CoT: reasoning stays separated from `response.output_text`
   in both Python and Rust stacks (23 reasoning deltas + 1 answer delta
   verified live per fresh turn); protocol tests pin complete/stream/
   late-reasoning separation.
3. Responses WebSocket external execution: an external route inside the
   Responses-WS endpoint executes over the provider's HTTP/SSE
   transport (the fallback) and now races upstream events against a
   downstream DisconnectMonitor (`1758108`). Previously a stalled
   upstream kept the worker and its HTTP stream alive after the codex
   client vanished. Contract test drops the WS after `response.created`
   and asserts the SSE upstream observes the closed connection.
4. Perf flake fixed (`b099a38`): upstream deltas can coalesce in one
   socket read, so `generation_ms >= 100` was not deterministic; the
   contract now asserts `ttft >= 100ms`, `generation <= duration`, and
   `duration >= 200ms`.
5. Full workspace suite green (112 app-lib + all crates), strict Clippy
   `-D warnings` clean, `cargo fmt` applied.
6. Phase B replay regressions pinned (`2e2dc0d`): an eligible-looking
   paginated compaction whose replacement history contains an opaque
   compaction item is rejected as a reverse base (full scan owns that
   case), and a `thread_rolled_back` event after the checkpoint base
   truncates the replayed suffix exactly like the full scan.
   `cargo test -p emp-codex --lib` 20/20, `emp-app history` and
   `conversation` green, strict Clippy `-D warnings` clean, fmt applied.
   Real 523 MB rollout retested end-to-end through the Rust service:
   reverse locate ~2.4 s, opaque gate forces full-scan fallback, request
   replayed (55.7 MB projected request, 256 items) and served an upstream
   429 rate-limit response; the service stayed alive and healthy
   throughout. Host OOM pressure (7.6 GB RAM, swap full, `oom_kill`
   events in the user slice) killed earlier foreground test-server
   instances; run such retests from a detached (`setsid`) process.

## Follow-up session results (2026-09-26)

1. Unified replay rewrite (`d7e74e2`): `emp-codex/src/history.rs` replaced its
   four duplicated scan/replay loops (`scan_suffix`, the inline replay inside
   `snapshot_from_base`, `scan_rollout`, and the `read_rollout` closure) with
   one `walk_records` record walker, one shared scan observer, and one
   `replay_records` state machine seeded by a `ReplayFrame`. The whitebox unit
   test module (≈1400 lines) and the `force_full` strategy switch it existed to
   exercise were deleted; the differential guarantee now rests on the e2e
   contract suites plus the `emp-history` python oracle. Net −1481 lines.
   Verified: workspace 415 tests green (clippy `-D warnings` clean, fmt
   applied), 523 MB real rollout reads in ~3.0–3.6 s with identical item
   counts (10195) on both strategies before the switch was removed.
2. Ablation pass across modules (`9c41576`): deleted the dead
   `ResponsesValidationErrorKind` enum + accessor and `anthropic_error_kind`;
   single-sourced the protocol→field tables, part classifier, tool item and
   open-text event builders in emp-protocol; merged `wire_item` tool arms,
   deduped prepare openings, shared `CHECKPOINT_PREFIX`/`portable_encrypted`
   in emp-history; shared `read_http_head` and the frame length-prefix helper
   between websocket server/client, folded `SystemProxyCacheState`, collapsed
   `run_command` arms in emp-transport; folded the `quota_status_value`
   wrapper and restructured `read_lease` validation in emp-app/integration.
   Net −175 lines over 12 files. One oracle regression (suppressed reasoning
   blocks consuming output indexes after the block-state helper merged the
   ordering push) was caught by the anthropic python oracle and fixed in the
   same commit — the oracle suites are the load-bearing contract.
   Workspace 415 green, clippy clean, fmt applied.

## STATE worker progress (2026-09-26)

- Point is taking over the existing `/home/fumo/codex_ws/agent_dev/emp-sec-state`
  worktree on `sec/state` after HTTP commit `6cdbae7`; that separate commit has
  not been integrated here.
- Preserving the existing migration v2, origin-bound secret, and bounded-file
  drafts. Read-only review is in progress; no STATE tests or data operations
  have run in this worktree yet. Python port 4200 remains untouched.
- Next: trace Python and Rust quota-history ownership across credential delete
  and reimport, inspect the live-store identity evidence read-only, then add
  focused retention and identity-separation behavior before changing data.
