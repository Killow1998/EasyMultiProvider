# Findings

- The reported Codex session selected `easy-multi-provider` and
  `gemini/gemini-3.7-flash` correctly. The first request completed; a later
  request ended after `task_started` without a response.
- Direct Gemini Chat Completions and the EMP Responses bridge later succeeded.
  One Gemini streaming request returned HTTP 503 with a transient high-demand
  message. EMP had no transient retry at that point, so Codex could look stuck.
- Python's standard URL opener already uses the configured HTTP/HTTPS proxy;
  local Codex-to-EMP traffic should use `NO_PROXY=127.0.0.1,localhost`.
- Gemini model metadata exposes context limits and `thinking: true`, but not a
  reasoning-level list. The current implementation maps known Gemini model
  families to their documented levels.

## Current investigation

- Need to inspect provider configuration, model catalog filtering, account
  import normalization, and the native Codex auth identity fields before
  changing the data model.

## Source inspection

- Models already have an `enabled` flag and `catalog.build_catalog()` already
  omits disabled models. Model discovery can therefore reuse that flag instead
  of adding a second hidden-state field.
- Accounts are normalized to `id`, `name`, `prefix`, encrypted `auth_file`,
  `enabled`, and quota. `auth_headers()` already extracts
  `tokens.account_id` or top-level `account_id`; this is the stable identity
  candidate for deduplication.
- `public_config()` currently exposes account metadata without credentials.
  Duplicate status can be added there without returning encrypted auth data.
- `router.model_metadata()` is Gemini-only today. Discovery should be a
  separate list operation: generic OpenAI-compatible providers use `GET
  /models`, Gemini uses native `GET /models` after removing `/openai`, and
  Anthropic has no standard list endpoint.
- The Web already renders every configured model and sends the full normalized
  state to `/api/config`; a checkbox can toggle the existing `enabled` flag.
  Provider editing currently replaces the whole provider object, so an update
  must preserve the existing secret marker/fields through the existing merge
  behavior rather than inventing a second secret path.
- `find_route()` and `catalog.build_catalog()` already skip disabled external
  models. The catalog account loop still includes every enabled credential;
  this is the correct single boundary for duplicate filtering.
- Existing tests cover encrypted import/delete, safe account responses, catalog
  aliases, and disabled external routing only indirectly. New tests should use
  patched HTTP discovery and a temporary `CODEX_HOME`, never real credentials.
- The real local auth file has `tokens.account_id`; this makes account-ID
  comparison reliable without persisting or displaying access/refresh tokens.
- Keep duplicate encrypted copies for now: deleting them automatically would
  be destructive, while omitting their aliases from the generated catalog
  gives the requested filtering and leaves a recoverable credential.
- The README documents the existing API and needs the two new endpoints plus
  the duplicate/hide behavior after implementation.

## Implementation

- Added `/api/providers/discover`; Gemini uses native `/models` pagination and
  generic API-key providers use OpenAI-compatible `GET /models`.
- Discovery adds models automatically, preserves an existing model's enabled
  state and manual context value, and refreshes the generated catalog.
- The Web model table now toggles `enabled`; hidden models remain editable but
  are omitted by the existing router/catalog boundary.
- Account public metadata now marks duplicates; catalog generation excludes
  accounts matching the current Codex auth or an earlier imported account.
- Correction: `openai/codex-security` is a separate official CLI/TypeScript
  SDK, not a Codex `.agents/skills` package. Its documented install path is
  the npm package `@openai/codex-security` and its scan command is `npx
  @openai/codex-security scan .`.

## Verification errors

- `uv run ...` could not create its default cache temporary file under
  `/home/nuc/.cache/uv` because that path is read-only in the sandbox. Retry
  with a project-scoped temporary UV cache under `/tmp`.
- The first duplicate test exposed that imported credentials are encrypted;
  identity comparison must call `load_auth()` and never parse `auth.json.enc`
  as plaintext. The hidden-model test also needed an isolated native catalog.
- Targeted Server tests pass except three pre-existing HTTP-bound tests blocked
  by sandbox socket policy; rerun the complete suite with local socket access.
- The complete suite with local socket access passed: 45 tests green, one
  optional real Codex CLI demo skipped by its explicit environment guard.
- A live Gemini discovery smoke test was blocked by sandbox network policy;
  retry with approved outbound access. The first Web syntax command failed
  only because its shell regex escaping was malformed.
- Codex Security dry-run reached input validation but rejected `/tmp` as an
  untrusted output parent; use a private owner-controlled directory instead.
- The scanner also rejected output under the workspace because its parent
  `/home/nuc/NA2H` was mode `775`; tighten that aggregate directory to `755`
  so the scan output path satisfies the tool's privacy check.
- The sandbox maps `/` and `/home` to an untrusted owner for the scanner's
  ancestry check. Running the same official dry-run outside the sandbox with
  a private `/tmp` output directory passed; this is an environment constraint,
  not an application finding.
- The first live scan selected the package default `gpt-5.6-sol`, reached
  24/25 files, and was canceled before the final report. The estimated cost at
  cancellation was about `$6.31`; partial output is not evidence of a clean
  or failed security result.
- The official CLI exposes `--model`, but its built-in provider choices are
  OpenAI, OpenRouter, Fireworks, and Amazon Bedrock. `lunamax` is not present
  in the local EMP or Codex configuration, so using it requires an exact
  model slug and a supported route rather than guessing.
