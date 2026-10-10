# Shadow frontend

Preview frontend changes against a running EMP without changing its configuration
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
The default follows EMP's platform configuration directory, or
`EASY_MULTI_PROVIDER_CONFIG`. Run as the user who owns those files. Close the
server with Ctrl+C.

The server forwards only listed GET endpoints. It reads usage SQLite data in
read-only mode and creates a separate preview session. Config edits stay in this
origin's browser storage; credential fields are removed before persistence.
Starting/restoring EMP, updates and model requests are disabled in the preview.
Refreshing quota or models reads existing data instead of contacting providers.
Use **Demos** to preview all service activity or error indicators with sample data.

For frontend acceptance, use this preview before replacing the running UI.
Check English and Simplified Chinese, light and dark themes, zoom/narrow layouts,
keyboard navigation, loading/error states and closing nested dialogs. Preview
storage is separate from the backend; resetting site data discards drafts.

## Source ownership

Official and preview pages share `crates/emp-app/web/` presentation modules:
statistics, usage data, quota styles, brand icons, errors, motion and interactions.

- `shadow-web.py`: loopback HTTP permissions and read-only backend access.
- `frontend.py`: shared assets plus preview isolation hooks.
- `shadow-drafts.js`: preview-only edits and credential scrubbing.
- `shadow-ui.js`: preview command guards and demo integration.
- `shadow_usage.py`: read-only statistics fallback for older installed backends.
- `shadow-service-demo.js`: sample services and demonstration state.

`contracts/call-outcomes.sql` is the same read projection used by Rust reports.
Changing call classification must keep live receipts and that projection aligned.

## Validation

```sh
node --test crates/emp-app/tests/web_features_contract.cjs
python3 scripts/shadow_frontend/e2e.py --emp /path/to/EMP
```

E2E requires Firefox and geckodriver on PATH. It creates a temporary backend,
synthetic upstream and browser profile, then removes them on exit. It uses no
production ports, accounts or paid model calls. It verifies real HTTP failures,
report classification, browser dialogs, preview drafts and blocked backend writes.
