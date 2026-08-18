# EasyMultiProvider model sync, subscription dedupe, and provider metadata

## Goal

Fetch provider model lists after Base URL is configured; let the user hide
unused models; detect imported Codex subscriptions duplicated by the current
`~/.codex/auth.json`; preserve the encrypted credential boundary.

Also install and run the requested official `@openai/codex-security` CLI in
the workspace, then define what LiteLLM-style deployments can and cannot
expose through a Base URL.

## Phases

- [completed] Inspect provider list protocols and auth identity fields
- [completed] Add model discovery API/UI and hidden model state
- [completed] Add subscription identity dedupe and catalog filtering
- [completed] Regression tests and verification
- [in_progress] Final wrap-up: install and run the official Codex Security CLI in this workspace
- [completed] Verify LiteLLM metadata/protocol limits and document the result

## MVP freeze decision

- Freeze the current architecture after the security wrap-up.
- Do not add speculative extensibility, database, worker pools, or platform
  packaging before a real workload exposes a concrete need.
- Expand only from observed failures or measured limits.

## Decisions

- Hidden is a soft config flag; do not delete provider models.
- Duplicate accounts are reported and excluded from the merged catalog; their
  encrypted credentials are not deleted automatically.

## Errors Encountered

| Error | Attempt | Resolution |
| --- | --- | --- |
| uv cache read-only | Default `uv run` cache | Use `/tmp/easy-mp-uv-cache` for verification. |
| Duplicate test red | Parsed encrypted auth as JSON | Decrypt through `load_auth()` in memory. |
| Hidden test red | Used host native catalog | Point test config at a missing temp catalog. |
| Server socket blocked | Sandbox disallows local bind | Rerun full suite with approved local socket access. |
| Live discovery blocked | Sandbox disallows outbound network | Retry read-only smoke test with approved network access. |
| Web check command red | Regex shell escaping | Use index slicing instead of a regex. |
| Security dry-run blocked | `/tmp` is not a trusted output parent | Use a private owner-controlled scan directory outside the project. |
| Security output permission blocked | Aggregate workspace was `775` | Tighten `/home/nuc/NA2H` to `755`; keep scan output under private `state/`. |
