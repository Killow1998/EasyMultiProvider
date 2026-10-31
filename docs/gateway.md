# EMP gateway API

Open **Settings → General → Gateway connection** to copy the API base URL and
EMP Gateway Key. API clients send `Authorization: Bearer <EMP_GATEWAY_KEY>`.
For another computer, use the address exposed by your port forward.

The gateway key belongs to the EMP instance and remains valid when Native
sign-in changes. EMP forwards requests with the selected service's saved
credentials, not the caller's gateway key. This key grants model API access;
WebUI management still uses its own session.

Codex sign-in is separate. EMP creates an **EMP local key** in `auth.json` only
when Codex has neither a subscription nor an OpenAI API key. An existing real
sign-in is preserved. **Change sign-in method** offers subscription sign-in
(device code or browser), your own OpenAI API key, an imported Other
subscription, or an EMP local key. The icon sits immediately before Refresh in
the Native service's action group. Applying an Other subscription keeps its
imported record. Switching a subscription to an EMP local key offers keeping it
as Other or removing its Native sign-in. Restart Codex Runtime after switching.

## Decision models

Add an external API service with its own API key, then open
**Settings → External services → Decision models**. Choose the default
service, then choose a decision model from that service's catalog. EMP matches
the API type for the official OpenAI, OpenRouter and TypeSafe hosts; compatible
relays can select the corresponding API type. **Refresh list** reloads the catalog.
The dialog keeps loaded lists during service and language changes. Read failures
and empty catalogs are shown directly; an existing saved model is retained.
**Other model (enter manually)** remains available for custom endpoints.
Preferences belong to each service. Clearing the default keeps those preferences
and lets clients select a service on each call.

OpenRouter decision catalogs require `GET /v1/models?output_modalities=decisions`:
the default text catalog does not list these models. TypeSafe supplies available
names through `GET /v1/models` under `models[].name`. EMP reads the configured
service with its saved key; the WebUI never needs a second key entry.
[OpenRouter model filtering](https://openrouter.ai/docs/api/api-reference/models/get-models),
[TypeSafe model catalog](https://docs.typesafe.ai/api/list-models).

| API type | EMP endpoint | Upstream endpoint | Native request format |
|---|---|---|---|
| OpenAI Decisions | `POST /v1/decisions` | `/v1/decisions` | `input`, question array |
| OpenRouter Decisions | `POST /v1/decisions` | `/api/alpha/decisions` | `state`, question map |
| TypeSafe / SystemOne | `POST /v1/systemone` | `/v1/systemone` | `state`, question map |

Sources: [OpenAI Decisions](https://developers.openai.com/api/docs/guides/decisions),
[OpenRouter Decisions](https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-request),
[TypeSafe SystemOne](https://docs.typesafe.ai/api/system-one).

OpenAI Decisions uses an OpenAI API service key, independently of a ChatGPT
subscription. Its documented model is `gpt-6-luna`, offered when the account's
catalog contains it. OpenRouter lists qualified decision IDs such as
`~typesafe/jev-latest`, including non-Jev decision models. TypeSafe lists
`jev-latest` and available explicit versions. Text models are excluded from this
list; decision preferences do not add models to the Codex conversation picker.
These settings do not enable the experimental Auto effort feature.
[TypeSafe models](https://docs.typesafe.ai/models)

`typesafe/jev-router` is a text-output routing model, separate from Jev's decision
endpoints. It remains eligible for the conversation picker when imported as a
conversation model and is excluded from the decision-model choices.

Clients authenticate with the EMP Gateway Key. They can omit `model` to use the
saved default, or supply a model ID for that request. `X-EMP-Provider` overrides
the default service; its value is the service ID from the EMP configuration.
When there is no default, the header is required.

```sh
curl "$EMP_API_BASE/decisions" \
  -H "Authorization: Bearer $EMP_GATEWAY_KEY" \
  -H 'Content-Type: application/json' \
  --data-binary @decision.json
```

For OpenAI Decisions, `decision.json` can contain:

```json
{
  "input": "A public example message",
  "questions": [
    {"name": "urgent", "type": "predicate", "instructions": "Is the message urgent?"}
  ]
}
```

For Jev, use the selected API's native format instead:

```json
{
  "state": {"message": "A public example message"},
  "questions": {
    "urgent": {"type": "noul", "instructions": "Is the message urgent?"}
  }
}
```

EMP preserves request fields, adds a missing model from settings, and returns
the upstream response body, status and `Retry-After`. It sends one upstream
request without automatically retrying. OpenAI question arrays and Jev question
maps are distinct contracts; EMP does not translate between them.

For TypeSafe SDK JSON calls, choose **SystemOne**. Set the SDK base URL to the
EMP origin, such as `http://127.0.0.1:4200`, because the SDK appends
`/v1/systemone`. The same protocol choice works for OpenRouter's SystemOne
compatibility endpoint when the saved service base URL is
`https://openrouter.ai/api/v1`. EMP forwards the documented
`TypeSafe-Danger-Zone-Long-Context` and `TypeSafe-Danger-Zone-Images` headers.
This gateway handles JSON requests; multipart uploads use a different wire format.
[OpenRouter TypeSafe SDK](https://openrouter.ai/docs/guides/community/typesafe-sdk)

Saved preferences use the existing configuration structure:

```json
{
  "decision_provider": "typesafe",
  "providers": [{
    "id": "typesafe",
    "base_url": "https://api.typesafe.ai/v1",
    "auth_mode": "api_key",
    "decision_api": {"protocol": "systemone", "model": "jev-latest"}
  }]
}
```

Existing clients with `X-EMP-Provider` and an explicit model still work without
settings. Official OpenAI and TypeSafe hosts infer their native protocol;
other unconfigured services retain OpenRouter Decisions routing. Select an API
type explicitly for relay URLs.
