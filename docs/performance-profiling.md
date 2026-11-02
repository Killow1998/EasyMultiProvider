# Local performance profiling

EMP has optional [hotpath-rs](https://github.com/pawurb/hotpath-rs) instrumentation.
Normal builds do not enable it. Profiling does not change routing, retry policy,
or model selection. Function reports contain timing/allocation statistics, not
request contents or credentials.

## Reproduce

Install the local report tools once:

```sh
cargo install hotpath --version 0.28.3 --locked --features utils
```

Build the opt-in workload with one compilation job. Use the same profile before
and after a change; do not compare debug measurements with release measurements.

```sh
CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_LTO=false \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
  cargo test -p emp-app --lib --release --features hotpath \
  isolated_hotpath_workload --no-run
```

Pass the test executable printed by Cargo to the runner:

```sh
python3 scripts/profile_hotpath.py /path/to/emp_app-test-executable /path/to/new-report-directory
```

The runner executes three fresh processes, each with temporary HOME, CODEX_HOME
and XDG paths. The workload creates its own configuration, encrypted synthetic
credentials, catalog and loopback upstream. Listeners use ephemeral ports. It
does not activate integration or call real models. Temporary state is removed
after each process exits; reports remain in the chosen directory. The hotpath
metrics listener is disabled for these runs.

For allocation measurements, rebuild with `--features hotpath-alloc` and use a
new report directory. Allocation instrumentation changes timing: compare timing
reports with timing reports and allocation reports with allocation reports.
The opt-in workload is ignored by normal test runs.

## What is measured

- Catalog/ETag generation with 40 native models and four synthetic accounts.
- Small text SSE events and 16 KiB tool-argument SSE events.
- Request-byte accounting for approximately 1 MiB of text.
- Real HTTP forwarding for native complete responses and external Chat
  Completions SSE, with short and long inputs.
- HTTP client acquisition during those requests.

`timing-*.json` contains scenario latency percentiles; `hotpath-*.json` contains
function reports. The HTTP scenarios include the fake upstream and local
scheduling, so they do not establish real-model first-token latency or token
generation speed. The workload asserts successful terminal responses, preserved
unknown native fields, and the expected upstream request count.

## Design and interpretation

The [architecture assessment](architecture.md) covers application ownership and
module interfaces separately from the measured hot paths below.

Use [Codebase Design](https://github.com/mattpocock/skills/tree/main/skills/engineering/codebase-design)
as a design reference: keep caller interfaces small, consolidate knowledge that
would otherwise be repeated, and test observable behavior through those interfaces.
An extra abstraction should serve actual callers rather than anticipated ones.

Optimize measured work first. A catalog cache must account for configuration,
credential identity and externally updated Codex catalogs; replacing reads with
a stale snapshot is not an acceptable speed improvement. Preserve byte-level
wire contracts when changing serialization and count semantics.

## Published 0.13.23 process resources (2026-11-01 UTC)

Two runs used the published Linux x86_64 binary on an i5-10310U (four cores,
eight logical CPUs), with isolated configuration and synthetic loopback
providers. Each client completed three turns. Short input was 256 KiB; long
input was 16 MiB of patterned text and varied synthetic data. RSS/PSS and
process CPU were sampled every 50 ms. The upstream added approximately 1.3 s
per turn. No real model calls were made.

CPU below is the average fraction of the eight-thread machine. RAM is peak
RSS of EMP alone; it excludes the fixture, browser and Codex processes.

| Workload | CPU | Peak RSS |
| --- | ---: | ---: |
| Fresh instance, 10-second idle sample | Below 0.1% | 22 MiB |
| One short WebSocket client | 0.10% | 26 MiB |
| Four short WebSocket clients | 0.35–0.38% | 32 MiB |
| One long WebSocket client | 0.67–0.75% | 135–151 MiB |
| Four long WebSocket clients | 2.61–2.75% | 488–519 MiB |
| Eight long WebSocket clients | 5.06–5.25% | 1,016 MiB |
| Four long HTTP/SSE clients | 12.96–13.49% | 387–404 MiB |

WebSocket clients sent one full history followed by two incremental requests;
HTTP/SSE clients resent the full history each turn. The table measures those
workflows, rather than an equal-payload comparison of transport protocols.
After the eight WebSocket clients disconnected, RSS was approximately 360 MiB
at the three-second sample. The two complete runs passed 132 response completions.

An already-used instance with its existing data and connections used about
432 MiB RSS and 1.08% machine CPU over a separate quiet 25-second observation.
This and the fresh-instance baseline describe different lifecycle states.
The buffer-ownership comparison below follows up on the large-request memory
cost without restricting session length or concurrency.

## Long-history buffer ownership (2026-11-01 UTC)

The local candidate removes three redundant copies: complete WebSocket frames
transfer their existing buffer, incoming request JSON is parsed once and its
wire text is released before routing, and native projection skips the input
field that it would immediately replace. Fragmentation, control frames, input
limits, native extension fields and continuation IDs retain their contracts.

Both variants used the same optimized release profile, with LTO disabled and
16 codegen units, without profiling instrumentation. The Rust baseline was
0.13.23 source at `19959a4e`; the candidate added only those ownership changes.
The frontend and synthetic fixtures were identical. Each variant ran the full
matrix twice, using fresh instances. Builds and other tests finished before
resource measurements started.

The following ranges cover both runs. Peak RSS takes the larger of the 50 ms
samples and the kernel's `VmHWM`, so a short allocation peak is still counted.
CPU is the average fraction of the same eight-thread machine.

| Workload | Peak RSS before / after | CPU before / after |
| --- | ---: | ---: |
| One short WebSocket client | 29 / 28 MiB | 0.10% / 0.10–0.16% |
| Four short WebSocket clients | 35 / 32–33 MiB | 0.38–0.47% / 0.54–0.57% |
| One long WebSocket client | 153–154 / 106 MiB | 0.64–0.77% / 0.67–0.68% |
| Four long WebSocket clients | 490–522 / 283–339 MiB | 2.74–2.87% / 2.03–2.52% |
| Eight long WebSocket clients | 971–1,034 / 603–632 MiB | 4.34–4.95% / 4.18–4.21% |
| Four long HTTP/SSE clients | 417–418 / 311–317 MiB | 9.16–10.05% / 8.12–8.87% |

Mean peak RSS fell 38.6% for four long WebSocket clients and 38.4% for eight.
Three seconds after all eight clients disconnected, RSS was 300–347 MiB,
compared with 410–411 MiB before. Fresh-instance idle RSS remained about 25 MiB.
The short-input elapsed-time ranges overlapped; their CPU samples increased
by 0–0.06 CPU seconds per case. The resource saving is concentrated in long
histories rather than every request size.

All 264 responses completed. The fake upstream verified the SHA-256 of each
full history and the response IDs of incremental continuations. The protocol,
transport and Native API suites passed 131 existing checks, including fragmented
frames, compressed frames, cancellation, interruptions, tool identities and
twenty-turn history continuation for native and imported accounts.
Strict Clippy and formatting checks passed. No real models were called and the
running EMP instance was not replaced or restarted.

## Large-history allocation lifecycle (2026-11-01 UTC)

A follow-up measured repeated requests with connections left open. The baseline
already included the three buffer-ownership changes above. The final candidate
also transfers coalesced receive buffers, writes masked upstream WebSocket
frames through a reusable 64 KiB buffer, and transfers Claude CLI stdin to its
writer instead of retaining a second owner throughout generation.

On GNU/Linux, startup fixes glibc's mmap threshold at 1 MiB before threads start.
Large buffers can then be unmapped on free. A lifecycle-owned worker checks
model activity every two seconds and calls `malloc_trim(0)` when idle, returning
free pages inside arenas left by many small JSON allocations. Shutdown wakes
the worker immediately. These platform calls add no dependency or user setting;
the buffer-ownership changes apply on every platform.

Both binaries used the same release profile as the ownership comparison. Each
long case repeated a full-history request and two continuations three times.
Clients remained connected during a two-second rest after each cycle. HTTP/SSE
resent the full history each turn. The structured case contained 8,192 messages
of 2 KiB each; its existing prefill path issued 24 additional `generate:false`
requests. The fixture verified the assembled history digest and continuation
IDs across those batches. The table uses one complete baseline/candidate pair.

| Workload | Peak RSS before / after | RSS 3 s after disconnect before / after | CPU seconds before / after |
| --- | ---: | ---: | ---: |
| Four short WebSocket clients | 32 / 30 MiB | 32 / 28 MiB | 0.14 / 0.12 |
| Four long WebSocket clients | 309 / 205 MiB | 236 / 29 MiB | 2.98 / 2.97 |
| Eight long WebSocket clients | 651 / 375 MiB | 332 / 31 MiB | 6.59 / 6.66 |
| Four structured-history WebSocket clients | 341 / 325 MiB | 326 / 29 MiB | 3.71 / 4.11 |
| Four compressed WebSocket clients | 364 / 265 MiB | 219 / 28 MiB | 4.30 / 4.03 |
| Four long HTTP/SSE clients | 314 / 245 MiB | 140 / 30 MiB | 11.98 / 9.97 |

At the end of each connected rest, long-case RSS was also about 28–31 MiB.
Fresh-instance RSS was about 25 MiB. Peak RSS again includes kernel `VmHWM`.
These measurements cover EMP alone, excluding Claude CLI, Codex and the fixture.
They do not establish a fixed memory bound for an instance with real caches
or a busy statistics query.

Structured-history CPU increased by about 11%; eight-client CPU increased by
about 1%. Other long cases used equal or less CPU in this pair. Complete workload
duration fell by 2–5%; it includes synthetic upstream delays and connected rests.
These single-pair observations establish the measured memory/CPU tradeoff;
real-model latency and TPS require separate provider measurements.

Ablation distinguished the two allocation measures: the fixed mmap threshold
alone retained 293 MiB after the structured case, while idle trimming alone
retained 61 MiB there and 78 MiB after eight long clients. Together they retained
29 MiB and 31 MiB respectively. An isolated jemalloc preload with background
decay retained 41 MiB in the structured case, but had a higher peak and CPU cost
than the final candidate; no allocator dependency was added.

Each complete matrix verified 228 generated responses and 24 prefill responses.
The transport suite, 61 Claude adapter checks, 49 Native API checks, nine
lifecycle checks, and two installed-Claude-CLI contracts passed. The latter
used a temporary HOME, synthetic credentials and loopback CPA, including media
above 10 MiB. Strict Clippy, formatting and diff checks passed. No real models
were called; the running EMP was not replaced or restarted.

## Shared request ownership (2026-11-01 UTC)

This comparison starts from the allocation-lifecycle candidate above
(`43d2418f`), including its buffer and allocator changes. The new candidate
(`6b88a78c`) retains each parsed WebSocket request as one JSON value. Accounting,
Claude execution and context recording borrow that value. Native HTTP/SSE uses
the same ownership contract. Claude's large-text and structured JSONL carriers
serialize directly into the send buffer, avoiding an intermediate JSON tree.
The Native authentication module now imports its dependencies explicitly.

Both builds used the same release profile and synthetic workload described
above. A further case rejected the upstream WebSocket handshake with HTTP 426:
four downstream WebSocket clients then sent their full 16 MiB history every turn
through HTTP/SSE fallback. Each table row is one baseline/candidate pair.

| Workload | Peak RSS before / after | RSS 3 s after disconnect before / after | CPU seconds before / after |
| --- | ---: | ---: | ---: |
| Four short WebSocket clients | 31 / 31 MiB | 28 / 28 MiB | 0.23 / 0.21 |
| Four long WebSocket clients | 205 / 205 MiB | 29 / 28 MiB | 2.96 / 2.92 |
| Eight long WebSocket clients | 397 / 367 MiB | 31 / 31 MiB | 7.26 / 5.90 |
| Four structured-history WebSocket clients | 310 / 279 MiB | 29 / 29 MiB | 4.17 / 3.72 |
| Four compressed WebSocket clients | 269 / 253 MiB | 28 / 29 MiB | 4.48 / 3.80 |
| Four WebSocket clients using HTTP/SSE fallback | 234 / 232 MiB | 29 / 29 MiB | 15.70 / 8.82 |
| Four long HTTP/SSE clients | 245 / 248 MiB | 30 / 30 MiB | 11.90 / 9.91 |

Peak RSS includes kernel `VmHWM`; CPU seconds are cumulative EMP process work.
In this pair, eight-client and structured-history peaks fell about 7% and 10%.
The ordinary four-client WebSocket peak stayed level; the HTTP/SSE peak rose
2.7 MiB. Fallback CPU work fell 44%, while its elapsed workload time stayed
about 27 seconds. These process measurements exclude CLI children and use a
synthetic upstream, so they measure proxy overhead rather than model latency.
Long-case RSS returned to 28–31 MiB after disconnect, with no measured swap.

Each complete comparison validated 264 generated responses and 24 prefills,
including complete history digests and continuation IDs. The 61 Claude adapter
checks, 49 Native API contracts and three installed-Claude-CLI contracts passed;
the CLI contracts cover structured output, media above 10 MiB and multi-turn
HTTP/WebSocket images and tool results against loopback CPA. Browser E2E passed
for the updated review list, real-data Shadow isolation, Native history notices,
Ultra and routing through their normal settings entries. Strict Clippy,
formatting and diff checks passed.

At this stage, Shadow's v0.13.23 review covered request memory and the Native
history notice. The [current review list](../scripts/shadow_frontend/README.md)
remains cumulative until the next release.

## Native catalog selection (2026-11-02 UTC)

The reader now tries catalogs in the existing modification-time order and stops
at the first valid native source. It retains fallback for missing, malformed or
EMP-merged catalogs. This removes an eager second read without adding a cache
or invalidation policy.

A release-profile synthetic benchmark used two 1.13 MiB catalogs, each with 24
models and 48 KiB instructions per model. Each version ran 1,000 selections per
sample, three samples per case, alternating before/after order. Both versions
checked the complete selected model count, original instructions and native ETag.
Builds and other test suites were finished before timing.

| Preferred source | Median before / after | CPU seconds before / after | Peak RSS before / after |
| --- | ---: | ---: | ---: |
| Valid EMP-owned native snapshot | 1,034 / 472 ms | 1.03 / 0.46 | 5.81 / 4.67 MiB |
| Newer genuine Codex cache | 1,042 / 487 ms | 1.03 / 0.48 | 5.85 / 4.67 MiB |
| Newer EMP-merged cache, requiring fallback | 985 / 1,007 ms | 0.98 / 1.00 | 4.66 / 4.68 MiB |

The valid-first cases took 53–54% less time in this function. The mandatory
fallback case stayed within 2.3%. File-specific `strace` verified 200 → 100 opens
for 100 valid-first selections and 200 opens for fallback after the change.
These are isolated catalog-reader measurements, not whole-EMP RAM, model TPS
or end-to-end response latency. Raw samples and the reproducible Rust/Python
harness are retained in the existing local evidence directory.

## Sustained long-history requests (2026-11-02 UTC)

The allocation-lifecycle candidate ran eight concurrent WebSocket clients for
60 cycles over 899 seconds. Each cycle sent a complete 16 MiB history and two
incremental continuations per client, then rested eight seconds with the
connections open. All 1,440 responses completed; the loopback upstream verified
every assembled-history digest and continuation ID. No provider was called.

EMP peak RSS was 406 MiB, including kernel `VmHWM`; sampled peak RSS was 401 MiB.
RSS was 37 MiB three seconds after disconnect. Connected-rest RSS medians across
successive groups of ten cycles were 31.7, 34.4, 36.3, 36.5, 36.7 and 36.8 MiB;
the final twenty rests ranged from 36.66 to 36.83 MiB. The complete run used
133.34 CPU seconds, averaging 1.85% of the eight-thread machine, including those
connected rests. Swap remained zero. The fixture process is excluded.

This uses the same optimized release profile, with LTO disabled and 16 codegen
units, as the preceding comparisons. Builds and other benchmarks were stopped
throughout the run. This fifteen-minute repeated-request measurement extends
the earlier three-cycle tests; it is not a provider-latency measurement.

## Call report reads (2026-10-31)

A fixed local ledger snapshot contained 30,257 calls in the selected 30-day
window. Both variants used the same unoptimized `dev` build profile and query
parameters through the Shadow accounting endpoint. Builds and other tests were
finished before timing three requests for each variant.

| Variant | Request times (seconds) | Median (seconds) |
| --- | --- | ---: |
| Normalize filtered calls once | 6.85, 6.37, 7.47 | 6.85 |
| Reuse parsed facts and model groups; price after paging | 5.59, 4.44, 4.35 | 4.44 |

The median fell 35%. Every complete JSON report matched, including summaries,
model groups, buckets, ordering and record prices. Parsed facts and aggregates
live in connection-local temporary tables; the persisted ledger is unchanged.

[Statistics](usage-accounting.md#session-bills) stream actual row-processing
progress, showing only a percentage and progress bar beside Query/Refresh.
Browser E2E exercises fragmented progress frames, both report completion orders
and a failed read followed by a late successful response. It confirms that
failure ends progress and the late response cannot replace the error.
On 2026-10-31, a fixed 30,622-record snapshot also produced identical complete
JSON reports before and after adding progress streaming. Progress counts actual
row visits during materialization and aggregation, not elapsed time.
Management events call the current page handler. Actual source SSE events verify
that Shadow's fixed snapshot does not cancel or restart its statistics reads.

### Optimized release follow-up (2026-11-02 UTC)

A new fixed SQLite backup contained 80,097 calls in the selected 30-day window.
The baseline and final 0.13.24 candidate used the same optimized release profile,
with LTO disabled and 16 codegen units. Each queried the same isolated accounting
snapshot four times per window with a 50-record page. Builds and browser tests
were stopped throughout timing. The table shows the median of the three repeat
queries; the initial query is excluded in both variants.

| Window | Selected calls | Before / after (seconds) | Reduction |
| --- | ---: | ---: | ---: |
| 1 day | 21,934 | 0.932 / 0.782 | 16.1% |
| 7 days | 60,089 | 2.692 / 2.327 | 13.6% |
| 30 days | 80,097 | 3.622 / 3.068 | 15.3% |

The first 30-day query took 3.688 / 3.207 seconds. Complete report hashes matched
for every window, including summaries, model groups, buckets, sorted records
and prices. Normalization now keeps binary JSON, and report aggregation reuses
the already-filtered temporary table. No persistent cache or dependency was
added, and the source ledger was left untouched. The October 31 measurements
above used a smaller snapshot and the unoptimized `dev` profile.

## Complete-response event synthesis (2026-10-07)

The opt-in example measures complete JSON responses converted into Responses
events, including per-event JSON encoding. It uses synthetic text and tool data
and needs no provider, credentials or listening socket:

```sh
CARGO_BUILD_JOBS=1 cargo run --release -p emp-router --example response_projection_profile -- 1
CARGO_BUILD_JOBS=1 cargo run --release -p emp-router --example response_projection_profile -- 8
```

For comparisons, build first and run the resulting executable in three fresh
processes for each worker count. Avoid concurrent builds and tests. The example
counts requested live heap bytes with the system allocator; this is neither RSS
nor total allocation traffic. Its elapsed time includes fixture creation, thread
startup and allocation-counter overhead.

The baseline below is the local size-bounded vector implementation immediately
before replacing the event vector with a pull-based iterator, not a published
release. These initial iterator measurements still included a cumulative byte
counter, since removed. Both variants used the same optimized build profile and
inputs. Median results from three processes per variant and worker count:

| Scenario | Workers | Peak heap before / after | Batch time before / after |
| --- | ---: | ---: | ---: |
| Long answer | 1 | 8.77 / 3.63 MiB | 36.66 / 26.62 ms |
| Many content parts | 1 | 6.80 / 2.95 MiB | 33.96 / 17.53 ms |
| Tool history | 1 | 3.04 / 1.34 MiB | 22.43 / 14.77 ms |
| Long answer | 8 | 64.90 / 26.42 MiB | 85.23 / 79.46 ms |
| Many content parts | 8 | 44.65 / 20.58 MiB | 75.49 / 55.93 ms |
| Tool history | 8 | 21.83 / 7.95 MiB | 52.58 / 44.46 ms |

Encoded byte totals and event counts per request matched in all scenarios.
Peak requested heap fell 54–64% in these fixtures. Timing is a small local
sample; the eight-worker long-answer time ranges overlap. No model-token speed,
whole-server memory limit or cross-platform performance claim follows from it.

The subsequent change removes that cumulative generated-byte counter: consumers
write and discard each event before requesting another. The three original
scenarios retain exactly the same event counts and encoded byte totals. A fourth
scenario, `repeated_metadata`, uses a source below 100 KiB and delivers 1,028
events totaling 64.3 MiB per request. Three fresh optimized runs measured:

| Workers | Peak requested heap for the batch | Completed requests |
| ---: | ---: | ---: |
| 1 | 0.92 MiB | 1 |
| 8 | 5.99 MiB | 8 |

This checks that cumulative output can exceed the old 64 MiB cutoff without
retaining the full event sequence. The source JSON, current event, serialization
buffer and thread allocations are included. It is not a whole-server RSS bound.
Loopback endpoint tests separately cover native/external JSON fallback delivering
more than 64 MiB and cancellation after the first text event, without retry or
a false completed receipt.

## Initial measurements (2026-10-04)

Linux, optimized build, hotpath 0.28.3, identical synthetic inputs, three fresh
processes per variant. Values below are the median of the three scenario p50s.
Compilation did not run concurrently with these measurements.

| Scenario | Before | After |
| --- | ---: | ---: |
| Native complete HTTP response | 60.70 ms | 4.04 ms |
| External short-input SSE, through connection close | 103.56 ms | 52.84 ms |
| External long-input SSE, through connection close | 107.83 ms | 61.46 ms |
| Approximately 1 MiB request-byte accounting | 2.50 ms | 0.64 ms |
| 16 KiB tool-argument SSE encoding | 45.42 µs | 15.56 µs |
| Small text SSE encoding | 366 ns | 177 ns |

The changes consolidate spaced JSON encoding into one serializer with two
actual writers: a frame buffer and a byte counter. Callers no longer implement
their own quote/escape scanners. The old counter-internals test is replaced by
an encoding-interface test; existing large-text and escaped-input contracts stay.

Disconnect monitoring now creates its worker on the first awaited operation.
Synchronous context checks previously spawned and immediately joined a socket
reader, often waiting for its 50 ms timeout before dispatching the request.
This lifecycle change is internal to the monitor; HTTP, WebSocket and compaction
call sites retain their existing interface and cancellation behavior.

In separate allocation-instrumented runs, the same 12,420 SSE frame calls
allocated 84.8 MB before and 49.4 MB after, as reported by hotpath, in all three
runs. This is allocation traffic, not a reduction in resident memory.

Catalog/ETag construction remained approximately 1.3 ms in this fixture and was
not changed. Warm HTTP client acquisition was measured in microseconds; this
workload does not establish lock contention under concurrent load. Neither
measurement justifies adding a long-lived cache or redesigning the pool yet.

## Interruptible connection teardown (2026-10-04)

The active disconnect worker now uses [polling](https://docs.rs/polling/3.11.0/polling/struct.Poller.html)
to wait for socket readiness or an explicit stop notification. Drop wakes and
joins the worker, restores the original read timeout and leaves queued bytes
untouched. A timed peek is retained only as a backstop after readiness.

The same optimized workload was rerun for both the previous and new binary:
three processes and 90 successful HTTP calls per variant, without compilation
running concurrently. Median scenario p50s were:

| Scenario | Previous timed worker | Interruptible worker |
| --- | ---: | ---: |
| Native complete HTTP response | 5.17 ms | 5.18 ms |
| External short-input SSE, through connection close | 51.95 ms | 4.35 ms |
| External long-input SSE, through connection close | 57.02 ms | 16.21 ms |

Catalog construction was about 2 ms in both variants in this run. Host load
varied from the initial measurements; the contemporaneous comparison avoids
attributing that variation to source changes. Request preparation was also
consolidated between these variants, without changing the fixture.

Focused Linux contracts passed for cancellation before headers and midstream,
HTTP/SSE/WebSocket forwarding, history/context switching and fake-upstream
Claude CLI calls. Socket checks cover unread bytes, connection usability and
timeout restoration after dropping both active and unused monitors. Windows
and macOS runtime verification is still required before release. These results
do not establish real-model token speed or first-token latency.

## Ownership-refactor verification (2026-10-04)

After the configuration, event, catalog, outcome and frontend changes, the
interruptible-worker baseline was compared with the final implementation in
three fresh processes per variant. Both used the same optimized instrumented
workload (90 HTTP calls per variant). No tests or builds ran during timing.

| Scenario | Before ownership changes | After ownership changes |
| --- | ---: | ---: |
| Four-account catalog/ETag | 1.54 ms | 1.57 ms |
| Native complete HTTP | 5.086 ms | 5.091 ms |
| External short-input SSE through close | 4.281 ms | 4.241 ms |
| External long-input SSE through close | 16.538 ms | 15.818 ms |
| Approximately 1 MiB byte accounting | 0.710 ms | 0.721 ms |
| 16 KiB tool-argument encoding | 11.58 µs | 12.83 µs |
| Small text encoding | 201 ns | 180 ns |

The main HTTP paths retained the earlier teardown improvement; no material
HTTP regression was observed in this fixture. Catalog source reuse did not
produce a measurable catalog-speed improvement in these runs. The isolated
large-tool encoder median was 10.8% higher (1.25 µs); its per-run medians overlap
(10.83–13.19 µs before, 11.04–13.68 µs after), so this pass claims no further
encoding speedup. These small samples do not prove equivalence under every load.

Validation on Linux: 731 workspace tests passed; seven installed-CLI tests
normally ignored by default also passed against fake upstreams. Four other
environment-specific checks were not run. Six frontend behavior checks passed,
as did formatting, strict workspace Clippy and the actual-server web assets and
session-restart contract. Windows and macOS execution remain unverified.

Frontend checks can be run independently with:

```sh
node crates/emp-app/tests/web_features_contract.cjs
```

The refactoring also fixes pending credential rotation being discarded before
an account import/removal commit succeeds. A persistence-failure fixture checks
that the previous files/configuration and the only usable rotated token survive.
