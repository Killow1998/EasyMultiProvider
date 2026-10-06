# EMP independent frontend preview

This preview is local source work. It has not been committed, pushed, published,
or packaged as an installer. Upstream release downloads do **not** contain this UI.

Backend baseline: v0.12.11, commit
`a568d005b4fc32793ec8e06db7d5891324a1c142`.

## Run the isolated preview

From the repository root, with Node.js 20.19+, 22.12+, or 24+:

```sh
cd frontend
npm ci --ignore-scripts
npm test
npm run build
npm run preview
```

Open `http://127.0.0.1:4318/` for the management UI with synthetic API responses.
Open `http://127.0.0.1:4318/site/` for the standalone product page.
The scenario selector provides matched, pending, native, empty, stale, failed,
conflict, many-source, offline and stopping states. The stopping scenario
automatically accepts confirmation only in the synthetic preview, so browser
automation can inspect the subsequent stopping/disconnected states. Production
confirmation is unchanged. `tests/fixtures/demo-signin.json` is a
synthetic import fixture, not usable credentials. Never import real credentials
into the preview. The preview has no EMP process or upstream model connection.
Use `EMP_FRONTEND_PORT` to choose another port when needed.

`npm run build` writes `dist/management` and `dist/site`. The static product page
is self-contained and can also be served directly from `frontend/site` with a
static HTTP server. Building does not publish either directory.

## Build the application from this checkout

The original Rust build command still applies:

```sh
cargo build --locked --release -p emp-app --bin EMP
```

Rust embeds `crates/emp-app/web/index.html`, `style.css`, and the existing JS
modules. There is no frontend build prerequisite for the executable and no new
asset route or API. Do not run the executable against a real Codex configuration
as part of frontend testing.

This delivery has no independent installer, Release, tag, or update channel.
The retained update screen uses the original upstream channel. Installing an
upstream update can replace this UI and the pinned backend; it is not an update
for this frontend preview.

## What the UI verifies

- Adding credentials is distinct from applying settings.
- A matched shared model catalog is distinct from a successful request.
- Unknown or stale quota stays unknown; existing observations show their time.
- Exit acceptance means stopping. Disconnection does not prove safe exit or
  usable native restoration.
- No frontend verification here establishes context continuity, tool-call
  continuation, or recovery of the reported backend restart issue.
