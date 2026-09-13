# Usage and API-equivalent estimates

Open **Usage & estimates** beside **Performance & health**. Choose Today,
Last 7 days, Last 30 days, All history, or a custom start/end in the browser's local time.
Filter Native, Other Subscription, External Provider, or Unknown source. The table separates
accounts/providers and real upstream model IDs. Select a daily bar to inspect
its hours. Idle hours have no usage bars.

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
its confirmed account and rate. Two identical real requests still count twice.
A turn with unmatched history and live observations shows an overlap warning;
no timestamp-only guesses are made. Old clients without turn metadata cannot
be reliably reconciled with live observations; totals may contain overlap.

Historical `model_provider=openai` alone does not prove a Native account. Bare
GPT/Codex models on that provider are classified as Native. Other routes are
compared with the current route configuration; their category is an inference.
Historical owners remain `history:<route prefix>` rather than the currently
logged-in or imported account. Unmapped models remain Unknown source. Changed
or reused prefixes cannot establish the original account identity.

Token counts come from upstream usage, not character estimates. Input includes
cached input and cache writes; output includes reasoning. These subsets are
displayed separately but not added twice. Responses, Chat Completions (including
DeepSeek cache fields) and Anthropic Messages normalize to this convention.
Gemini gateways must include thoughts in OpenAI `completion_tokens`, with the
thought count also reported as `reasoning_tokens`. NA2H's adapter and local
metering follow this convention; both sides need the corrected gateway version.
Reported usage on incomplete or failed responses still counts. Missing usage
is shown as missing, not a successful zero-token call. Pre-request handshake
failures followed by HTTP fallback are excluded. Repeated observations of the
same response ID within a route/account/model count once.

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
