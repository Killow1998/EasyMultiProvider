# Shadow frontend

The v0.13.2 product flows are embedded from `crates/emp-app/web` in the normal
executable. The review overview, fault selector and mock lifecycle actions stay
in this development directory and are injected only by the Shadow launcher.
`e2e_formal.py` checks the actual executable rejects these development routes;
`e2e_startup_recovery.py` checks real reset and exit against disposable data.

Use one entry point for current candidate previews:

```sh
python3 scripts/shadow_frontend/shadow-web.py --emp /path/to/candidate/EMP --port 4201
```

Open `http://127.0.0.1:4201/`. **Changes** (`/?review=1`) lists the ten candidate
adjustment groups since v0.13.1: six frontend flows (five revised flows and the
new recovery page) plus four backend groups. agy is separate. The six frontend
entries cover migration navigation, merged account display, import preference
conflicts, removal progress, Restore Native versus Quit, and key recovery.

The **Demos** menu contains this overview, **Key recovery**, **Restore Native /
Quit**, **Remove account**, **Existing preference conflicts**, **Import the same
account**, **All services running**, and **All service errors**. These use the
same production frontend modules and a disposable synthetic workspace. Return
from a service demo to the same workspace; there is no separate import-demo URL
or launcher. Production backend configuration and credentials are not read.

For an import, choose **Import the same account**, then **Open package**. Resolve
visibility/context conflicts in the preview and click **Import** once. Saves
apply only to the disposable backend. Service activity/error galleries are
read-only. Restarting Shadow resets its temporary data. Ctrl+C stops it.

`/recovery` serves the actual candidate recovery HTML with a preview adapter.
Its six scenarios include missing/invalid/mismatched keys, key permissions,
invalid settings and an interrupted reset. All offer clear-and-set-up or exit;
there is no retry or manual file-repair step. Optional backup (unchecked by
default), reset and exit are simulated and repeatable;
no files are cleared. The simulated lifecycle flow keeps the disposable server
alive even after Quit. Removal operates on a synthetic account and adds a
clearly announced 0.7-second delay to make progress visible. These are UI
demonstrations, not Windows runtime acceptance.
Existing preference conflicts populate a synthetic duplicate in this same
workspace and open the actual model settings dialog; saving affects only that
workspace. The import demo exercises the current preference choices before import.

Verify the new recovery/lifecycle/removal review flows with:

```sh
python3 scripts/shadow_frontend/e2e_preview_review.py \
  --emp /path/to/candidate/EMP --output /tmp/emp-shadow-review.json
```

Validate that flow with:

```sh
python3 scripts/shadow_frontend/e2e_account_merge_demo.py \
  --emp /path/to/candidate/EMP --output /tmp/emp-import-merge.json
```

The focused production-asset migration/account acceptance runs against disposable
EMP backends and synthetic credentials, with no real model calls:

```sh
python3 scripts/shadow_frontend/e2e_migration_navigation.py \
  --emp /path/to/candidate/EMP --output /tmp/emp-migration-acceptance.json
```

It requires Firefox and geckodriver. It covers migration guidance, direct-parent
Back, cancellation, an encrypted package import, same-account service merging,
visibility/context conflict choices, and removal of a merged imported service.
It retains coverage for existing historical preference conflicts; new import
conflicts are covered by the import-stage acceptance above.
The same launcher also supports the optional read-only live-data mode below.
For ordinary candidate review, use the disposable workspace above.

For read-only inspection of existing local data, preview frontend changes against
a running EMP without changing its configuration
or interrupting requests. Python 3.10+ is sufficient; no packages are required.

From the repository root:

```sh
python3 scripts/shadow_frontend/shadow-web.py --open
```

The preview listens on `http://127.0.0.1:4201`. To use an isolated backend:

```sh
python3 scripts/shadow_frontend/shadow-web.py \
  --port 4201 --backend-port 4202 --state-dir /path/to/isolated/state
```

The state directory contains the backend's `web-session.json` and `usage.sqlite3`.
Usage labels resolve the same account fingerprint across native/subscription
categories. Unidentified fingerprints are not shown as account names. History
route prefixes remain explicitly unlinked unless a matching turn and route have
one verified realtime account; matching aliases or emails alone do not prove identity.
The default follows EMP's platform configuration directory, or
`EASY_MULTI_PROVIDER_CONFIG`. Run as the user who owns those files. Close the
server with Ctrl+C.

