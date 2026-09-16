# Codex protocol review follow-up

Implementation checked on 2026-09-15 against the current EMP checkout (base
`5fcbdbb`, with earlier uncommitted fixes retained). The original external review
is preserved in [GPT Pro's review](gpt-pro-review-2026-09-14.md).

## Evidence and delivered changes

Official source snapshot:
[`openai/codex` at `2fdcdeaf0e219eea34c710e01de2ee0571ddeeb5`](https://github.com/openai/codex/tree/2fdcdeaf0e219eea34c710e01de2ee0571ddeeb5).
This is a source snapshot, not a claim about the latest stable release.

- `catalog._external_entry` now copies an explicit set of coding instructions
  and tool settings. New native fields no longer silently become external
  capabilities. Nested model messages also use an allowlist: token-budget and
  other unreviewed modes are not copied. Experimental context is explicitly
  disabled. Native model capabilities remain intact.
- `_account_entry` omits `available_access_programs` because the source catalog
  belongs to the current login, not the imported account. Absence preserves
  unknown status; it does not assert an empty set of entitlements.
- `HistoryAnchor.from_headers` compares explicit thread identities and no
  longer equates them with `session-id`. Only a positively identified stable
  legacy `version` header through 0.154.0 permits session-only fallback.
  Unknown and newer versions need explicit thread identity to read history.
  The outgoing cache key is not rewritten, and execution-session metadata is
  not substituted for a thread ID. An empty prewarm turn ID means no turn yet.
- WebSocket continuation records the completed response's thread/window scope.
  Changed scope (including losing a previously known scope) requires the client's full request through the
  existing `previous_response_not_found` response; EMP does not replay a task.
  A turn change alone is not treated as expiry.
- `tool_bridge.ExternalTools` maps namespace/name identities to deterministic,
  request-local portable function names and restores them on the way back.
  Definitions, historical calls, `tool_choice`/allowed-tools restrictions,
  ordinary/custom calls, full responses, streaming items, and history observers
  share this one boundary. Plain names and call IDs remain unchanged. No global
  alias registry, schema cache, or second implementation per protocol was added.
  Portable names include a readable prefix, and descriptions retain namespace
  identity and documentation so a model can distinguish similar tools.
  Identical definitions remain valid; conflicting schemas or aliases return
  HTTP 422 before contacting an upstream. Native requests are not mapped.
- Client-executed `tool_search` is exposed as an external function and restored
  as a Codex `tool_search_call` with object arguments. Codex performs discovery
  and execution. Loaded tools are collected from `additional_tools` and
  `tool_search_output`; call/output identity and discovered definitions survive
  history normalization. Compaction treats search calls/results as tool pairs.
  Search argument deltas are not mislabeled as function-call deltas. Malformed
  search output fails explicitly, and server-executed search is not emulated.
  External catalog entries now enable search after the complete round trip was
  exercised with the official runtime and a local MCP server.

The identity interpretation follows `core/src/client.rs` (`responses_session_id`,
`build_session_headers`, and incremental-request handling). Capability ownership
follows `protocol/src/openai_models.rs` (`ModelInfo` and `ModelMessages`). Tool
search follows `core/src/tools/handlers/tool_search.rs`, `tool_search_spec.rs`,
`core/src/tools/spec/plan.rs`, and `protocol/src/models.rs` in the same snapshot.
[Official App Server documentation](https://learn.chatgpt.com/docs/app-server)
describes the client interface used by the binary-level checks.

## Verification

- Windows native PowerShell 7.6.5; Python 3.11; 259 targeted tests passed,
  including router integration across all three external protocols, destination
  compaction, and existing local WebSocket server continuity tests.
- Official Codex 0.154.0: four isolated binary tests passed on Windows,
  Intel macOS, and Linux x64. They cover the
  existing backend's model list, recovery after a simulated TLS failure without
  EMP replay, and an external-model tool round trip that verifies actual command
  output rather than accepting an unknown-tool error as success, and a complete
  tool-search/discovery/MCP-execution round trip. The MCP fixture is read-only,
  has no network or filesystem operations, and returns a fixed marker.
- Four in-memory ablations independently restored the old catalog inheritance,
  cache/thread comparison, tool deduplication, and continuation decisions. Each
  was caught by its corresponding regression. No working files were reverted.
- Two additional in-memory ablations disabled namespace restoration and search
  event conversion independently. Their integration/round-trip tests detected
  both regressions. Shared projection stays in one small boundary rather than
  introducing provider-specific discovery engines or persistent mapping state.
- `.github/workflows/runtime-compatibility.yml` includes `tests/**` triggers and
  separates compatibility checks from packaging. It declares Windows, macOS,
  and Linux jobs pinned to Codex 0.154.0. The workflow has not been pushed or run
  on GitHub. macOS/Linux protocol suites were also executed over SSH in isolated
  source copies (259 tests on each platform). The Mac test environment uses a canonical temporary path to
  avoid system `/var` symlinks; production symlink protections remain unchanged.
  Noninteractive CLI tests explicitly close stdin, including when run over SSH.
  The command fixture separates its home from its scratch directory so Codex
  can create its sandbox helper normally. Linux uses the complete official npm
  distribution, including bundled `bubblewrap`; a lone release executable was
  insufficient for sandboxed command execution. Sandbox settings were not
  weakened to make the check pass.
- The existing non-fatal observer contract is preserved: an observer exception
  cannot turn otherwise valid forwarding into a failed request. The final
  bridge/stream checks passed on all three platforms after that integration.
- Temporary remote source copies, test runtimes, virtual environments, and
  source archives were removed after checking for surviving test processes.
  Existing installations and original remote source directories were untouched.

Run the real-binary checks with:

```text
python tools/test_codex_runtime.py /absolute/path/to/codex
```

The runner exports the binary's bundled catalog, creates an empty temporary
`CODEX_HOME`, supplies fixture-only credentials, and blocks non-loopback network
access through an unavailable proxy. It does not use personal model caches,
account credentials, or a paid upstream. Temporary homes are removed on exit.

## Acceptance boundaries

The fixed binary and protocol fixtures establish the tested Codex contract, not
the failure rate of paid upstream services or behavior of every ChatGPT App
build. No broad cross-turn invalidation rule was inferred from an unconfirmed
upstream issue, and no runtime compatibility range was automatically expanded.
No running EMP binary was replaced and no release was published in this round.
