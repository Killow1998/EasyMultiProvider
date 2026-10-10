# Call outcomes and quota observations

Call recording and reporting have separate owners. Request handlers feed terminal
facts into `RequestOutcome`; activity and storage use `emp_state::usage::call_state`.
`usage/ledger/calls.rs` records those facts and delivery. Its `report` module owns
filtered read queries, pagination and aggregates.

Reports use `contracts/call-outcomes.sql`. This read projection also classifies
older records using stored delivery evidence, without changing usage or guessing
whether a later request retried the original one. The shadow chart reader uses
the same projection when filtering by outcome.

Request success means completed / (completed + failed). Explicit interruption,
disconnect, history resend and unrecorded outcomes remain separate. This measures
requests, not final conversation success. Upstream completion and delivery to
Codex are independent facts. Native upstream wire errors remain unchanged; their
receipt source is upstream even if their payload claims otherwise.

`call-outcomes.js` owns the percentage, failure grouping and child dialog lifecycle.
`call-reports.js` owns report views and queries. The child preserves the parent
filters and closes independently with Escape. Legacy backends without classified
samples display a completion ratio.

Quota measurements and failed observations are stored separately. A failed check
records its phase, duration, status and categorized cause; it never creates a
quota value. Trend gaps show recorded failures. Older gaps without diagnostics
remain unexplained. Only measured points contribute to quota trends.

Validate changes at their boundary: report integration tests for classification
and filtering, quota store tests for failed observations, native endpoint tests
for receipts and unchanged wire behavior, and browser acceptance for dialog and
preview isolation. See [shadow frontend acceptance](../scripts/shadow_frontend/README.md).

The Statistics window shares its time/service/model/session/outcome filters across
performance and accounting. `/api/usage?series=true` groups the reconciled ledger
into at most 48 time buckets (minimum 60 seconds), with the same rows feeding
Token/USD totals and service/model chart colors. Receipt filters apply only to
accounting rows linked to matching calls; unmatched history is not guessed.

Official and shadow pages share presentation modules in `crates/emp-app/web/`.
The shadow server adds only its read-only boundary, local drafts and demos.
The official page defaults to bars; Settings switches to mirrored quota rings.