The server forwards only listed GET endpoints. It reads usage SQLite data in
read-only mode and creates a separate preview session. Config edits stay in this
origin's browser storage; credential fields are removed before persistence.
Starting/restoring EMP, installing updates and model requests are disabled in the preview.
Check updates reads public release notes before any installation decision; the
shadow bridge caches this bounded metadata for 15 minutes and never starts an update.
Chinese and English notes use the release body's maintained language sections.
The verified 0.12.18 English body has a maintained Chinese rendering; other missing
translations show an explicit notice rather than unrelated or guessed content.
Refreshing quota or models reads existing data instead of contacting providers.
Use **Demos** to preview all service activity or error indicators with sample data.

Settings place Quota display immediately after Quota style; service display labels
use Name. Internal identifiers and saved schema remain unchanged.
Connection labels omit optional/required annotations. A saved API key displays
six stars as a placeholder; focus clears the placeholder, and replacement input
is visible for checking. Keeping the input empty preserves the saved key;
closing after entering a replacement discards it. Only Save replaces the key.

The candidate account layout, context explanations, centered quota rings and
stronger light/dark activity halos live in the shadow assets. Model settings use
one scroll owner in the dialog body, keeping navigation and Save visible.
All non-Native services put Remove at the right of their tab row; Native has no
Remove. Subscription import asks for a name; its internal reference is allocated
by the backend. Service connection forms no longer ask for local IDs. Model-source
labels use service names; upstream model identifiers remain where needed.
Provider models are managed from their service: visible entries sort first,
search remains available, and each row has visibility, edit and test icons.
Manual add lives in the same dialog. Its header Back arrow returns to the model
manager with the search retained; X closes the dialog. Shared dialog navigation
uses explicit parent callbacks: add-service forms return to their chooser,
subscription paste/file forms return to import choices, and quota confirmation
returns to credit selection before returning to the service's previous tab.
Back and X use transparent controls without borders. Call-outcome Back removes
only the overlay and preserves the report filters; X closes the report.
Subscription rows show the
model name and usable context without repeating the upstream ID. Statistics no
longer links to the retired History and pricing window. Homepage model lists and service-wide
visibility buttons are hidden in the preview; model tests never issue real requests.
Subscription visibility also uses icons; global context help and each row's live
calculation separate catalog limits, current settings and usable context. The far-right
straight opposing-arrow button marks that model's visibility and context for copying
to the other Codex subscriptions, including Native. It aligns with the visibility icon.
**Save changes** applies the latest values to all marked models in one save; closing
discards the synchronization. A blank context restores the catalog default everywhere.
Opening model settings never replaces chosen contexts with catalog maxima. Reset
actions and reset-count summaries use a physical key icon; refresh keeps its arrows.
**Data migration**
replaces the two transfer buttons here. Both selection and result lists show service
names and types, with model choices folded by default. The left selection keeps
source names read-only; the right preview lets users edit names for the incoming
services, including verified merges. A single **Import** saves the selected
services and edited names together. Closing discards edits; unchanged or empty
names retain the proposed name. Matching names remain allowed, and duplicate-name
bubbles disappear when the edited name no longer collides.
API, OpenRouter and Claude CPA share the External API service classification and
merge explanation; brand icons and connection methods remain distinct. Local
Claude sign-in remains a subscription and requires verified identity to merge.
Internal IDs remain in the encrypted schema and route references but are not shown
as service identity. The local Native login exports under a random four-character
reference while retaining its display name; other references are preserved unless
identity matching or collision avoidance remaps them. Optionally carry Codex
subscription context settings and model display names.
Imports save only shadow drafts. Yellow bubbles explain verified identity merges,
separate-service imports and matching names. Matching names neither prevent import
nor cause a merge. The import tab's **Preview an example import** uses
synthetic collisions and supports the same inline name editing. Its **Import**
closes without saving; it requires no migration file.
The question mark beside **Data migration** documents the common merge policy.
Password labels also have question marks: exports require 12–4096 trimmed UTF-8
bytes and matching entries; imports retain legacy 8-byte compatibility. Password
validation messages appear only after submitting invalid values. Provider identity
matching does not require a local ID collision: connection method, trusted current
base URL and nonempty API key must match incoming values; different or unverifiable connections map to a new ID, retaining
the original. Their model routes and presentation aliases are mapped together.
Cached private preview credentials allow repeated imports to reuse the same
verified connection without exposing keys to the browser.
Export previews show the new computer's Native login plus the imported services;
exporting creates an encrypted `.emp` download.

Migration preview uses the existing Rust migration codec and identity merge rules.
Build its read-only helper once (normal page navigation needs only Python):

```sh
cargo build -p emp-state --example shadow_migration_codec
```

