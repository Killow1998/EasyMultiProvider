# Usage and API-equivalent estimates

Open **Statistics** and choose the period or a custom start/end in the browser's
local time. Common service, source-type and model filters apply across Overview,
Services, Models, Session bills and Calls. Unknown or removed services retain
their recorded attribution. Historical records are not assigned to today's
account merely because a route prefix matches.

Click a service badge to open its information and settings. Its lifetime usage
summary combines rows with the same upstream model ID across dates, service
tiers and Native/Subscription categories belonging to that verified account.
Provider summaries include only that provider's records. Input, output and
priced costs are summed; missing usage remains missing and known zero-cost
usage remains zero. Use **Statistics** for date and category breakdowns.

## Official provider balances

The service summary's Credits position displays DeepSeek and Moonshot/Kimi balances.
DeepSeek returns its account balance in the reported CNY or USD currencies;
Moonshot uses CNY on its China endpoint and USD on its international endpoint.
Their information pages link to the official billing console. Other services
do not display a balance section or receive balance requests. EMP does not query
Key spending limits, request an organization Admin key or use private dashboard APIs.

Balance queries run separately from inference. Configuration reads use the
in-memory snapshot; opening the WebUI refreshes observations older than one
minute. The service refresh button explicitly refreshes its balance. Query
failures retain an error state rather than turning into a measured zero.
Changing a provider endpoint or credential selects a different cache entry.
Self-hosted endpoints do not receive speculative balance requests.

## CPA subscription quota

CPA services exposing the observer's `GET /v1/quota` use the existing service Key.
The observer scopes account visibility; EMP does not request a management Key.
Service rows show the 5h/7d remaining percentage and each window's nearest future
reset. Hover or focus the quota to see anonymous accounts, their 5h/7d quotas,
resets and stale/disabled/unknown states.

Each summary window uses the arithmetic mean of enabled, fresh, known account
percentages; valid zero is included. Disabled, stale and missing values do not
enter the mean or the nearest reset. No known values means unknown.
These subscription percentages are separate from token usage and money balances.

Queries read the observer snapshot separately from inference and cache it for
five minutes. Visible pages check cache age once per minute; hidden pages stop
these checks. Service Refresh and Refresh quota force a snapshot read, including
after an unsupported endpoint becomes available. Reads do
not generate inference records. Endpoint or Key changes select a new cache entry.
An unsupported quota endpoint leaves the service's model connection unchanged.

### CPA operator integration

In EMP, choose **Add service → Claude Code / CPA → CPA**, then enter the CPA Base
URL and its service Key. The question mark beside Base URL links to this contract.
The Key must authorize both model calls and quota reads. EMP uses no separate
management Key or account-list endpoint.

The CPA service or its observer must expose `GET /v1/quota` and return HTTP 200
with `Content-Type: application/json`. An API compatible with Anthropic Messages
does not automatically include this quota endpoint; the operator implements it
using its own account-quota observations.

EMP strips one trailing `/v1` and appends `/v1/quota`, preserving any earlier path:

| CPA Base URL | Quota URL |
|---|---|
| `https://cpa.example.com` | `https://cpa.example.com/v1/quota` |
| `https://cpa.example.com/cpa/v1` | `https://cpa.example.com/cpa/v1/quota` |

```sh
curl -H "Authorization: Bearer $CPA_API_KEY" \
  -H "Accept: application/json" \
  https://cpa.example.com/cpa/v1/quota
```

Example snapshot; the operator supplies current observations and reset times:

```json
{
  "scope": "shared_pool",
  "provider": "claude",
  "checked_at": "2026-11-07T12:00:00Z",
  "accounts": [
    {
      "id": "account-a",
      "enabled": true,
      "stale": false,
      "status": "ok",
      "as_of": "2026-11-07T12:00:00Z",
      "windows": {
        "five_hour": {"remaining_percent": 80, "resets_at": "2026-11-07T15:00:00Z"},
        "seven_day": {"remaining_percent": 40, "resets_at": "2026-11-12T12:00:00Z"}
      }
    },
    {
      "id": "account-b",
      "enabled": true,
      "stale": false,
      "status": "ok",
      "as_of": "2026-11-07T12:00:00Z",
      "windows": {
        "five_hour": {"remaining_percent": 40, "resets_at": "2026-11-07T14:00:00Z"},
        "seven_day": {"remaining_percent": 0, "resets_at": "2026-11-10T12:00:00Z"}
      }
    }
  ]
}
```

