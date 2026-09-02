import os
import socket
import ssl
import unittest
from urllib.error import HTTPError, URLError
from unittest.mock import patch

from easy_multi_provider.server import _pre_output_http_failure
from easy_multi_provider.transport_failures import (
    DNS_FAILURE,
    PROXY_UNAVAILABLE,
    TLS_FAILURE,
    network_failure,
    upstream_http_failure,
)


class TransportFailureClassificationTests(unittest.TestCase):
    def test_refused_configured_proxy_is_service_unavailable(self):
        with patch.dict(
            os.environ,
            {"HTTPS_PROXY": "http://127.0.0.1:9"},
            clear=True,
        ):
            failure = network_failure(URLError(ConnectionRefusedError()))

        self.assertEqual(failure.status, 503)
        self.assertEqual(failure.error_class, PROXY_UNAVAILABLE)
        self.assertEqual(failure.failure_reason, PROXY_UNAVAILABLE)

    def test_dns_and_tls_failures_have_distinct_classes(self):
        with patch.dict(os.environ, {}, clear=True):
            dns = network_failure(URLError(socket.gaierror(-2, "name failed")))
            tls = network_failure(URLError(ssl.SSLError("handshake failed")))

        self.assertEqual((dns.status, dns.error_class), (503, DNS_FAILURE))
        self.assertEqual((tls.status, tls.error_class), (502, TLS_FAILURE))
        self.assertEqual(dns.failure_reason, DNS_FAILURE)
        self.assertEqual(tls.failure_reason, TLS_FAILURE)

    def test_proxy_generated_http_error_is_not_reported_as_upstream_5xx(self):
        error = HTTPError(
            "https://example.invalid/v1/responses",
            502,
            "Bad Gateway",
            {"Proxy-Agent": "local-gateway"},
            None,
        )

        failure = upstream_http_failure(error, b"gateway unavailable", "model")

        self.assertEqual(failure.status, 503)
        self.assertEqual(failure.error_class, PROXY_UNAVAILABLE)

    def test_origin_http_502_remains_upstream_502(self):
        error = HTTPError(
            "https://example.invalid/v1/responses",
            502,
            "Bad Gateway",
            {"Content-Type": "application/json"},
            None,
        )

        failure = upstream_http_failure(
            error,
            b'{"error":{"message":"temporarily unavailable"}}',
            "model",
        )

        self.assertEqual(failure.status, 502)
        self.assertEqual(failure.error_class, "upstream_5xx")

    def test_pre_output_error_uses_safe_specific_message(self):
        failure = _pre_output_http_failure(
            {
                "type": "response.failed",
                "response": {
                    "error": {
                        "status": 503,
                        "error_class": PROXY_UNAVAILABLE,
                        "failure_reason": PROXY_UNAVAILABLE,
                    }
                },
            }
        )

        self.assertIsNotNone(failure)
        status, body = failure
        self.assertEqual(status, 503)
        self.assertEqual(
            body["error"]["message"],
            "Configured proxy is unavailable.",
        )
        self.assertIsNone(body["error"]["param"])


if __name__ == "__main__":
    unittest.main()
