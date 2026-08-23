"""Content-free storage primitives for native compaction continuity.

Implements Spec S3 route identity and S4.2 source-binding storage plus the
S10.1 pure continuity-service boundary.  No conversation content, opaque
bytes, or credentials are retained.
"""

from __future__ import annotations

import hashlib
import math
import re
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping

from . import capabilities
from .vault import VaultError, read_encrypted_json, write_encrypted_json

_BINDING_FORMAT_VERSION = "emp1"
_MAX_STORED_FIELD_CHARS = 512
_FINGERPRINT_PATTERN = re.compile(r"sha256:[0-9a-f]{64}\Z")
_STORE_FIELDS = frozenset({"format", "bindings"})
_ENTRY_FIELDS = frozenset(
    {
        "fingerprint",
        "auth_identity",
        "endpoint_fingerprint",
        "source_model_slug",
        "endpoint_deployment_fingerprint",
        "deployment_fingerprint",
        "format_version",
        "created_at",
        "updated_at",
    }
)
_ACCOUNT_HEADER = "chatgpt-account-id"
_AUTH_IDENTITY_DOMAIN = b"emp-native-trust-domain\x00"
_DEPLOYMENT_DOMAIN = b"emp-native-deployment-fingerprint\x00"


class BindingError(ValueError):
    """Content-free binding failure."""


class BindingMissing(BindingError):
    """No valid binding exists for the supplied fingerprint."""


class BindingStale(BindingError):
    """A binding exists but no longer matches the requested trust domain."""


class ContinuityError(ValueError):
    """Content-free continuity-service failure."""