| Field | Contract |
|---|---|
| `scope`, `provider` | Required strings, exactly `shared_pool` and `claude`. |
| `accounts` | Required array; return only accounts visible to the calling service Key. |
| Account `id` | Required stable anonymous string; EMP uses it to join trend history. |
| `enabled`, `stale` | Required booleans. Disabled or stale accounts do not enter the summary. |
| `status` | Required string, such as `ok`, `error` or `disabled`; report freshness through `stale`. |
| `windows` | Required object. `five_hour` and `seven_day` are the displayed windows; use `{}` if unknown. |
| `remaining_percent` | Number from 0 to 100, or `null` if unknown. Zero is an observed exhausted window. |
| `resets_at` | RFC 3339 timestamp with a timezone, or `null` if unknown. |
| `checked_at`, `as_of` | Snapshot/account timestamps or `null`; both may be omitted. |

The example displays 40% for 5h and 20% for 7d. A known exhausted 7d window makes
that account's contribution to the usable 5h pool zero; the individual account's
5h observation stays unchanged. EMP averages these known percentages from
enabled, fresh accounts, rounds the displayed percentage upward, and uses
the nearest future reset for each window. Raw trend samples retain fractional
precision. The selected pool or account shows its 5h, 7d and 30d series together
with fixed colors. Missing values stay unknown; do not substitute zero for failed reads.
Return anonymous IDs and quota observations, without account credentials.

Serve a cached observer snapshot rather than querying every account during each
EMP read. EMP reads at most 2 MiB and waits up to five seconds. Its background
sampler refreshes every five minutes; manual refresh reads immediately. Trend
queries use EMP's local fifteen-day history, so the CPA needs no history API.
HTTP 404/405 hides the service's quota Refresh, Trend and both meters while
retaining model calls; HTTP 401/403 means
the service Key lacks access. After enabling the endpoint, use the global
Refresh quota in EMP to read it immediately and restore those controls.

## History resend records

**History resend** records an instruction to send full context, not a model failure.
When a matching follow-up completes, its request ID is retained on the original
record and Calls shows the completed recovery. This uses the existing scoped
diagnostic correlation; older unlinked records retain their observed status.
The resend record does not become another successful inference or duplicate usage.
Current Codex clients send full input directly after stateless external WebSocket
completions, avoiding the normal tool-loop rejection and replay.

## Reported model and output usage

Gemini `generateContent` responses retain the upstream `modelVersion` in call
records, separately from the selected route and requested upstream model. Output
usage includes reported answer and thought tokens. A thought-only `MAX_TOKENS`
response still contributes its reported output; explicit zero stays zero, while
missing answer and thought counts remain unknown. This applies to complete and
streaming responses.

## Reasoning effort and speed tier

Expand a record in **Statistics > Calls** to see the selected reasoning effort,
upstream-reported effort, requested tier and upstream-reported tier separately.
For example, an Ultra selection can report `xhigh`; a Fast request that reports
Standard belongs to the Standard performance group. Model groups retain both
effort values, so different reported efforts are not averaged together.

The current turn's effort takes precedence over the initial request setting.
Background memory receipts use the effort forwarded by EMP, including an override
that differs from the original Codex turn metadata.
Upstream effort is captured before protocol projection removes reasoning fields.
Tier confirmation does not require token usage. An absent tier report displays
**Tier unconfirmed**, including older records that lack confirmation provenance.
Old receipts are not rewritten or assigned a tier using today's settings.
API-equivalent pricing still uses its separate fallback policy described below.

## Session bills

Choose **Statistics > Session bills** to search session names or IDs and sort by
recent activity, tokens or estimated USD. The list is paginated; expanding a
conversation loads its model breakdown and task/branch breakdown for the same
period and filters. Identical upstream model IDs are summed across its services.
Titles come from the local Codex index or read-only threads database; a confirmed
main conversation without a title displays a shortened ID.

One bill belongs to one main conversation, including its linked Auto Review,
Sidechat and nested subagents. Expanding it retains each component's type, tokens
and estimate. Ownership comes from recorded request parents and Codex's read-only
thread metadata and spawn edges; ephemeral children do not need a saved rollout.
Cycles, conflicting parents and unavailable roots remain unassigned, rather than
becoming unnamed user conversations. The shared accounting selection reconciles
live and historical observations before aggregation. Cache
reads/writes and reasoning remain subsets of input/output, not extra tokens to
add to the total.

Token and USD displays use lowercase **k/m/b** for thousands, millions and
billions; hover to see the full recorded value. This affects presentation only.
Statistics use six ten-minute buckets for 1h, 24 hourly buckets for 1d, seven
daily buckets for 7d and 30 daily buckets for 30d. Usage and performance charts
read the same bucket width from the ledger; regrouping does not change recorded
totals.

Statistics stream actual row-processing progress through the selected call
records and their aggregates. Only a percentage and progress bar appear beside
Query/Refresh; changing filters or closing the window cancels the older stream.
Call reports reuse parsed facts and model aggregates within each query; record
pricing is looked up after pagination.

