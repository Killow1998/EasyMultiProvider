# EMP connection frontend

An independent Web UI and standalone product page on the exact v0.12.11 backend
baseline `a568d005b4fc32793ec8e06db7d5891324a1c142`. This branch provides the
frontend source for review; it has no independent installer or release.

See [build and preview instructions](site/BUILD.md).

## Layout and data boundaries

- `crates/emp-app/web/index.html`: connection workspace, passive source details,
  staged account import and existing management UI. The connection code is an
  inline script because the original Rust asset allowlist is fixed. No new
  backend endpoint or asset route is required.
- `crates/emp-app/web/style.css`: shared management styling, dark mode, responsive
  layouts, and focus states. Existing feature modules and their behavior remain.
- `frontend/site`: standalone product page with a labeled synthetic diagram,
  three-step explanation, FAQs, and accurate source/build availability.
- `frontend/tests`: DOM integration tests against the actual shipped page;
  deterministic fake APIs prohibit generation calls. The same fixture schema
  powers the loopback-only preview server.
- `frontend/evidence`: screenshots of the actual UI using synthetic data.

Source details do not call quota refresh, model discovery, generation, or Codex
verification. Status refresh reads `/api/integration`; only an explicit confirmed
catalog check invokes the existing reload operation. Existing quota/event,
diagnostics, usage and update mechanisms remain. No new periodic helper,
resource/plugin download, or model probe has been added.

Account nicknames use the existing `name` field. New technical IDs and prefixes
are generated without collisions and validated as the backend requires. Existing
IDs and prefixes remain unchanged. Replacing credentials is an explicit action
and retains the existing visibility/context settings. If catalog synchronization
fails after import, the saved account remains and only synchronization is retried.

## Verification commands

```sh
node --test crates/emp-app/tests/web_features_contract.cjs
npm --prefix frontend test
npm --prefix frontend run build
npm --prefix frontend run verify:baseline
git diff --check
```

`verify:baseline` hashes every original tracked file except the two edited Web
assets and compares it with the pinned commit. It verifies Rust sources, Cargo
manifests/lockfile, tests, workflow files and backend configuration code byte for
byte. New files are confined to `frontend/`.

Browser checks and limitations are recorded in [SELF-TEST.md](SELF-TEST.md).
No claim is made that the original backend's restart/restoration issue is fixed.
