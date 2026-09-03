import importlib
import unittest
from unittest.mock import Mock, patch

import easy_multi_provider.tls_runtime as tls_runtime


class TlsRuntimeTests(unittest.TestCase):
    def setUp(self):
        importlib.reload(tls_runtime)

    def tearDown(self):
        importlib.reload(tls_runtime)

    def test_system_trust_is_injected_once(self):
        backend = Mock()
        with patch.object(
            tls_runtime.importlib, "import_module", return_value=backend
        ) as loaded:
            self.assertEqual(tls_runtime.configure_system_trust(), "system")
            self.assertEqual(tls_runtime.configure_system_trust(), "system")

        loaded.assert_called_once_with("truststore")
        backend.inject_into_ssl.assert_called_once_with()
        self.assertEqual(tls_runtime.tls_trust_source(), "system")

    def test_verified_openssl_remains_the_fallback(self):
        with patch.object(
            tls_runtime.importlib,
            "import_module",
            side_effect=ImportError("unsupported"),
        ):
            self.assertEqual(tls_runtime.configure_system_trust(), "openssl")

        self.assertEqual(tls_runtime.tls_trust_source(), "openssl")


if __name__ == "__main__":
    unittest.main()
