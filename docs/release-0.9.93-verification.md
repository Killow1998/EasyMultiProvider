# 0.9.93 verification

- Windows native PowerShell 7.6.5: 1,013 unittest cases, passed with 11 existing platform/integration skips.
- Request-source regression: a hidden model request retains its client-claimed session ID and shares the HTTP request ID; arbitrary headers and conversation text are excluded.
- Ablation: replacing proxy identity with a constant causes the live system-proxy change regression to fail. Restoring it passes. Keep this mechanism; it prevents connection reuse across proxy endpoint changes.
- The request-source fields extend the existing journal and observation ring. No separate tracing service, session database, or background history scanner is added.
- Actual Clash rule/global/TUN switching is not covered by this simulated proxy regression.
- Native package update/rollback checks run in the release packaging workflow for each platform. The running local installation is not replaced by this release operation.
