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

The initial registry covers current notable models from OpenAI, Google Gemini,
Anthropic, xAI, Meta, Moonshot AI, DeepSeek, and Zhipu AI. OpenRouter contributes
its official Provider contract, while its live `/models` response remains the
source for its changing model catalog.

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

Official endpoints are identified by exact registered API roots, not by model
name. Live `/models` fields are normalized first, then missing/unknown fields
are filled from an exact registry model ID. OpenRouter live metadata includes
architecture input/output modalities and supported parameters. Anthropic uses
its documented Models API. Providers without a rich model endpoint receive
only exact registry fallback facts.

The registry never causes EMP to call a generation endpoint, spend model quota,
or send user content. Updating the bundled registry is a maintainer/release
operation, not an automatic startup network request.

## Maintenance

Every registry entry records `reviewed_at` and primary source URLs. A normal EMP
release reviews the bundled registry and updates confirmed changes. Removed or
uncertain facts become unknown instead of being carried forward by assumption.
OpenRouter's full catalog is deliberately excluded because it is already a live
aggregated data source.

## Acceptance

- Unknown/custom deployments remain conservative and receive no publisher-name
  inference.
- Live advertised values beat bundled values.
- Bundled values fill only missing/unknown fields for exact official routes and
  exact model IDs.
- Manual and observed values survive discovery refreshes.
- Text, image, audio, video, file/PDF and output modalities remain distinct.
- Provider protocols and model capabilities remain separate.
- Every bundled non-null capability has a primary official source.
- Package builds include the registry without local configuration or secrets.