Amounts are API-equivalent estimates, not subscription charges or invoices.
No known price displays **—**; a known zero rate displays **US$0**. An asterisk
marks a known subtotal when prices or usage fields are missing. Missing output
or cache data is not replaced with a measured zero. The existing ledger may
store a zero placeholder for an unknown price; bills check its pricing issue as
well as the amount so that placeholder cannot be mistaken for a free model.

Session links form a small index beside the existing usage rows. Old call receipts
populate their verified links on startup. Existing rollout checkpoints receive
one read-only rescan to populate historical links, with indexing shown in the bill
view; event IDs, recorded totals and
frozen rates remain unchanged, and later scans are incremental again. Conversation
ownership is resolved from existing metadata at query time, without a new rollout
rescan. Inherited copies of one event count once if all owners resolve to the same
main conversation; disagreeing copies remain unassigned. Their known usage stays
in the overall total and is shown separately instead of being charged arbitrarily.

The authenticated `/api/usage?sessions=true` endpoint accepts the existing period,
service/source, model and session filters plus `q`, `sort=recent|tokens|cost`,
`offset` and `limit` (default 20, maximum 100). Model details use the existing
`series=true&conversation=true&session=MAIN_ID` projection, including `components`.
Omitting `conversation=true` retains the individual-thread projection. Both paths
share selection and aggregation; there is no second accounting or pricing engine.
An authenticated caller may select an already observed opaque `owner` directly,
instead of resolving the current `account`/`account_id` credential. These choices
are mutually exclusive; owner filters are bounded and cannot contain control
characters. Shadow uses this to calculate account-scoped snapshot reports without
copying credentials or guessing account identity from historical labels.

## What is counted

EMP reads `sessions` and `archived_sessions` under the active `CODEX_HOME`,
including usage recorded before installation and after restoring native mode.
The first scan runs in the background; subsequent scans check for appended
records every minute. **Scan history** requests an immediate incremental pass.
The progress indicator shows completion and read failures. Files on another
computer and cloud-only conversations are not included.

Live HTTP, SSE and native WebSocket observations complement the local rollouts.
Records use completion time, with an inclusive start and exclusive end. The
bounded diagnostic logs are not treated as a complete accounting history.

Background memory extraction and consolidation count toward the selected
subscription's usage, alongside ordinary responses. On initialization, EMP
restores missing memory entries from saved call receipts, retaining the original
account, token counts and completion time. This reads existing records without
calling the model again. Saved response identities and the normal history/live
reconciliation prevent duplicate totals after a restart or history scan. Restored
entries use the available price catalog; unreported token counts remain unknown.
Calls without a response ID reuse their request identity so recording the call
receipt and reopening the ledger cannot add another copy.

Both memory-model help bubbles compare a fixed November 4–5, 2026 Pacific
24-hour sample against ordinary conversation/coding: input/output token ratios,
input-cache hit rates and API-equivalent USD ratios. Both memory stages use
GPT-6.1 Sol; ordinary calls mainly use Sol with a little GPT-6 Luna. Automatic
reviews and calls without token reports are excluded. USD uses Standard and
cache-read unit rates without a long-context surcharge. Subscription allowance
remains a server-side measurement. Users can select each stage's model and effort
independently; Codex 0.167 defaults to Low extraction and Medium consolidation.
Effort changes apply to subsequent memory requests. Unset model hints
follow the trusted runtime: Codex 0.167 uses GPT-6 Luna for extraction and
GPT-6.1 Terra for consolidation. Explicit original choices remain unchanged.

Repeated cumulative notifications are ignored. Event identities deduplicate
archive copies and inherited fork turns. A checkpoint includes the file identity,
byte offset, prefix and tail digests, model/turn metadata and numeric counters;
partial final lines are retried later. A replaced file is rescanned; already
indexed usage is retained if the source file disappears. A cumulative-only fork
baseline is not counted as new usage. Cumulative-only deltas are kept unpriced
because their individual requests and context tiers cannot be recovered.

History/live overlap is matched one-to-one using the Codex turn ID, requested
model, input, output and cached counts. The live observation wins, preserving
its recorded account attribution and rate. Two identical real requests still count twice.
A turn with unmatched history and live observations shows an overlap warning;
no timestamp-only guesses are made. Old clients without turn metadata cannot
be reliably reconciled with live observations; totals may contain overlap.

Historical `model_provider=openai` alone does not prove a Native account. Bare
GPT/Codex models on that provider are classified as Native. Other routes are
compared with the current route configuration; their category is an inference.
Historical owners remain `history:<route prefix>` rather than the currently
logged-in or imported account. Unmapped models remain Unknown source. Changed
or reused prefixes cannot establish the original account identity.

