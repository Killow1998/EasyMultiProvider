# Usage accounting verification

## Regressions

`tests/test_usage_history.py` exercises repeated cumulative notifications,
restart/append checkpoints, archive and fork copies, incomplete final lines,
file replacement, history/live matching, cumulative fork baselines, unknown
source attribution, inconsistent gateway totals, schema backup and transaction
rollback. Fixtures contain no real conversation content or credentials.

`tests/test_usage_ledger.py` verifies normalized token subsets, cache-write
lifetimes, context/service price tiers, pending-price refresh, concurrent writes,
response-ID deduplication, persistence and management API authentication.

## Ablation and simplification

Three temporary in-memory ablations were run against the same fixture tests:

| Disabled behavior | Observed regression |
| --- | --- |
| File checkpoints | Unchanged files were reread instead of skipped. |
| Repeated cumulative guard | Repeated usage notifications increased the total. |
| History/live matching | The same call was counted in both data sources. |

All three baseline tests passed; each ablation failed the relevant assertion.
These mechanisms were retained. The ablations did not modify production data
or leave alternate implementations in the codebase.

The scanner reuses Codex's existing turn-metadata reader and EMP's token/price
normalization. It has one worker and one checkpoint store, without a scheduler
framework, parser registry, or separate account-identity abstraction. Queries
reconcile once into a temporary numeric projection shared by chart/table
aggregation, instead of repeatedly sorting all historical events.

## User-facing verification

The actual browser was used with an isolated local configuration to verify
Chinese/English labels, all-history selection, account-category filtering,
day-to-hour drilldown, scan/refresh actions and the local-time range display.
The browser reported no JavaScript errors during those flows.

API-equivalent amounts are estimates. Historical prices and account identities
cannot be reconstructed when the original records lack them. Matching requires
Codex turn metadata and compatible usage fields; unmatched overlap is reported,
not silently declared exact. Cross-platform package builds and updater smoke
tests run in the release workflow; they do not replace real upstream testing.
