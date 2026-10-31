# Official Model Capability Registry

Status: implemented and independently verified

## Objective

EMP keeps a small, release-bundled registry of official Provider contracts and
notable model capabilities. The registry fills gaps left by incomplete model
list endpoints; it does not replace live discovery and it never creates a route
for a model that the configured upstream did not advertise or the user did not
add.

This reduces false `text-only`, unknown-context, and missing-reasoning metadata
without claiming that stale static data is live Provider truth.

## Domain boundaries

An **API Provider** owns an endpoint, authentication shape, and wire protocols.
A **model publisher** owns a model family and model card. An aggregator such as
OpenRouter is an API Provider but does not become the publisher of every model
it routes. A self-hosted deployment is neither assumed to match the publisher's
official limits nor enriched from a family-name guess.

The registry covers notable models from OpenAI, Google Gemini and Gemma,
Anthropic, xAI, Meta, Moonshot AI, DeepSeek, Zhipu AI, and Inception. OpenRouter contributes
its official Provider contract, while its live `/models` response remains the
source for its changing model catalog.

The Inception preset uses its Chat Completions endpoint. Mercury 3 defaults cover
its documented 1,000,000-token context, 131,072-token output limit, text modalities,
streaming, tools, structured output, and its documented instant/low/medium/high
efforts. Codex 0.163 accepts model-advertised effort strings; EMP preserves them
when projecting the model catalogue and the Chat Completions request.

## Truth precedence

For each capability field, highest priority wins:

1. user manual override;
2. observed runtime success or explicit failure boundary;
3. live advertised model metadata;
4. release-bundled official registry;
5. conservative unknown/default.

A refresh may update an older advertised or official value. It must not replace
manual or observed state, visibility, deployment identity, or context
calibration. Missing metadata never proves support. Unknown input capability
therefore remains text-only in the Codex catalog.

## Data model

Provider records contain:

- stable key, display name, and role;
- exact official API roots and model-list endpoint when public;
- authentication mode;
- supported EMP wire protocols and preferred protocol;
- endpoint-specific model redirects when the official API documents them;
- source URLs and last review date.

Model records contain only fields confirmed for an exact model ID:

- context and output token limits;
- input and output modalities;
- reasoning support and exact selectable levels where documented;
- streaming, tool calling, parallel tools, and structured output when known;
- supported protocols, release/status information, aliases;
- field-level official source URLs.

The registry may retain modalities that Codex cannot currently send or render,
including audio and video. The generated Codex catalog still exports only the
modalities supported by Codex. `supports_image_detail_original` remains
independent from image-input support.

## Discovery merge

Official endpoints are identified by exact registered API roots. Live `/models`
fields are normalized first, including top-level snake-case/camel-case and
OpenRouter architecture input/output modalities. Moonshot's boolean
`supports_image_in` and `supports_video_in` fields also declare input modalities:
two false values explicitly mean text-only. An explicit modality list takes
precedence over these flags; non-boolean flags do not declare support.
Anthropic uses its documented
Models API, including `capabilities.image_input.supported`.

Missing/unknown fields on an official endpoint are filled from its exact registry
model ID. Relays and aggregators also inherit published input/output modalities
for exact IDs and documented aliases, including publisher-qualified IDs such as
`anthropic/claude-opus-5-5`. This does not identify a relay as the official API or
copy the publisher's context, output, protocol or reasoning limits onto it.
Arbitrary deployment names and family-name matches remain unknown. Explicit
advertised, manual or observed restrictions always win.

Documented API redirects apply only to that API's exact registered roots. For
example, DeepSeek's official `deepseek-v4-flash` route now serves the vision-capable
`deepseek-flash` model. OpenRouter retains the original text-only V4 Flash weights.
Provider `model_aliases` resolves the official redirect before enrichment while
preserving the requested upstream ID. Published model aliases remain independent
from these endpoint redirects.

Saved HTTP models and cached discovery previews receive the same modality
backfill when read after an upgrade. Saved limits and reasoning settings remain
unchanged. Loading a cached preview does not refetch upstream, rewrite its file
or change its discovery timestamp. The existing Claude CLI reasoning-default
merge remains specific to that execution backend.

The registry never causes EMP to call a generation endpoint, spend model quota,
or send user content. Updating the bundled registry is a maintainer/release
operation, not an automatic startup network request.

## Maintenance

Every registry entry records `reviewed_at` and primary source URLs. A normal EMP
release reviews the bundled registry and updates confirmed changes. Removed or
uncertain facts become unknown instead of being carried forward by assumption.
OpenRouter's full catalog is deliberately excluded because it is already a live
aggregated data source.

