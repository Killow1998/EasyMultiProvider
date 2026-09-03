"""PyInstaller entry point kept outside the runtime package."""

import json
import ssl
import sys

from easy_multi_provider.tls_runtime import configure_system_trust


def tls_check():
    """Offline packaging probe: no service, credentials, or config writes."""
    trust_source = configure_system_trust()
    context = ssl.create_default_context()
    try:
        certificates = context.cert_store_stats()["x509_ca"]
    except (AttributeError, NotImplementedError):
        certificates = None
    print(json.dumps({
        "openssl": ssl.OPENSSL_VERSION,
        "verify_required": context.verify_mode == ssl.CERT_REQUIRED,
        "check_hostname": context.check_hostname,
        "certificates": certificates,
        "trust_source": trust_source,
    }))


if __name__ == "__main__":
    if sys.argv[1:] == ["--emp-package-tls-check"]:
        tls_check()
    else:
        configure_system_trust()
        from easy_multi_provider.main import main

        raise SystemExit(main())
