# Usage accounting verification

## Current contracts

Run `scripts/shadow_frontend/e2e_request_modes.py --emp /path/to/EMP --output
/path/to/result.json` against a candidate binary. It sends synthetic requests
through Native, imported subscriptions, external Responses and Chat Completions,
then opens the actual bilingual statistics UI. It covers HTTP, SSE and Responses
WebSocket, requested versus reported speed tiers, missing usage, current-turn
effort and Ultra versus reported `xhigh`. It checks persisted call details,
model-group totals and upstream request counts. It uses disposable credentials
and loopback endpoints, with no real provider calls. Its hand-written requests
declare client version 0.167.0; it does not launch or require a Codex runtime.

The Rust accounting contracts are:

- `crates/emp-state/tests/call_reports.rs`: durable call details, history-recovery
  linkage without duplicate usage, scan progress/cancellation, tier and effort
  persistence, and exclusion of invalid historical TPS from generation averages.
- `crates/emp-state/tests/account_usage.rs`: account ownership, periods and start
  times; memory receipts reconciled once with rollout history; chart totals and
  source filters; historical identity requiring a unique matching turn and route.
- `crates/emp-app/src/tests/gateway_contract.rs`: device attribution, shared-key
  callers and invalid report periods at the HTTP boundary.
- `scripts/shadow_frontend/e2e_native_local_key.py`: changing Native credentials
  retains account data and does not close a newer dialog when a save finishes.

## Data flow

`crates/emp-state/src/usage/ledger.rs` owns persisted usage. Its report modules
project calls, accounts, sessions and chart series from that ledger.
`crates/emp-codex/src/usage_history.rs` reads Codex history and reconciles it with
live receipts. Checkpoints skip unchanged files. Account identity and compatible
turn metadata determine whether historical and live records refer to one call.

Reported zero and missing usage remain distinct. API-equivalent amounts are
estimates; unpriced models do not become known zero-cost calls. The UI identifies
partial pricing. Historical identities or prices absent from the original
records cannot be reconstructed from token totals alone.

The release workflow checks package builds and updater startup. Those checks
complement the request/accounting contracts; real provider behavior is tested
separately with explicit account authorization.
