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

## Reasoning effort and speed tier

Expand a record in **Statistics > Calls** to see the selected reasoning effort,
upstream-reported effort, requested tier and upstream-reported tier separately.
For example, an Ultra selection can report `xhigh`; a Fast request that reports
Standard belongs to the Standard performance group. Model groups retain both
effort values, so different reported efforts are not averaged together.

The current turn's effort takes precedence over the initial request setting.
Upstream effort is captured before protocol projection removes reasoning fields.
Tier confirmation does not require token usage. An absent tier report displays
**Tier unconfirmed**, including older records that lack confirmation provenance.
Old receipts are not rewritten or assigned a tier using today's settings.
API-equivalent pricing still uses its separate fallback policy described below.

With **Automatic reasoning effort** enabled, call details retain the caller's
configured effort separately from EMP's selected effort, classifier model,
classification time and fallback reason. The selected effort remains distinct
from an upstream-reported effort. Classifier requests have their own
`effort_classification` usage records linked to the same conversation; their
tokens are included in the bill. Main-request latency includes time spent
waiting for classification. Reusing a cached decision adds no new classifier
request or classification wait.

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

Amounts are API-equivalent estimates, not subscription charges or invoices.
No known price displays **—**; a known zero rate displays **US$0.0000**. An asterisk
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
