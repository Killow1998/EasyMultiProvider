"""Select the operating system trust store before network clients are imported."""

from __future__ import annotations

import importlib
import threading


_LOCK = threading.Lock()
_CONFIGURED = False
_TRUST_SOURCE = "openssl"


def configure_system_trust() -> str:
    """Use the native trust store when the optional backend is available.

    Packaged EMP builds run on Python 3.11 and include ``truststore``. Source
    installations on older Python versions keep the verified OpenSSL default.
    This function must run before urllib/WebSocket clients are imported.
    """

    global _CONFIGURED, _TRUST_SOURCE
    with _LOCK:
        if _CONFIGURED:
            return _TRUST_SOURCE
        try:
            truststore = importlib.import_module("truststore")
            truststore.inject_into_ssl()
        except Exception:
            _TRUST_SOURCE = "openssl"
        else:
            _TRUST_SOURCE = "system"
        _CONFIGURED = True
        return _TRUST_SOURCE


def tls_trust_source() -> str:
    """Return a content-free diagnostic name for the active trust backend."""

    return _TRUST_SOURCE
