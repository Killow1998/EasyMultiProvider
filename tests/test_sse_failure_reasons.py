import unittest
from unittest.mock import patch

from easy_multi_provider.transport import sse_json_events, TransportError
from easy_multi_provider.transport_failures import failure_from_exception, public_failure_message
from easy_multi_provider.router import _project_responses_stream


class SSEFailureReasonTests(unittest.TestCase):
    def test_parser_causes_survive_projection_without_content(self):
        for data, reason in ((b"data: private-broken-json\n\n", "sse_invalid_json"),
                             (b"data: []\n\n", "sse_non_object")):
            with self.subTest(reason=reason):
                with self.assertRaises(Exception) as caught:
                    list(_project_responses_stream({}, [data]))
                failure = failure_from_exception(caught.exception)
                self.assertEqual(failure.failure_reason, reason)
                self.assertNotIn("private", public_failure_message(failure.error_class, reason))

    def test_limit_reports_emp_limit_and_accepts_normal_events(self):
        with patch("easy_multi_provider.transport.MAX_SSE_EVENT_BYTES", 32):
            with self.assertRaises(TransportError) as caught:
                list(sse_json_events([b"data: " + b"x" * 40 + b"\n\n"]))
            self.assertEqual(caught.exception.failure_reason, "sse_event_too_large")
        self.assertEqual(list(sse_json_events([b'data: {"type":"response.created"}\n\n'])),
                         [{"type": "response.created"}])

    def test_many_valid_events_in_one_network_chunk_are_not_one_large_event(self):
        event = b'data: {"type":"ping"}\n\n'
        with patch("easy_multi_provider.transport.MAX_SSE_EVENT_BYTES", 32):
            self.assertEqual(len(list(sse_json_events([event * 20]))), 20)
            self.assertEqual(len(list(sse_json_events([event[:8], event[8:] + event * 20]))), 21)
