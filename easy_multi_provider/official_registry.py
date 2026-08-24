"""Official model capability registry for EMP.

Keeps a small, release-bundled registry of official Provider contracts and
notable model capabilities.  The registry fills gaps left by incomplete model
list endpoints; it does not replace live discovery and never creates a route
for a model the upstream did not advertise or the user did not add.

See docs/official-model-registry-spec.md for the full design.
"""

from __future__ import annotations

import copy
import json
from datetime import datetime
from pathlib import Path
from urllib.parse import urlsplit

from .capabilities import make_provenance

_DEFAULT_DATA_PATH = Path(__file__).resolve().parent / "data" / "official_models.json"

# Registry fields copied to the discovered model under the same top-level name.
_TOP_LEVEL_FIELDS = (
    "context_window",
    "max_input_tokens",
    "input_modalities",
    "output_modalities",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "web_search",
)

# Registry fields projected as (target field, nested under capabilities).
_PROJECTED_FIELDS = {
    "max_output_tokens": ("output_limit", False),
    "protocols": ("supported_protocols", False),
    "tool_calling": ("structured_tools", True),
    "parallel_tool_calling": ("parallel_tools", True),
    "streaming": ("streaming", True),
    "structured_output": ("structured_output", True),
}

_CAPABILITY_SOURCE_FIELDS = _TOP_LEVEL_FIELDS + tuple(_PROJECTED_FIELDS)


class RegistryError(ValueError):
    """Raised when the registry is missing, invalid, or cannot satisfy a request."""


def _is_missing(value):
    """Return True when value represents an explicit unknown/missing field."""
    if value is None:
        return True
    if isinstance(value, (list, str)) and len(value) == 0:
        return True
    return False


def _normalize_url(url):
    """Strictly normalize a root URL for exact comparison.

    Only http/https URLs without username, password, query, or fragment are
    accepted.  Scheme and hostname are lowercased, IDNA hosts are converted to
    ASCII, default ports are removed, and only a trailing slash difference is
    allowed.  Invalid URLs normalize to an empty string and never match.
    """
    if not isinstance(url, str) or not url.strip():
        return ""
    try:
        parsed = urlsplit(url.strip())
        scheme = parsed.scheme.lower()
        hostname = parsed.hostname
        if scheme not in ("http", "https") or not hostname:
            return ""
        if parsed.username is not None or parsed.password is not None:
            return ""
        if parsed.query or parsed.fragment:
            return ""
        port = parsed.port
        hostname = hostname.encode("idna").decode("ascii").lower()
    except (UnicodeError, ValueError):
        return ""
    default_port = 80 if scheme == "http" else 443
    port_part = "" if port in (None, default_port) else ":%d" % port
    host = hostname
    if ":" in host and not host.startswith("["):
        host = "[%s]" % host
    path = parsed.path or "/"
    if len(path) > 1:
        path = path.rstrip("/") or "/"
    return "%s://%s%s%s" % (scheme, host, port_part, path)


def _all_api_base_urls(provider_record):
    """Return normalized, valid entries from the provider api_base_urls list."""
    urls = provider_record.get("api_base_urls")
    if not isinstance(urls, list):
        return set()
    return {
        normalized
        for normalized in (_normalize_url(url) for url in urls)
        if normalized
    }


def _validate_capability_sources(model):
    """Require a non-empty HTTPS source for every non-null capability field.

    Also validates every entry in ``sources_by_field``, including the ``all``
    fallback key, so a non-HTTPS or malformed URL anywhere is rejected.
    """
    sources = model.get("sources_by_field")
    if not isinstance(sources, dict):
        sources = {}

    def has_valid_urls(value):
        if not isinstance(value, list) or not value:
            return False
        for url in value:
            if not isinstance(url, str):
                return False
            try:
                parsed = urlsplit(url.strip())
            except ValueError:
                return False
            if parsed.scheme != "https" or not parsed.netloc:
                return False
        return True

    for field, urls in sources.items():
        if not has_valid_urls(urls):
            raise RegistryError(
                "sources_by_field entry must be a non-empty list of HTTPS URLs: "
                + str(field)
            )

    for field in _CAPABILITY_SOURCE_FIELDS:
        if model.get(field) is None:
            continue
        if not (
            has_valid_urls(sources.get(field))
            or has_valid_urls(sources.get("all"))
        ):
            raise RegistryError(
                "capability field requires sources_by_field entry with "
                "non-empty HTTPS URLs: " + str(field)
            )