New live ChatGPT observations use an opaque fingerprint of the account ID in
the selected upstream authentication headers. Local account IDs, prefixes,
display names and token rotation do not change that identity. Native and Other
Subscription remain separate categories even when they use the same account.
If no account ID is available, the selected credential is fingerprinted and
shown as identity unconfirmed; credential rotation can split those groups.
Older live rows are retained without reassignment to the current login. External
usage remains grouped by provider source; an API key does not establish the
identity of an account behind a gateway or account pool.

Token counts come from upstream usage, not character estimates. Input includes
cached input and cache writes; output includes reasoning. These subsets are
displayed separately but not added twice. Responses, Chat Completions (including
DeepSeek cache fields) and Anthropic Messages normalize to this convention.
Gemini gateways must include thoughts in OpenAI `completion_tokens`, with the
thought count also reported as `reasoning_tokens`. The gateway's adapter and local
metering follow this convention; both sides need the corrected gateway version.
Reported usage on incomplete or failed responses still counts. Missing usage
is shown as missing, not a successful zero-token call. Pre-request handshake
failures followed by HTTP fallback are excluded. Repeated observations of the
same response ID within a route/account/model count once.

Steering is recorded as an interrupted call, separately from provider failure
and client disconnection. Control frames do not create extra model-call records.
Unreported usage on cancelled inference remains missing.

## Prices

EMP downloads the public
[LiteLLM price catalog](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)
at startup if there is no fresh cached copy, then every 24 hours while running.
Download failures keep the last valid catalog, show a warning, and retry after
an hour. No usage, account information or conversation content is uploaded.
The catalog is a maintained community source and can lag a provider's official
price changes; daily checks do not guarantee an invoice-exact result.

Models must match an exact upstream ID, optionally with a known vendor prefix.
Display names and approximate model-name matches are never used for prices.
Unknown models remain unpriced. The total shows the number of priced requests
and an asterisk when only a subtotal is available.

The estimate uses USD token rates, including reported cache reads/writes,
Anthropic cache-write lifetimes, known context-length tiers and service tiers.
An upstream-reported service tier takes precedence over the requested tier;
otherwise the requested tier is used, with standard rates when none is set.
Missing rates or cache breakdowns remain unpriced. Reasoning is already part
of output; a distinct reasoning rate, when supplied by the catalog, replaces
the output rate for that subset. Long-context rates apply to each request,
not to aggregated daily tokens.

Imported history uses prices available at scan time, not historical invoices.
A missing historical service tier is estimated at standard rates. Legacy
records whose total or reasoning fields contradict their input/output remain
unpriced; EMP does not silently repair gateway-reported token counts.

Each priced record freezes the applied rates, catalog revision and fetch time.
A later catalog update can fill previously missing prices but does not change
already priced history. Arithmetic uses Decimal and stores integer billionths
of a USD dollar; presentation rounds only after aggregation.

This is a token-cost comparison, not a subscription bill or reseller invoice.
It excludes tool calls, cache storage, taxes, regional surcharges and reseller
markups. It does not prove that EMP changes cache hit rates versus native use.

## Local storage

`state/usage.sqlite3` and `state/api_prices.json` live beside the EMP config.
The ledger survives restarts and does not expire with the diagnostic ring or
15-day quota trend. It stores numeric counts, route identifiers and rates;
no prompts, completions, credentials or conversation histories are stored.
An accounting write failure is logged and shown in the usage view without
failing the model request. The UI/API requires the existing management session.

Existing accounts, config and quota databases are not migrated. An older
nonempty usage database is backed up as `usage.pre-v011.sqlite3` before adding
the history index columns; SQLite commits schema and checkpoint changes
transactionally. The original `.codex` files are never modified. Usage is local
installation history and is not included in `.emp` configuration exports.
For an offline backup, exit EMP cleanly and copy the usage database and price
cache. An older EMP build ignores these additional files on rollback.

## References

- [Tokscale pricing and token normalization](https://github.com/junhoyeo/tokscale)
- [OpenAI reasoning token usage](https://developers.openai.com/api/docs/guides/reasoning)
- [OpenAI Astra pricing and long-context tiers](https://developers.openai.com/api/docs/models/gpt-6-astra)
- [Anthropic cache creation and read usage](https://platform.claude.com/docs/en/api/admin/usage_report)
- [Gemini prompt, candidate and thought token definitions](https://ai.google.dev/api/generate-content#UsageMetadata)

EMP parses Codex rollouts and its own forwarding observations directly. It
does not require the Tokscale CLI or its Rust/Node runtime.
