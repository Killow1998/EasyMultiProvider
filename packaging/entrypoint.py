"""PyInstaller entry point kept outside the runtime package."""

import json
import ssl
import sys

from easy_multi_provider.main import main


def tls_check():
    """Offline packaging probe: no service, credentials, or config writes."""
    context = ssl.create_default_context()
    print(json.dumps({
        "openssl": ssl.OPENSSL_VERSION,
        "verify_required": context.verify_mode == ssl.CERT_REQUIRED,
        "check_hostname": context.check_hostname,
        "certificates": context.cert_store_stats()["x509_ca"],
    }))


if __name__ == "__main__":
    if sys.argv[1:] == ["--emp-package-tls-check"]:
        tls_check()
    else:
        raise SystemExit(main())
