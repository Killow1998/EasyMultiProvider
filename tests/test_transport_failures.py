import os
import json
import socket
import ssl
import unittest
from urllib.error import HTTPError, URLError
from unittest.mock import Mock, patch

from easy_multi_provider.server import _pre_output_http_failure, _retry_headers
from easy_multi_provider.router_errors import UpstreamHTTPError
from easy_multi_provider.stream_adapters import (
    StreamAdapterIO,
    _reliable_responses_stream,
    stream_anthropic_completion,
    stream_chat_completion,
)
from easy_multi_provider.transport import sse_json_events
from easy_multi_provider.transport_failures import (
    DNS_FAILURE,
    PROXY_UNAVAILABLE,
    TLS_FAILURE,
    network_failure,
    parse_retry_after,
    upstream_http_failure,
)


class TransportFailureClassificationTests(unittest.TestCase):
    def test_external_missing_finish_reason_survives_http_translation(self):
        event = {"type": "response.failed", "response": {
            "status": "failed", "error": {
                "code": "upstream_incomplete_response",
                "error_class": "stream_incomplete",
                "failure_reason": "upstream_incomplete_response",
                "message": "private upstream detail",
            },
        }}
        wire = ("data: " + json.dumps(event) + "\n\n").encode()
        events = list(sse_json_events(_reliable_responses_stream(lambda: [wire])))
        status, payload = _pre_output_http_failure(events[-1])
        self.assertEqual(status, 502)
        self.assertEqual(payload["error"]["failure_reason"], "upstream_incomplete_response")
        self.assertIn("completion event", payload["error"]["message"])
        self.assertNotIn("private upstream detail", str(payload))

    def test_retry_after_seconds_and_http_date_are_normalized(self):
        with patch("easy_multi_provider.transport_failures.time.time", return_value=0):
            self.assertEqual(parse_retry_after("Thu, 01 Jan 1970 00:00:30 GMT"), 30)
        self.assertEqual(parse_retry_after("2.5"), 3)
        for value in (None, "", "-1", "NaN", "inf", "secret-value", "3\r\nX-Test: value"):
            with self.subTest(value=value):
                self.assertIsNone(parse_retry_after(value))
        error = HTTPError("https://example.invalid", 503, "busy", {"Retry-After": "12"}, None)
        self.assertEqual(upstream_http_failure(error, b"busy", "model").retry_after_seconds, 12)

    def test_stream_rate_limit_keeps_delay_without_local_replay(self):
        request = Mock(side_effect=UpstreamHTTPError("private", 429, "rate_limited", "rate_limit", 12))
        events = list(sse_json_events(_reliable_responses_stream(request, replay_safe=True)))
        self.assertEqual(request.call_count, 1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["response"]["error"]["retry_after_seconds"], 12)
        self.assertIn("Please try again in 12s.", events[0]["response"]["error"]["message"])
        status, payload = _pre_output_http_failure(events[0])
        self.assertEqual(status, 429)
        self.assertEqual(_retry_headers(payload), {"Retry-After": "12"})
        self.assertNotIn("private", str(payload))

    def test_protocol_adapters_preserve_rate_limit_for_the_stream_boundary(self):
        for adapter in (stream_chat_completion, stream_anthropic_completion):
            with self.subTest(adapter=adapter.__name__):
                request = Mock(side_effect=UpstreamHTTPError(
                    "private upstream detail", 429, "rate_limited", "rate_limit", 12
                ))
                io = StreamAdapterIO(request, Mock(), Mock(), lambda _, body, __: body)
                body = {"model": "fixture", "input": "hello", "stream": True}
                reports = []
                events = list(sse_json_events(_reliable_responses_stream(
                    lambda: adapter(io, {}, body, {}, {}, "fixture"),
                    reports.append, replay_safe=True,
                )))
                self.assertEqual(request.call_count, 1)
                self.assertEqual(len(events), 1)
                status, payload = _pre_output_http_failure(events[0])
                self.assertEqual(status, 429)
                self.assertEqual(payload["error"]["failure_reason"], "rate_limited")
                self.assertEqual(_retry_headers(payload), {"Retry-After": "12"})
                self.assertEqual(reports[0]["retry_after_seconds"], 12)
                self.assertNotIn("private", str(payload))

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
