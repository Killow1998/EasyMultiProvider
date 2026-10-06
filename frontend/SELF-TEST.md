# Local frontend self-test

Recorded: 2026-10-06 17:07 Asia/Shanghai (2026-10-06 02:07 America/Los_Angeles).

Baseline: v0.12.11 commit `a568d005b4fc32793ec8e06db7d5891324a1c142`, branch
`frontend/account-connections-v01211`. v0.12.11 does not change
`crates/emp-app/web`; the frontend changes carried over from v0.12.10 without
conflicts. Backend response fields used by the page are unchanged.

## Automated results

| Check | Result | Coverage |
| --- | --- | --- |
| Original Web feature contracts | 6 passed | Login/session handling, failed settings saves, request/diagnostics lifecycle, shipped script order |
| Frontend DOM and site tests | 12 passed | Passive details, catalog evidence boundaries, import recovery/collisions, stable routes, quota/XSS handling, many sources, keyboard/bilingual behavior, shutdown uncertainty, load failure, retained dialogs and site assets |
| Static build | Passed | Inline scripts compile; management and standalone site outputs generated locally |
| Baseline file hashes | 521 matched | Every original tracked file except the two edited Web assets, compared byte for byte with v0.12.11 |
| Diff whitespace | Passed | `git diff --check` clean |

## Visual redesign

The connection workspace follows a product-panel style: tight, heavy headings;
neutral hairline borders; a single white (or dark) panel for the connection
map; sources as a divided list with pill status labels; neutral wires; a
segmented view switch; and one bordered status card. Dot-grid backgrounds,
colored wires and dashed boxes from the earlier draft were removed.

## Browser results

The preview binds only to `127.0.0.1:4318` and uses synthetic data.

- Desktop light and dark: connection panel, source list, status card and
  header version (v0.12.11) reviewed.
- 390px mobile: document scroll width equals the viewport (390px); the
  connection map uses two columns and the source list fits (324px).
- Earlier checks (modal focus, import, catalog states, shutdown uncertainty,
  offline state) are covered by the DOM tests above and were not repeated in
  the browser for this revision.

Screenshots: [desktop](evidence/connections-desktop.jpg),
[dark](evidence/connections-dark.jpg),
[mobile](evidence/connections-mobile.jpg). Other evidence files predate this
revision.

## Limits

- Cargo is unavailable on this host. No Rust build or native desktop runtime
  test was performed.
- No real EMP process, Codex configuration, credentials or provider was used.
- Original upstream packages do not include these frontend changes, and the
  retained updater can replace them.
