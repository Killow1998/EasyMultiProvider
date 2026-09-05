# Codex 0.153 / Astra compatibility review

Reviewed against the official `rust-v0.153.4` source and the Astra API model
documentation on 2026-09-04. Side chat reconstruction is outside this review.

## Changes required in EMP

- Add `gpt-6-astra` to the official API capability registry. API metadata is
  1,050,000 context, 922,000 input and 128,000 output tokens, with `low` through
  `max` reasoning. The registry enriches discovered/user-added routes; it does
  not create account access or override native Codex model metadata.
- Keep native catalog metadata intact when applying names and visibility.
  Codex's bundled Astra metadata uses a 272,000 default context and 872,000
  maximum, includes `ultra`, and advertises the `priority` service tier. These
  are Codex-specific settings, not interchangeable with API limits.
- Stop external models inheriting a native template's service tiers, plan
  restrictions, compaction hash, model upgrades, experimental tools and
  Responses Lite routing. A coding-tool template does not establish these
  capabilities for another upstream.
- Preserve an explicitly supplied `service_tier` in portable Responses
  projection. Dropping it silently changes a requested Fast call to Standard.

## Transport and stability findings

Direct source comparison of 0.152.0 and 0.153.4 found no changes in
`codex-rs/core/src/client.rs` or
`codex-rs/codex-api/src/endpoint/responses_websocket.rs`.
The latter still enables permessage-deflate using the WebSocket library's
default frame/message configuration. EMP already enables upstream compression,
keeps each native connection associated with its downstream connection, probes
idle sockets, and bounds request growth by available memory.

The SSE change preserves raw response usage as additional metadata; it does not
change the existing output/reasoning token fields used by EMP. New
`model_messages.auto_review.node_repl_policy` is preserved by native catalog
copying. App-server thread metadata adds nullable model/effort fields; EMP's
model catalog probe does not depend on those thread fields.

The CLI's TUI reconnect changes are between TUI and app-server, not the EMP
Responses listener. Experimental context management is gated upstream and is
not enabled by EMP. No retry-limit or concurrency increase is justified solely
by this release. Local capacity remains distinct from upstream account limits;
this review does not establish a new production concurrency guarantee.

## Verification

`tests/test_astra_compatibility.py` covers registry enrichment, native metadata
and presentation preservation, external template separation and explicit tier
projection. Its optional real-CLI test accepts `EMP_CODEX_TEST_BINARY` and
`EMP_CODEX_TEST_CATALOG` (the official bundled models JSON), uses a temporary
Codex home and synthetic credentials, and checks Standard/Fast requests through
EMP against a local Responses fixture. It never calls a real model.

Windows verification completed: 969 tests, 11 conditional skips; the optional
0.153.4 CLI test passed separately, including the actual upstream tier values
(`None` for Standard and `priority` for Fast). The Windows executable was rebuilt
and passed the packaging smoke checks. Compatibility is now 0.149.x–0.153.x,
with 0.153.4 recommended. This does not measure real upstream throughput or
guarantee account availability for Astra.

Intel macOS verification also passed with the official 0.153.4 runtime: native
`model/list` returned Astra, refreshed the older native catalog cache, and EMP
preserved its native capabilities. The isolated Standard/Fast forwarding test
passed on macOS without calling a real model. Runtime discovery now checks
ChatGPT/Codex app bundles separately from the managed standalone runtime; an old
standalone installation no longer masks the desktop app's version. Discovery and
rescan regression tests cover both versions and the plugin fallback.

## Sources

- [Official Codex changelog](https://learn.chatgpt.com/docs/changelog)
- [Astra API model](https://developers.openai.com/api/docs/models/gpt-6-astra)
- [Codex 0.153.4 source](https://github.com/openai/codex/tree/rust-v0.153.4)
- [Version comparison](https://github.com/openai/codex/compare/rust-v0.152.0...rust-v0.153.4)