def _bounded_text(value: object, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise BindingError("%s must be a non-empty string" % field)
    if len(value) > _MAX_STORED_FIELD_CHARS:
        raise BindingError("%s is too long" % field)
    return value


def _fingerprint(value: object) -> str:
    if not isinstance(value, str) or not _FINGERPRINT_PATTERN.fullmatch(value):
        raise BindingError("fingerprint format is invalid")
    return value


def _timestamp(value: object, field: str, *, optional: bool = False):
    if value is None and optional:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise BindingError("%s must be a finite timestamp" % field)
    try:
        result = float(value)
    except (OverflowError, ValueError):
        raise BindingError("%s must be a finite timestamp" % field) from None
    if not math.isfinite(result) or result < 0:
        raise BindingError("%s must be a non-negative finite timestamp" % field)
    return result


def _validate_timestamp_order(created_at, updated_at) -> None:
    if (
        created_at is not None
        and updated_at is not None
        and updated_at < created_at
    ):
        raise BindingError("updated_at must not precede created_at")


def _sha256(*parts: bytes) -> str:
    digest = hashlib.sha256()
    for part in parts:
        digest.update(part)
        digest.update(b"\x00")
    return "sha256:" + digest.hexdigest()


def _read_account_id(headers: Mapping[str, str]) -> str:
    """Return a validated chatgpt-account-id from case-insensitive headers.

    Only the chatgpt-account-id transient header is consulted.  Missing,
    empty, or whitespace-only values fail closed.  Authorization headers,
    bearer tokens, local account ids, and provider labels are never used as
    substitutes.
    """
    if not isinstance(headers, Mapping):
        raise ContinuityError("account identity header is unavailable")
    value = None
    for key, candidate in headers.items():
        if not isinstance(key, str) or not isinstance(candidate, str):
            continue
        if key.lower() == _ACCOUNT_HEADER:
            value = candidate.strip()
            break
    if not isinstance(value, str) or not value:
        raise ContinuityError("account identity header is unavailable")
    if len(value) > _MAX_STORED_FIELD_CHARS:
        raise ContinuityError("account identity header is too long")
    return value


@dataclass(frozen=True)
class NativeTrustDomain:
    auth_identity: str
    endpoint_fingerprint: str

    def __post_init__(self):
        _bounded_text(self.auth_identity, "auth identity")
        _bounded_text(self.endpoint_fingerprint, "endpoint fingerprint")


@dataclass(frozen=True)
class CompactionBinding:
    fingerprint: str
    trust_domain: NativeTrustDomain
    source_model_slug: str
    endpoint_fingerprint: str
    deployment_fingerprint: str
    format_version: str = _BINDING_FORMAT_VERSION
    created_at: float = None
    updated_at: float = None

    def __post_init__(self):
        _fingerprint(self.fingerprint)
        if not isinstance(self.trust_domain, NativeTrustDomain):
            raise BindingError("trust domain must be a NativeTrustDomain")
        _bounded_text(self.source_model_slug, "source model slug")
        _bounded_text(self.endpoint_fingerprint, "endpoint fingerprint")
        _bounded_text(self.deployment_fingerprint, "deployment fingerprint")
        if self.format_version != _BINDING_FORMAT_VERSION:
            raise BindingError("binding format version is unsupported")
        created_at = _timestamp(self.created_at, "created_at", optional=True)
        updated_at = _timestamp(self.updated_at, "updated_at", optional=True)
        _validate_timestamp_order(created_at, updated_at)
        object.__setattr__(self, "created_at", created_at)
        object.__setattr__(self, "updated_at", updated_at)


@dataclass(frozen=True)
class NativeRouteIdentity:
    """Resolved native route identity for binding construction.

    Every field is either the exact routable catalog slug (operational
    metadata, not a display name) or a domain-separated one-way fingerprint.
    No account id, endpoint URL, token, provider label, or local account
    label is stored on this object.
    """

    trust_domain: NativeTrustDomain
    source_model_slug: str
    endpoint_fingerprint: str
    deployment_fingerprint: str

    def __post_init__(self):
        if not isinstance(self.trust_domain, NativeTrustDomain):
            raise ContinuityError("trust domain must be a NativeTrustDomain")
        _bounded_text(self.source_model_slug, "source model slug")
        fingerprints = (
            self.trust_domain.auth_identity,
            self.trust_domain.endpoint_fingerprint,
            self.endpoint_fingerprint,
            self.deployment_fingerprint,
        )
        if any(
            not isinstance(value, str)
            or not _FINGERPRINT_PATTERN.fullmatch(value)
            for value in fingerprints
        ):
            raise ContinuityError("native route identity fingerprint is invalid")
        if self.trust_domain.endpoint_fingerprint != self.endpoint_fingerprint:
            raise ContinuityError("native route endpoint identity is inconsistent")


def fingerprint_opaque_content(value: object) -> str:
    """Return a domain-separated SHA-256 fingerprint of opaque content."""

    if not isinstance(value, str) or not value:
        raise BindingError("opaque content must be a non-empty string")
    try:
        encoded = value.encode("utf-8", errors="strict")
    except UnicodeEncodeError:
        raise BindingError("opaque content must be valid UTF-8 text") from None
    digest = hashlib.sha256()
    digest.update(b"emp-native-compaction\x00")
    digest.update(encoded)
    return "sha256:" + digest.hexdigest()


def derive_native_route_identity(
    headers: Mapping[str, str],
    endpoint: Any,
    deployment_identity: Any,
    upstream_model: Any,
    source_model_slug: Any,
) -> NativeRouteIdentity:
    """Derive a native route identity from transient validated headers.

    The auth identity is a domain-separated SHA-256 over the stable
    chatgpt-account-id plus the canonical endpoint fingerprint.  The
    deployment fingerprint is a domain-separated SHA-256 over the endpoint
    fingerprint, the deployment identity, and the upstream model.  Returned
    values never contain the account id, endpoint URL, token, provider
    label, or local account label.
    """
    account_id = _read_account_id(headers)
    if not isinstance(source_model_slug, str) or not source_model_slug.strip():
        raise ContinuityError("source model slug must be a non-empty string")
    source_slug = source_model_slug.strip()
    if len(source_slug) > _MAX_STORED_FIELD_CHARS:
        raise ContinuityError("source model slug is too long")
    deployment_value = deployment_identity
    if not isinstance(deployment_value, str) or not deployment_value.strip():
        deployment_value = "default"
    else:
        deployment_value = deployment_value.strip()
        if len(deployment_value) > _MAX_STORED_FIELD_CHARS:
            raise ContinuityError("deployment identity is too long")
    if not isinstance(upstream_model, str) or not upstream_model.strip():
        raise ContinuityError("upstream model is unavailable")
    upstream = upstream_model.strip()
    if len(upstream) > _MAX_STORED_FIELD_CHARS:
        raise ContinuityError("upstream model is too long")
    endpoint_fp = capabilities.endpoint_fingerprint(endpoint)
    auth_identity = _sha256(
        _AUTH_IDENTITY_DOMAIN,
        account_id.encode("utf-8"),
        endpoint_fp.encode("ascii"),
    )
    deployment_fp = _sha256(
        _DEPLOYMENT_DOMAIN,
        endpoint_fp.encode("ascii"),
        deployment_value.encode("utf-8"),
        upstream.encode("utf-8"),
    )
    return NativeRouteIdentity(
        trust_domain=NativeTrustDomain(
            auth_identity=auth_identity,
            endpoint_fingerprint=endpoint_fp,
        ),
        source_model_slug=source_slug,
        endpoint_fingerprint=endpoint_fp,
        deployment_fingerprint=deployment_fp,
    )


def binding_for_opaque(identity: NativeRouteIdentity, opaque: str) -> CompactionBinding:
    """Construct a CompactionBinding for an opaque value without retaining it."""
    if not isinstance(identity, NativeRouteIdentity):
        raise ContinuityError("identity must be a NativeRouteIdentity")
    fingerprint = fingerprint_opaque_content(opaque)
    return CompactionBinding(
        fingerprint=fingerprint,
        trust_domain=identity.trust_domain,
        source_model_slug=identity.source_model_slug,
        endpoint_fingerprint=identity.endpoint_fingerprint,
        deployment_fingerprint=identity.deployment_fingerprint,
    )


def _is_native_compaction_item(item: Any) -> bool:
    return (
        isinstance(item, Mapping)
        and item.get("type") == "compaction"
        and isinstance(item.get("encrypted_content"), str)
        and item.get("encrypted_content").strip()
    )


def _iter_compaction_opaques(output: Any):
    if not isinstance(output, list):
        return ()
    return (
        item["encrypted_content"]
        for item in output
        if _is_native_compaction_item(item)
    )


def register_native_json(
    store: "CompactionBindingStore",
    identity: NativeRouteIdentity,
    response: Any,
    success: bool,
) -> int:
    """Register native compaction items from a successful native JSON result.

    Only output items whose type is "compaction" with a non-empty
    encrypted_content string are registered.  Ordinary message, reasoning,
    tool, image, or checkpoint content is never copied or stored.  Callers
    pass success explicitly; non-success or malformed responses register
    nothing and do not raise content-bearing errors.
    """
    if not isinstance(store, CompactionBindingStore):
        raise ContinuityError("store must be a CompactionBindingStore")
    if not isinstance(identity, NativeRouteIdentity):
        raise ContinuityError("identity must be a NativeRouteIdentity")
    if not success:
        return 0
    if not isinstance(response, Mapping):
        return 0
    output = response.get("output")
    registered = 0
    for opaque in _iter_compaction_opaques(output):
        binding = binding_for_opaque(identity, opaque)
        try:
            store.register(binding)
        except BindingError as exc:
            raise ContinuityError("native compaction binding failed") from exc
        registered += 1
    return registered


class NativeCompactionObserver:
    """Request-scoped observer for native Responses compaction items.

    Observes only response.output_item.done compaction items and the
    response.completed/response.incomplete/response.failed terminal events.
    Opaque values are held only in memory until the successful terminal
    state is known; on response.completed they are registered, and on any
    other terminal or abandoned stream they are discarded.  The same opaque
    value is never registered twice.  Ordinary message, reasoning, tool,
    image, or checkpoint content is never retained.
    """

    __slots__ = ("_store", "_identity", "_lock", "_pending", "_terminal")

    def __init__(self, store: "CompactionBindingStore", identity: NativeRouteIdentity):
        if not isinstance(store, CompactionBindingStore):
            raise ContinuityError("store must be a CompactionBindingStore")
        if not isinstance(identity, NativeRouteIdentity):
            raise ContinuityError("identity must be a NativeRouteIdentity")
        self._store = store
        self._identity = identity
        self._lock = threading.Lock()
        self._pending = {}
        self._terminal = False

    def _ingest_opaque(self, opaque: str) -> None:
        if opaque in self._pending:
            return
        self._pending[opaque] = None

    def _harvest_completed(self, response: Any) -> None:
        if not isinstance(response, Mapping):
            return
        for opaque in _iter_compaction_opaques(response.get("output")):
            self._ingest_opaque(opaque)

    def _flush_and_register(self) -> int:
        opaques = list(self._pending.keys())
        self._pending.clear()
        count = 0
        for opaque in opaques:
            binding = binding_for_opaque(self._identity, opaque)
            try:
                self._store.register(binding)
            except BindingError as exc:
                raise ContinuityError("native compaction binding failed") from exc
            count += 1
        return count

    def _discard(self) -> None:
        self._pending.clear()

    def observe(self, event: Any) -> int:
        """Observe a single Responses event.

        Returns the number of newly registered bindings (non-zero only on
        response.completed).
        """
        with self._lock:
            if self._terminal:
                return 0
            if not isinstance(event, Mapping):
                self._terminal = True
                self._discard()
                return 0
            event_type = event.get("type")
            if not isinstance(event_type, str) or not event_type:
                self._terminal = True
                self._discard()
                return 0
            if event_type == "response.output_item.done":
                item = event.get("item")
                if _is_native_compaction_item(item):
                    self._ingest_opaque(item["encrypted_content"])
                return 0
            if event_type == "response.completed":
                self._terminal = True
                response = event.get("response")
                if not isinstance(response, Mapping) or not isinstance(
                    response.get("output"), list
                ):
                    self._discard()
                    return 0
                self._harvest_completed(response)
                return self._flush_and_register()
            if event_type in ("response.incomplete", "response.failed"):
                self._terminal = True
                self._discard()
                return 0
            if event_type == "error":
                self._terminal = True
                self._discard()
                return 0
            return 0

    def abandon(self) -> None:
        """Discard any pending opaque values without registering."""
        with self._lock:
            self._terminal = True
            self._discard()


def _validated_entry(value: object) -> dict:
    if not isinstance(value, dict) or set(value) != _ENTRY_FIELDS:
        raise BindingError("binding entry format is invalid")
    if value.get("format_version") != _BINDING_FORMAT_VERSION:
        raise BindingError("binding entry format version is unsupported")
    entry = {
        "fingerprint": _fingerprint(value.get("fingerprint")),
        "auth_identity": _bounded_text(
            value.get("auth_identity"), "auth identity"
        ),
        "endpoint_fingerprint": _bounded_text(
            value.get("endpoint_fingerprint"), "endpoint fingerprint"
        ),
        "source_model_slug": _bounded_text(
            value.get("source_model_slug"), "source model slug"
        ),
        "endpoint_deployment_fingerprint": _bounded_text(
            value.get("endpoint_deployment_fingerprint"),
            "endpoint deployment fingerprint",
        ),
        "deployment_fingerprint": _bounded_text(
            value.get("deployment_fingerprint"), "deployment fingerprint"
        ),
        "format_version": _BINDING_FORMAT_VERSION,
        "created_at": _timestamp(value.get("created_at"), "created_at"),
        "updated_at": _timestamp(value.get("updated_at"), "updated_at"),
    }
    _validate_timestamp_order(entry["created_at"], entry["updated_at"])
    return entry


class CompactionBindingStore:
    def __init__(self, path: Path, capacity: int = 256):
        self._path = Path(path)
        if isinstance(capacity, bool) or not isinstance(capacity, int) or capacity < 1:
            raise BindingError("capacity must be a positive integer")
        self._capacity = capacity
        self._lock = threading.RLock()

    def _load(self) -> list:
        if not self._path.exists():
            return []
        try:
            raw = read_encrypted_json(self._path)
        except (VaultError, OSError):
            raise BindingError("binding store is unavailable or corrupt") from None
        if (
            not isinstance(raw, dict)
            or set(raw) != _STORE_FIELDS
            or raw.get("format") != _BINDING_FORMAT_VERSION
            or not isinstance(raw.get("bindings"), list)
        ):
            raise BindingError("binding store format is invalid")
        bindings = []
        fingerprints = set()
        for value in raw["bindings"]:
            entry = _validated_entry(value)
            if entry["fingerprint"] in fingerprints:
                raise BindingError("binding store contains duplicate entries")
            fingerprints.add(entry["fingerprint"])
            bindings.append(entry)
        return bindings

    def _save(self, bindings: list) -> None:
        payload = {"format": _BINDING_FORMAT_VERSION, "bindings": bindings}
        try:
            write_encrypted_json(self._path, payload)
        except (VaultError, OSError) as exc:
            raise BindingError("binding store cannot be written") from exc

    def register(self, binding: CompactionBinding) -> None:
        if not isinstance(binding, CompactionBinding):
            raise BindingError("value must be a CompactionBinding")
        with self._lock:
            bindings = self._load()
            existing_index = next(
                (
                    index
                    for index, item in enumerate(bindings)
                    if item["fingerprint"] == binding.fingerprint
                ),
                None,
            )
            existing = (
                bindings[existing_index] if existing_index is not None else None
            )
            if existing is not None:
                stored_source = (
                    existing["auth_identity"],
                    existing["endpoint_fingerprint"],
                    existing["source_model_slug"],
                    existing["endpoint_deployment_fingerprint"],
                    existing["deployment_fingerprint"],
                    existing["format_version"],
                )
                requested_source = (
                    binding.trust_domain.auth_identity,
                    binding.trust_domain.endpoint_fingerprint,
                    binding.source_model_slug,
                    binding.endpoint_fingerprint,
                    binding.deployment_fingerprint,
                    binding.format_version,
                )
                if requested_source != stored_source:
                    raise BindingStale(
                        "binding source conflicts with the existing binding"
                    )
            now = _timestamp(time.time(), "updated_at")
            created_at = (
                existing["created_at"]
                if existing is not None
                else binding.created_at
                if binding.created_at is not None
                else now
            )
            updated_at = (
                binding.updated_at if binding.updated_at is not None else now
            )
            _validate_timestamp_order(created_at, updated_at)
            if existing is not None:
                entry = dict(existing)
                entry["updated_at"] = updated_at
            else:
                entry = _validated_entry(
                    {
                        "fingerprint": binding.fingerprint,
                        "auth_identity": binding.trust_domain.auth_identity,
                        "endpoint_fingerprint": (
                            binding.trust_domain.endpoint_fingerprint
                        ),
                        "source_model_slug": binding.source_model_slug,
                        "endpoint_deployment_fingerprint": (
                            binding.endpoint_fingerprint
                        ),
                        "deployment_fingerprint": binding.deployment_fingerprint,
                        "format_version": binding.format_version,
                        "created_at": created_at,
                        "updated_at": updated_at,
                    }
                )
            kept = list(bindings)
            if existing_index is None:
                kept.append(entry)
            else:
                kept[existing_index] = entry
            while len(kept) > self._capacity:
                oldest = min(
                    range(len(kept)), key=lambda index: kept[index]["created_at"]
                )
                kept.pop(oldest)
            self._save(kept)

    def lookup(self, opaque_content: str) -> CompactionBinding:
        fp = fingerprint_opaque_content(opaque_content)
        with self._lock:
            for item in self._load():
                if item["fingerprint"] != fp:
                    continue
                return CompactionBinding(
                    fingerprint=fp,
                    trust_domain=NativeTrustDomain(
                        item["auth_identity"], item["endpoint_fingerprint"]
                    ),
                    source_model_slug=item["source_model_slug"],
                    endpoint_fingerprint=item[
                        "endpoint_deployment_fingerprint"
                    ],
                    deployment_fingerprint=item["deployment_fingerprint"],
                    format_version=item["format_version"],
                    created_at=item["created_at"],
                    updated_at=item["updated_at"],
                )
            raise BindingMissing("no binding exists for the requested content")

    def resolve(
        self,
        opaque_content: str,
        trust_domain: NativeTrustDomain,
        endpoint_fingerprint: str,
        deployment_fingerprint: str,
    ) -> CompactionBinding:
        td = trust_domain if isinstance(trust_domain, NativeTrustDomain) else None
        if td is None:
            raise BindingError("trust domain must be a NativeTrustDomain")
        ep = _bounded_text(endpoint_fingerprint, "endpoint fingerprint")
        dp = _bounded_text(deployment_fingerprint, "deployment fingerprint")
        with self._lock:
            binding = self.lookup(opaque_content)
            if (
                binding.trust_domain != td
                or binding.endpoint_fingerprint != ep
                or binding.deployment_fingerprint != dp
            ):
                raise BindingStale(
                    "binding does not match the requested trust domain"
                )
            return binding
