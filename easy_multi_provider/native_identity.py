"""Transient, content-free identity for native WebSocket route reuse."""

from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from typing import Any, Mapping

from .capabilities import endpoint_fingerprint


_MAX_FIELD_CHARS = 512
_FINGERPRINT = re.compile(r"sha256:[0-9a-f]{64}\Z")
_AUTH_DOMAIN = b"emp-native-route-auth\x00"
_DEPLOYMENT_DOMAIN = b"emp-native-route-deployment\x00"


class NativeIdentityError(ValueError):
    """A bounded failure to derive native connection identity."""


@dataclass(frozen=True)
class NativeRouteIdentity:
    """The minimum transient identity needed to reuse a native socket."""

    auth_identity: str
    endpoint_fingerprint: str
    deployment_fingerprint: str

    def __post_init__(self) -> None:
        for value in (
            self.auth_identity,
            self.endpoint_fingerprint,
            self.deployment_fingerprint,
        ):
            if not isinstance(value, str) or not _FINGERPRINT.fullmatch(value):
                raise NativeIdentityError("native route identity is invalid")

    @property
    def connection_key(self) -> str:
        return ":".join(
            (self.auth_identity, self.endpoint_fingerprint, self.deployment_fingerprint)
        )


def _bounded_text(value: Any, field: str, *, default: str = "") -> str:
    if not isinstance(value, str) or not value.strip():
        if default:
            return default
        raise NativeIdentityError("native route %s is unavailable" % field)
    value = value.strip()
    if len(value) > _MAX_FIELD_CHARS:
        raise NativeIdentityError("native route %s is too long" % field)
    return value


def _account_id(headers: Mapping[str, Any]) -> str:
    if not isinstance(headers, Mapping):
        raise NativeIdentityError("native route account identity is unavailable")
    for key, value in headers.items():
        if isinstance(key, str) and key.lower() == "chatgpt-account-id":
            return _bounded_text(value, "account identity")
    raise NativeIdentityError("native route account identity is unavailable")


def _digest(domain: bytes, *parts: str) -> str:
    digest = hashlib.sha256()
    digest.update(domain)
    for part in parts:
        digest.update(part.encode("utf-8"))
        digest.update(b"\x00")
    return "sha256:" + digest.hexdigest()


def derive_native_route_identity(
    headers: Mapping[str, Any],
    endpoint: Any,
    deployment: Any,
    upstream_model: Any,
) -> NativeRouteIdentity:
    """Derive socket identity without retaining account or endpoint content."""

    account = _account_id(headers)
    deployment_value = _bounded_text(
        deployment, "deployment identity", default="default"
    )
    upstream = _bounded_text(upstream_model, "upstream model")
    endpoint_value = endpoint_fingerprint(endpoint)
    return NativeRouteIdentity(
        auth_identity=_digest(_AUTH_DOMAIN, account, endpoint_value),
        endpoint_fingerprint=endpoint_value,
        deployment_fingerprint=_digest(
            _DEPLOYMENT_DOMAIN, endpoint_value, deployment_value, upstream
        ),
    )


__all__ = [
    "NativeIdentityError",
    "NativeRouteIdentity",
    "derive_native_route_identity",
]
