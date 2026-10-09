# Codex maintenance window

EMP follows the latest stable Codex minor series and its previous nine series.
Patch releases count within their series; alpha and beta releases do not move
the window. The engine used by the CLI, App or IDE is the compatibility input,
independent of the host application's version. This policy does not introduce a
global version rejection: individual operations still check their interfaces.

## Source acceptance on 2026-10-09

The current maintenance target is **0.153–0.162**, based on the official
[0.162.0 release](https://github.com/openai/codex/releases/tag/rust-v0.162.0).
The following checks used the local EMP v0.12.14 development source and official
Linux Codex binaries, with disposable homes and synthetic loopback providers.

| Engine | Accepted paths |
| --- | --- |
| 0.153.4, 0.154.0, 0.155.1, 0.156.1, 0.157.1, 0.158.0, 0.159.3, 0.160.1, 0.161.0, 0.162.0 | Real MCP tool invocation before/after compaction; external compaction; parent, early fork, later fork and grandchild resume; exact native configuration restoration; idempotent second restore |
| 0.162.0 | Additional: external streaming steering; native WebSocket interrupt and same-turn continuation; deferred Tool Search and same-named namespace tools through Chat, Anthropic, external Responses and native HTTP forwarding |

All ten series passed the common Linux fixture contract. Windows, macOS, App
and IDE bundled engines need their own acceptance evidence; Linux fixtures do
not establish real-provider acceptance.

## Protocol decisions

- **Provider capabilities:** EMP's normal integration changes the existing
  Codex endpoint and catalog settings. It does not replace them with a new
  custom provider. In 0.162, omitted capability overrides retain provider
  defaults, including native OpenAI remote compaction. No extra custom-provider
  capability declaration is needed for this integration. See
  [Codex's capability defaults](https://github.com/openai/codex/blob/rust-v0.162.0/codex-rs/model-provider/src/capabilities.rs).
- **Retry advice:** synthesized SSE and WebSocket errors expose
  `error.headers["Retry-After"]`, while retaining the legacy seconds field.
  Converting a pre-output native failure to HTTP reads this header first,
  including case-insensitive names, then falls back to the legacy field.
  Numeric and HTTP-date advice share the existing bounded parser. HTTP clients
  receive the actual `Retry-After` header. Other upstream headers are not copied.
  See [Codex's streamed error reader](https://github.com/openai/codex/blob/rust-v0.162.0/codex-rs/codex-api/src/sse/responses_error.rs).
- **Tools:** the real-engine fixture exercises tool input/results through
  compaction and forks. Source contracts additionally exercise 0.162
  `additional_tools`, client Tool Search input/results, repeated definitions and
  equal function names in distinct namespaces through Chat, Anthropic and
  external Responses projection. Streaming restoration suppresses only search
  argument deltas and retains ordinary function deltas. Real native HTTP tests
  verify that incremental catalog/history items pass through unchanged. The
  real-engine contract additionally runs deferred Tool Search in Codex 0.162.0:
  search, discover `alpha.read` and `beta.read`, execute both local read tools,
  return their results, and finish the same turn. It covers Chat, Anthropic,
  external Responses and native HTTP forwarding with Responses Lite incremental
  catalogs. Actual request receipts verify search item types and paired history.
  These checks do not establish every incremental event combination or real
  upstream-model behavior.
- **Instruction roles:** Responses Lite developer/system messages are projected
  to ordered Anthropic system blocks. They retain instruction priority instead
  of being rejected or downgraded to user messages. Projection failures retain
  their content-free reason in EMP errors.

Use the [native restore harness](../scripts/native_restore_acceptance/README.md)
for isolated behavior checks. Record the actual engine version and tested paths
when moving the maintenance window.

Run the real-engine Tool Search contract with an official Linux 0.162.0 binary:

```sh
EMP_TEST_CODEX_CLI=/path/to/codex cargo test -p emp-app --lib \
  installed_codex_discovers_and_calls_same_named_tools_across_protocols \
  -- --ignored --nocapture
```

The contract creates disposable Codex homes, randomized loopback EMP/upstream
ports and local read fixtures. It disables plugins and uses synthetic keys; it
neither uses the installed EMP service nor consumes real provider quota.

## Runtime discovery and update acceptance

The existing inert installation matrix covers CLI, App and IDE paths on Windows,
Linux, Intel macOS and Apple Silicon macOS. Host metadata is checked separately
from the selected engine version; these checks never execute the inert files.
Passing this matrix does not establish inference in every bundled engine.

Updater fixtures cover interrupted downloads, the existing three-retry budget,
stage/error receipts surviving cleanup, actual process handoff and preservation
of configuration and encrypted credentials. Native Windows/macOS system-proxy
behavior and the next official-package update need host-specific acceptance.
Do not replace that acceptance with a private build installed on a user's host.