On the local development workspace, use `.emp-cargo.sh` as usual. The bridge finds
the standard debug example path; `--migration-helper /path/to/shadow_migration_codec`
selects another build. `--config` chooses the read-only source configuration.
No production import/export endpoint is called. Only selected live credentials
are read for an explicitly requested export. Passwords and decoded credentials
stay in local process memory; the browser receives redacted previews and opaque
tokens. Up to four decoded packages are retained for 30 minutes to allow draft
re-export; restarting the shadow server clears them. Reopen the original package
if a saved draft's token has expired. Closing or resetting browser drafts does not
modify production credentials. The existing scrypt codec needs temporary memory
and can take several seconds in debug builds; this work runs only on migration,
never on quota refresh or service animation.

For frontend acceptance, use this preview before replacing the running UI.
Check English and Simplified Chinese, light and dark themes, zoom/narrow layouts,
keyboard navigation, loading/error states and closing nested dialogs. Preview
storage is separate from the backend; resetting site data discards drafts.

After Xian accepts a completed shadow change, summarize it and ask whether to
migrate it into the official frontend, unless that migration is already authorized
for the current scope. See the [frontend development constraints](../../docs/architecture.md#frontend-development-and-acceptance)
for the acceptance workflow and the permanent `233%` / `1m111s` unlimited-5h design.

## Source ownership

Official and preview pages share `crates/emp-app/web/` modules, including migration,
model management, subscription preferences, connection drafts and updates. Each
controller owns its modal requests and cancellation. The preview supplies isolation
and demonstration adapters; it no longer carries duplicate accepted UI assets.

- `shadow-web.py`: loopback HTTP permissions and read-only backend access.
- `frontend.py`: shared assets plus preview isolation hooks.
- `shadow-drafts.js`: preview-only edits and credential scrubbing.
- `shadow-ui.js`: preview command guards and demo integration.
- `crates/emp-app/web/model-manager.js`: shared service-scoped model operations and editor navigation.
- `shadow_usage.py`: read-only statistics fallback for older installed backends.
- `shadow-service-demo.js`: sample services and demonstration state.
- `preview-review.js`: candidate change inventory, synthetic removal/preferences
  and simulated lifecycle actions using production dialogs.
- `preview-recovery.js`: simulated responses around the production recovery HTML;
  `candidate_web.py` blocks recovery API forwarding as a second boundary.
- `crates/emp-app/web/service-presentation.js/css`: accepted account/context/feedback presentation.
- `crates/emp-app/web/migration.js`: shared dialog state, requests, debounce and cancellation.
- `crates/emp-app/web/migration-view.js` and `migration.css`: service/model projection, markup and scoped layout.
- `migration_selection.py`: pure selected-package projection, combination, redaction and action notices.
- `shadow_migration.py`: local private identity cache and codec orchestration.
- `shadow_release_notes.py`: bounded read-only release metadata and a short-lived cache.
- `crates/emp-state/examples/shadow_migration_codec.rs`: read-only production codec
  and merge-rule reuse; no network, credential persistence or configuration writes.

Account information and lifetime usage renderers are passed at page composition;
the preview does not cut or replace component function bodies. Service navigation
selects tabs by their explicit view identity. Migration previews merge against
the current shadow draft and use cached private account/provider identities for repeated
imports, retaining prior edits and credentials. Export selections are grouped by
their credential source before combining, so unrelated packages cannot contribute
another service's key. Migration containers own their grid and scrolling styles;
they do not inherit the page-panel `section` layout.

`contracts/call-outcomes.sql` is the same read projection used by Rust reports.
Changing call classification must keep live receipts and that projection aligned.

## Validation

```sh
node --test crates/emp-app/tests/web_features_contract.cjs
python3 scripts/shadow_frontend/e2e.py --emp /path/to/EMP
python3 scripts/shadow_frontend/test_migration_selection.py
python3 scripts/shadow_frontend/e2e_refinements.py --emp /path/to/EMP \
  --helper /path/to/shadow_migration_codec
python3 scripts/shadow_frontend/e2e_model_manager.py --emp /path/to/EMP \
  --output /tmp/shadow-model-manager.json
python3 scripts/shadow_frontend/e2e_navigation.py --emp /path/to/EMP \
  --output /tmp/shadow-navigation.json
python3 scripts/shadow_frontend/e2e_subscription_sync.py --emp /path/to/EMP \
  --output /tmp/shadow-subscription-sync.json
```

E2E requires Firefox and geckodriver on PATH. It creates a temporary backend,
synthetic upstream and browser profile, then removes them on exit. It uses no
production ports, accounts or paid model calls. It verifies real HTTP failures,
report classification, browser dialogs, preview drafts and blocked backend writes.
The refinements test also verifies actual Firefox fullZoom at 150% and 170%,
normal animation frames in both themes, selected imports and selected re-export
with synthetic credentials. Browser system access is enabled only on this
temporary Firefox instance to set its zoom; no user browser is attached.