def load_registry(path=None):
    """Load and validate the official registry.

    Parameters
    ----------
    path:
        Optional explicit path to a registry JSON file.  When None, the
        release-bundled data file is used.

    Returns
    -------
    dict
        The parsed and validated registry.

    Raises
    ------
    RegistryError
        If the file is missing, not valid JSON, lacks required top-level
        keys, contains invalid provider/model entries, duplicate provider
        keys, unknown model providers, duplicate model IDs or aliases, or
        capability fields without HTTPS sources.
    """
    if path is None:
        file_path = _DEFAULT_DATA_PATH
    else:
        file_path = Path(path)

    if not file_path.is_file():
        raise RegistryError("registry file not found: " + str(file_path))

    try:
        with open(file_path, "r", encoding="utf-8") as fh:
            registry = json.load(fh)
    except (json.JSONDecodeError, OSError) as exc:
        raise RegistryError("registry file is not valid JSON: " + str(exc)) from exc

    if not isinstance(registry, dict):
        raise RegistryError("registry root must be a JSON object")

    for required in ("schema_version", "providers", "models"):
        if required not in registry:
            raise RegistryError("registry missing required key: " + required)

    _SUPPORTED_SCHEMA_VERSION = 1
    schema_version = registry.get("schema_version")
    if not isinstance(schema_version, int) or isinstance(schema_version, bool) or schema_version <= 0:
        raise RegistryError("schema_version must be a positive integer")
    if schema_version != _SUPPORTED_SCHEMA_VERSION:
        raise RegistryError(
            "unsupported schema_version %d; supported version is %d"
            % (schema_version, _SUPPORTED_SCHEMA_VERSION)
        )
    reviewed_at = registry.get("reviewed_at")
    if not isinstance(reviewed_at, str) or not reviewed_at:
        raise RegistryError("reviewed_at must be a non-empty ISO date string")
    try:
        datetime.fromisoformat(reviewed_at)
    except ValueError:
        raise RegistryError("reviewed_at must be a valid ISO date string")

    providers = registry.get("providers")
    models = registry.get("models")
    if not isinstance(providers, list) or not isinstance(models, list):
        raise RegistryError("registry providers and models must be lists")

    provider_keys = set()
    for provider in providers:
        if not isinstance(provider, dict):
            raise RegistryError("each provider entry must be an object")
        key = provider.get("key")
        if not key:
            raise RegistryError("provider entry with null or missing key")
        if key in provider_keys:
            raise RegistryError("duplicate provider key: " + str(key))
        provider_keys.add(key)

    openrouter_key = "openrouter"
    openrouter_registered = any(
        p.get("key") == openrouter_key for p in providers
    )

    model_keys = set()
    alias_keys = set()
    for model in models:
        if not isinstance(model, dict):
            raise RegistryError("each model entry must be an object")
        provider_key = model.get("provider_key")
        if provider_key not in provider_keys:
            raise RegistryError(
                "model provider_key does not match a registry provider: "
                + str(provider_key)
            )
        if provider_key == openrouter_key and openrouter_registered:
            raise RegistryError(
                "OpenRouter is a live aggregator; static model records are not allowed"
            )
        model_id = model.get("model_id")
        if not model_id:
            raise RegistryError("model entry with null or missing model_id")
        model_key = (provider_key, model_id)
        if model_key in model_keys or model_key in alias_keys:
            raise RegistryError(
                "duplicate model id or alias: " + provider_key + "/" + str(model_id)
            )
        model_keys.add(model_key)
        for alias in model.get("aliases") or []:
            if not alias:
                raise RegistryError("model alias must be a non-empty value")
            alias_key = (provider_key, alias)
            if alias_key in model_keys or alias_key in alias_keys:
                raise RegistryError(
                    "duplicate model id or alias: "
                    + provider_key
                    + "/"
                    + str(alias)
                )
            alias_keys.add(alias_key)
        _validate_capability_sources(model)

    return registry


