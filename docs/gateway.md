# EMP gateway API

Open **Settings → General → Gateway connection** to copy the API base URL and
EMP Gateway Key. API clients send `Authorization: Bearer <EMP_GATEWAY_KEY>`.
For another computer, use the address exposed by your port forward.

The gateway key belongs to the EMP instance and remains valid when Native
sign-in changes. EMP forwards requests with the selected service's saved
credentials, not the caller's gateway key. This key grants model API access;
WebUI management still uses its own session.

### Devices and usage

Create a named device key in **Gateway connection** and copy it once into the
client's Bearer credential. Only its SHA-256 digest and metadata are saved. The
existing shared key remains available. Named keys authenticate model APIs and
cannot manage EMP. Revocation rejects subsequent HTTP calls and the next
generation on an existing WebSocket; it does not replay an in-flight request.

**Caller usage** groups calls by key, operation, service/account and model. The
peer address comes from the socket, not caller-supplied forwarding headers. Use
different keys for different devices: a tunnel can give several clients the
same visible peer address, while a shared key combines their usage. Tokens use
reported counters, retaining unknown and reported zero as different values.
USD is an API-price estimate for calls with known pricing, not subscription
quota. Decision calls have their own operation and do not enter conversation
bills or generation TPS. A Decision disconnect cancels its upstream request.

### Aggregate routing

Open **Settings → Native services → Aggregate routing**. Choose a model, its
sources, a display prefix and a policy. The model list includes those with at least
two enabled sources, using their display names and source counts. EMP generates
the internal `auto/…` identifier and preserves it when the prefix changes. The
default display prefix is `auto`, displayed as `autoModel` with no added
separator. An empty prefix shows just the original model name. The original model name stays unchanged and
subsequent model renames are reflected in the aggregate display too.
Different models can share a prefix. Multiple aggregate routes for the same model
must use distinct prefixes; rejected saves preserve the form and configuration.
Each route can use a different policy:

| Policy | New conversation allocation |
|---|---|
| Priority | First available source in the configured order |
| Quota balance | Source with the most remaining headroom within the same subscription plan |
| Round robin | Next available source in the rotation |

Existing conversations stay on their actual account, including after a Native
account moves to Other. Reordering affects new conversations. Known exhausted,
disabled and cooling-down sources are skipped; a failed upstream request is
not replayed against another account. A later request can select another source.
The alias exposes common capabilities and the smallest context window of its
sources. Manual model entries remain available. Gateway keys and aggregate
preferences belong to the gateway instance, independently of service migration.

Codex sign-in is separate. EMP creates an **EMP local key** in `auth.json` only
when Codex has neither a subscription nor an OpenAI API key. An existing real
sign-in is preserved. **Change sign-in method** offers subscription sign-in
(device code or browser), your own OpenAI API key, an imported Other
subscription, or an EMP local key. The icon sits immediately before Refresh in
the Native service's action group. Applying an Other subscription keeps its
imported record. Switching a subscription to an EMP local key offers keeping it
as Other or removing its Native sign-in. Restart Codex Runtime after switching.

### Codex image tools with an EMP local key

EMP enables one `emp_images.imagegen` MCP tool when Native is Free or uses an
EMP local key. The same tool generates images and edits local PNG, JPEG, WebP
or GIF files. Paid Native subscriptions keep Codex's native image tools.

In **Settings → Routing settings**, choose a subscription with image quota.
Save and restart Codex Runtime to load the tool. Calls follow Codex's current
MCP permission settings and use the selected subscription's quota. The first
call can require authorization in Codex.

Free Native stays signed in. EMP keeps `openai`, the original conversation IDs,
and the existing permission/sandbox settings; no fork is needed. The helper is
the existing EMP executable over stdio, so no additional runtime is installed.
It reads reference files on the client, forwards the current Codex turn metadata
to the image route, and saves images under the client's EMP `state/generated_images`
directory. Tool results contain only a file path, not image Base64 in text.

EMP owns only its `mcp_servers.emp_images` entry. Switching to paid Native or
restoring native integration reinstates the original entry and keeps other
MCP servers unchanged.

Cancellation closes the helper's HTTP request and the gateway's upstream image
connection. The helper remains responsive to MCP messages while generating.
Closing its stdin also cancels active image requests.

### Image 2.6 API

Use `POST /v1/images/generations` or `POST /v1/images/edits` with the gateway key
and `X-EMP-Provider: <service-id>` to select a configured API-key service.
The gateway uses that service's stored key; a Codex turn is not required.
Without this header, existing Codex image calls retain their per-turn
subscription routing. An unknown or disabled service is rejected.

Example JSON for `generations`:

```json
{
  "model": "gpt-image-2.6-sunburst",
  "prompt": "A simple landscape icon",
  "output_format": "svg",
  "complexity": "low"
}
```

`gpt-image-2.6-sunburst` and `gpt-image-2.6-flare` support the Image 2.6 options.
The API forwards the original JSON and upstream response. SVG is generation-only.
GIF supports generation and editing, with `frame_count` (2–16) and
`frame_duration_ms` (20–2000). SVG/GIF do not support streaming, partial images
or output compression. The selected service must provide the requested model.
[OpenAI image generation guide](https://developers.openai.com/api/docs/guides/image-generation).

The existing MCP `imagegen` tool accepts these model names, `output_format`,
`complexity`, `frame_count` and `frame_duration_ms`. PNG remains its default;
SVG, GIF, JPEG and WebP results use matching file extensions. Selecting a model
does not change the configured subscription account or create API billing credentials.

### Responses client compatibility

Codex clients identify themselves through `originator` or `User-Agent` containing
`codex` and retain Codex extension events. Other Responses clients receive no
`codex.*`, `response.metadata` or JSON `keepalive` events. HTTP uses SSE comments
for these frames; WebSocket omits them. Upstream usage processing runs before
this output filtering.

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