The Rust service can read a local registry override from
`EMP_OFFICIAL_MODELS_PATH`. It reads that file on each model discovery, so an
atomic file replacement takes effect without restarting EMP. If the file is
missing, malformed, or larger than 4 MiB, the release-bundled registry remains
in use. Normal installations need no override.

## Acceptance

- Unrecognized deployment aliases remain conservative and receive no family-name
  inference.
- Live advertised values beat bundled values.
- Bundled deployment limits fill missing/unknown fields only for exact official
  routes and model IDs; published modalities also fill exact IDs on relays.
- Manual and observed values survive discovery refreshes.
- Text, image, audio, video, file/PDF and output modalities remain distinct.
- Provider protocols and model capabilities remain separate.
- Every bundled non-null capability has a primary official source.
- Package builds include the registry without local configuration or secrets.

On 2026-10-30, the bundled data adds seven exact GPT IDs from the stable Codex
0.166 catalog and nine legacy Claude IDs from primary model documentation and
API release notes. Older Claude Haiku 3.5 image support follows the later API
release note, rather than its initially text-only launch announcement. The app
contract exercises discovery, cached selection, published modalities and process
restart. Separate contracts retain explicit text-only restrictions and saved
limits, and upgrade old model-list caches without a network request.

The subsequent 2026-10-30 public-source audit adds **72** more canonical records,
taking that 51-record candidate to **123** records: **101 image-capable** and
**22 text-only**, with no undeclared input modalities. The six previously unknown
records are resolved: Grok 4.5, Kimi K3 and K2.7 Code support images; GLM 5.1,
4.7 and 4.6 are text-only. The inventory covers these publishers:

| Publisher | Canonical records |
|---|---:|
| OpenAI | 36 |
| Anthropic | 21 |
| Google Gemini / Gemma | 22 |
| xAI | 8 |
| Moonshot / Kimi | 6 |
| DeepSeek | 3 |
| GLM | 22 |
| Meta | 2 |
| Inception | 3 |

Current principal chat, coding and vision families are checked against primary
directories: [OpenAI](https://developers.openai.com/api/docs/models),
[Claude](https://platform.claude.com/docs/en/models/overview),
[Gemini](https://ai.google.dev/gemini-api/docs/models),
[Gemma](https://ai.google.dev/gemma/docs/core),
[xAI](https://docs.x.ai/developers/pricing),
[Kimi](https://platform.kimi.ai/docs/models),
[DeepSeek](https://api-docs.deepseek.com/quick_start/pricing/),
[GLM](https://docs.z.ai/guides/overview/overview),
[Meta](https://github.com/meta-llama/llama-models), and
[Inception](https://docs.inceptionlabs.ai/get-started/models).
The additions include Gemini 4 Argon / 3.8 Flash, all five documented Gemma 4
instruction-model sizes, Grok 4.7, Kimi K3.1 Pro / Highspeed, GLM 5.3 / Flash /
FlashX, and DeepSeek Flash. They also recognize older GPT/Codex and GLM IDs that
can still appear in imported services. Recognizing a retired ID describes its
documented capabilities; it does not make a retired API callable.

OpenRouter's public snapshot contained **456** IDs, all with input-modality
metadata. Discovery consumes these live fields directly. Published names such as
`google/gemini-4-argon`, `moonshotai/kimi-k3.1-pro`, `z-ai/glm-5.3-flash` and
`anthropic/claude-opus-5.5` also resolve when a relay omits capabilities. Custom
deployment aliases still use the service declaration or the user's override.
Image generation, speech, embedding, editing and decision endpoints retain their
existing protocol ownership; this audit adds capability data for chat/code
imports. Every added record carries primary field sources.

The application contract follows 12 representative entries through discovery,
cached selection, restart, public configuration and Codex catalog publication.
It retains video/audio/file metadata in EMP while projecting only text/image to
Codex. A registry-wide contract exercises every canonical ID and published alias;
separate checks cover endpoint redirects, explicit restrictions and saved limits.
All requests use synthetic loopback services, with no real model generation.

On 2026-10-21, Mercury 3 passed isolated browser/backend acceptance: the actual
service chooser saved its preset and masked key, model discovery filled the
official limits and capabilities, and two Responses-to-Chat-Completions rounds
preserved instant/high efforts, streamed text, parallel tool calls and tool
results. The existing executable read the candidate registry through a local
override, with only the Inception API root redirected to a synthetic upstream.
Chinese narrow and English desktop layouts passed. This exercised no real
Inception account or model quota.
