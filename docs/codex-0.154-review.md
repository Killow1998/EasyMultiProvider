# Codex 0.154 compatibility review

Reviewed against official `rust-v0.154.0`, commit
`6b9826e3aa83b1a5947db50f4332cb9c65f1b340`, on 2026-09-11.

## Model refresh and verification

Codex consumes `X-Models-Etag` on Responses HTTP requests and
`codex.response.metadata.headers.x-models-etag` on WebSockets. EMP now sends
the revision of its own catalog through both transports. The discovery response
uses the same canonical hash. A reused WebSocket announces the current revision
for each request; an upstream catalog revision cannot replace the EMP revision.

The local `model/list` check now compares visible IDs, display names and
descriptions. Previously, matching IDs could incorrectly report success after
a rename or context-label change. Stale presentation stays pending, while
connection and permission failures remain distinct errors.

Windows Python builds can omit `socket.AF_UNIX` even when Winsock and Codex
support local Unix sockets. A small Windows connector handles that case without
starting another Codex process. macOS and Linux keep the ordinary socket path.
Linux App discovery also checks the known
`CODEX_HOME/plugins/.plugin-appserver/codex` location; this does not establish
coverage for every third-party AppImage or distribution package layout.

Dynamic discovery still requires the appropriate ChatGPT login and startup
configuration. Codex's 0.154 refresh worker runs approximately every 270 seconds;
an ETag change can trigger an earlier fetch after a response. This is not an
instant repaint guarantee for an idle desktop menu. Changing a Base URL or
leaving a previously loaded static catalog still requires a safe client restart.

## Transport errors and diagnostic history

Upstream HTTP errors retain their safe error category and, for 429/503,
`Retry-After`. Seconds and HTTP dates are normalized before forwarding.
Chat and Anthropic stream adapters pass these errors to the existing shared
stream boundary instead of losing the delay while converting them to events.
The HTTP response and WebSocket error retain structured metadata; failed SSE
events retain the delay and a bounded public message. No new automatic retry
or timeout increase was added.

Linux tests exposed a separate history-ordering bug: log file modification
times can tie or change after copying. Reading recent observations now uses
each record's UTC timestamp and a bounded heap, so the newest records survive
the limit regardless of file traversal order. The journal format is unchanged.

## Simplification and ablation

- `reload` delegates to the existing read-only `observe` path instead of
  duplicating its query and error handling. Neither operation manages the
  desktop App Server lifecycle.
- Windows, macOS and Linux share one known plugin-runtime path lookup.
- Catalog revisions use one function; no watcher service or extra cache was
  introduced.
- HTTP failures pass through the existing stream failure boundary instead of
  adding separate retry policies to each protocol adapter.

Four temporary mutation checks removed one behavior at a time without rewriting
production files. Each baseline passed; each removal was detected:

| Removed behavior | Regression detected |
| --- | --- |
| Updating the revision sent on Responses | Response revision differs from model discovery after rename |
| Checking names and descriptions | Stale presentation is incorrectly accepted when IDs match |
| Forwarding `Retry-After` | HTTP 429 loses the upstream wait interval |
| Windows socket adapter | Official Codex local model query cannot connect with this Python build |

The permanent regression tests remain in the repository. These checks justify
the retained behavior; they are not a claim that every abstraction in EMP was
evaluated.

## Verification

The tests use isolated source copies and temporary Codex homes, with no real
account credentials or paid model requests. The optional official-binary test
is enabled with `EMP_TEST_CODEX_BIN` and starts only its own temporary backend.
It verifies the local control connection and `model/list` presentation fields.
HTTP and reused-WebSocket tests verify EMP catalog notifications against local
fixtures. They do not observe a running ChatGPT App menu repaint.

| Host environment | Full suite | Official 0.154.0 local control query |
| --- | --- | --- |
| Windows x64, Python 3.11.14 | 1028 tests, 11 conditional skips, passed | Passed |
| Intel macOS 15.7.3, Python 3.11.16 | 1028 tests, 10 conditional skips, passed | Passed |
| Linux x64, Python 3.11.16 | 1028 tests, 10 conditional skips, passed | Passed |

Each suite ran with `uv run --no-sync python -m unittest discover -s tests -q`.
The existing platform-specific and optional live-service skips remain; failures
were not converted to skips. The macOS run also used its installed Node.js for
the Web UI DOM tests. Unix test copies lived in private, non-symlinked paths so
the production executable-path checks could remain enabled.

A read-only scan of installed runtimes found the macOS ChatGPT App and managed
runtime. The Linux host exposed CLI and VS Code runtimes, but no ChatGPT App was
found in the inspected standard locations. The Linux plugin-path branch is
covered by a filesystem fixture, not by an installed App on that host.

The desktop menu repaint, unknown Linux packaging layouts, real upstream
availability and Apple Silicon execution were not tested. No release was
published during these source checks, and no installed EMP or user account
configuration was replaced.

## Official source references

- [Release and native binaries](https://github.com/openai/codex/releases/tag/rust-v0.154.0)
- [App Server model mapping](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/src/models.rs)
- [Model refresh worker](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/src/models_refresh_worker.rs)
- [Responses ETag regression tests](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/core/tests/suite/models_etag_responses.rs)
- [Model API type](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server-protocol/schema/typescript/v2/Model.ts)
