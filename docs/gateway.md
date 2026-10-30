# EMP gateway API

Open **Settings → General service settings → Gateway connection** to copy the API base URL and
EMP Gateway Key. API clients send `Authorization: Bearer <EMP_GATEWAY_KEY>`.
For another computer, use the address exposed by your port forward.

The gateway key belongs to the EMP instance and remains valid when Native
sign-in changes. EMP forwards requests with the selected service's saved
credentials, not the caller's gateway key. This key grants model API access;
WebUI management still uses its own session.

Codex sign-in is separate. EMP creates an **EMP local key** in `auth.json` only
when Codex has neither a subscription nor an OpenAI API key. An existing real
sign-in is preserved. **Change sign-in method** offers subscription sign-in
(device code or browser), your own OpenAI API key, and an imported Other
subscription. Applying an Other subscription keeps its imported record. Restart
Codex Runtime after switching sign-in.

## Jev Decisions

Configure an OpenRouter service with its API key. Jev uses
`POST /v1/decisions` and is excluded from the Codex model picker.
`X-EMP-Provider` selects the configured service ID. Use the `provider` ID already
present on that service's models in the EMP configuration.

```sh
curl "$EMP_API_BASE/decisions" \
  -H "Authorization: Bearer $EMP_GATEWAY_KEY" \
  -H "X-EMP-Provider: $EMP_PROVIDER_ID" \
  -H 'Content-Type: application/json' \
  --data-binary @decision.json
```

`decision.json` is an OpenRouter Decisions request with a `typesafe/jev-*` model.
EMP forwards it to the selected service's `/api/alpha/decisions` endpoint and
returns the upstream JSON and status. Each call sends one upstream request.
Jev does not enable the separate experimental Auto effort feature.
