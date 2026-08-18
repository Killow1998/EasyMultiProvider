# Progress

## 2026-08-18

- Started model discovery and subscription deduplication work.
- Created the file-based plan and findings log.
- Inspected config, account normalization, and catalog generation.
- Inspected router, server routes, and the current Web UI save flow.
- Confirmed model hiding can reuse `enabled`; next inspect test seams and the
  actual native auth identity without exposing credentials.
- Inspected test coverage and identified safe seams for discovery and auth
  identity tests.
- Inspected the local auth shape without printing credential values; stable
  identity is `tokens.account_id`.
- Decided to retain encrypted duplicate copies but omit their subscription
  aliases from the generated catalog.
- Next: implement model discovery and identity filtering.
- Implemented provider model discovery, automatic model insertion, model hide
  toggles, and subscription duplicate status/filtering.
- Next: add focused regression tests, then run syntax/full tests and a local
  discovery/API smoke check.
- Added discovery, pagination/filtering, hidden-model, duplicate-account, and
  state-merge regression tests; the non-socket portions pass.
- Sandbox blocked three tests that bind `127.0.0.1`; next rerun the full suite
  with the required local socket permission.
- Full suite rerun passed: `Ran 45 tests ... OK (skipped=1)`.
- Next: fix the standalone Web syntax check and run a read-only live Gemini
  model-list smoke test with network access.
- Live Gemini discovery passed (37 models); current local account inspection
  marked `ship` as duplicated by the current Codex login and the regenerated
  catalog omitted its aliases.
- Correction: the previous curated `security-best-practices` install was not
  the requested tool; replace it with the official Codex Security npm CLI.
- User decision: defer that installation and scan until all EasyMultiProvider
  features and tests are complete; treat it as the final wrap-up step.
- MVP freeze decision: current local performance, memory, concurrency, and
  feature scope are accepted; future extensibility will be driven by real
  failures or measurements rather than prebuilt abstractions.
- Final security wrap-up started; scan scope will exclude local secrets,
  encrypted account storage, generated artifacts, and configuration data.
- First test attempt was blocked by the sandbox's read-only default uv cache;
  retrying with `/tmp/easy-mp-uv-cache`.
- Codex Security dry-run rejected `/tmp` as the output parent; switching to an
  owner-controlled sibling directory outside the project.
- The scanner requires every output parent to be non-group-writable; the
  aggregate workspace is being tightened from `775` to `755` as a minimal
  permission hardening step.
- Official Codex Security dry-run passed outside the sandbox with ChatGPT
  stored-credential mode; scan scope is the application, tests, docs, and
  dependency manifest only.
- Removed the unrelated curated security skill from this project and added
  `node_modules/` to `.gitignore`; the official scanner remains available via
  the local npm dependency.
- The first actual scan used the official default `gpt-5.6-sol`; it reached
  24/25 files before being canceled by the user to control token cost. Its
  partial artifacts are outside the project and are not a final scan result.
- No `lunamax` model is present in the current project configuration; do not
  rerun until its exact model/provider route is identified.
