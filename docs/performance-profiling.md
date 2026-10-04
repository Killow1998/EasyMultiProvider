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