def identify_provider(provider, registry=None):
    """Identify the official provider key for a user-configured provider.

    Matching is by exact normalized API root from ``api_base_urls``, not by
    model name.  An explicit ``official_provider`` is accepted only when the
    key exists and the configured ``base_url`` belongs to that provider's
    registered roots; otherwise no official identity is returned.
    """
    if not isinstance(provider, dict):
        return None

    if registry is None:
        registry = load_registry()

    explicit = provider.get("official_provider")
    if isinstance(explicit, str) and explicit:
        for provider_record in registry.get("providers", []):
            if provider_record.get("key") != explicit:
                continue
            base_url = _normalize_url(provider.get("base_url"))
            if base_url and base_url in _all_api_base_urls(provider_record):
                return explicit
        return None

    base_url = _normalize_url(provider.get("base_url"))
    if not base_url:
        return None

    for provider_record in registry.get("providers", []):
        if base_url in _all_api_base_urls(provider_record):
            return provider_record.get("key")

    return None


def _build_model_lookup(registry):
    """Build a {(provider_key, model_id): model_record} lookup."""
    lookup = {}
    for model in registry.get("models", []):
        key = (model.get("provider_key"), model.get("model_id"))
        if key[0] and key[1]:
            lookup[key] = model
        for alias in model.get("aliases") or []:
            akey = (model.get("provider_key"), alias)
            if akey[0] and akey[1]:
                lookup.setdefault(akey, model)
    return lookup

_UNKNOWN_OR_WEAK_SOURCES = frozenset({"unknown", "inferred", "official"})


def _can_override(current_value, sources, field):
    if _is_missing(current_value):
        return True
    provenance = sources.get(field) if isinstance(sources, dict) else None
    if isinstance(provenance, dict):
        source = provenance.get("source")
    elif isinstance(provenance, str):
        source = provenance
    else:
        source = None
    if source is None:
        return False
    return source in _UNKNOWN_OR_WEAK_SOURCES


def enrich_discovered_models(provider, models, registry=None):
    if registry is None:
        registry = load_registry()

    provider_key = identify_provider(provider, registry)

    if not provider_key:
        return copy.deepcopy(models)

    official_provenance = make_provenance(
        "official", observed_at=registry.get("reviewed_at")
    )
    model_lookup = _build_model_lookup(registry)

    result = []
    for model in models:
        enriched = copy.deepcopy(model)

        model_id = (
            enriched.get("upstream_id")
            or enriched.get("model_id")
            or enriched.get("id")
        )
        if not model_id:
            result.append(enriched)
            continue

        official = model_lookup.get((provider_key, model_id))
        if not official:
            result.append(enriched)
            continue

        sources = enriched.get("capability_sources")
        sources = dict(sources) if isinstance(sources, dict) else {}

        for field in _TOP_LEVEL_FIELDS:
            official_value = official.get(field)
            if _is_missing(official_value):
                continue
            if _can_override(enriched.get(field), sources, field):
                enriched[field] = copy.deepcopy(official_value)
                sources[field] = dict(official_provenance)

        for registry_field, (target, nested) in _PROJECTED_FIELDS.items():
            official_value = official.get(registry_field)
            if _is_missing(official_value):
                continue
            if nested:
                capabilities = enriched.get("capabilities")
                if not isinstance(capabilities, dict):
                    capabilities = {}
                current = capabilities.get(target)
            else:
                current = enriched.get(target)
            if _can_override(current, sources, target):
                if nested:
                    capabilities[target] = copy.deepcopy(official_value)
                    enriched["capabilities"] = capabilities
                else:
                    enriched[target] = copy.deepcopy(official_value)
                sources[target] = dict(official_provenance)

        if sources:
            enriched["capability_sources"] = sources

        result.append(enriched)

    return result
