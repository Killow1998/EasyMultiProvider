import io
import unittest
from http.server import BaseHTTPRequestHandler
from types import SimpleNamespace
from unittest.mock import Mock, patch

from easy_multi_provider.quota import QuotaError
from easy_multi_provider.server import make_handler


class ManagementDisconnectTests(unittest.TestCase):
    def test_management_failures_keep_specific_quota_error_codes(self):
        handler = object.__new__(make_handler(SimpleNamespace()))
        for code in ("quota_output_encoding_error", "quota_output_protocol_error", "quota_timeout"):
            self.assertEqual(handler._management_failure_class(QuotaError("safe error", code)), code)

    def test_disconnect_while_returning_quota_error_does_not_write_twice(self):
        for error in (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            with self.subTest(error=error.__name__):
                journal = Mock()
                handler = object.__new__(make_handler(SimpleNamespace(journal=journal)))
                handler._begin_http_request = Mock()
                handler._release_request_budget = Mock()
                handler._record_http_request_once = Mock()
                handler._record_unexpected_exception = Mock()
                handler.send_response = Mock()
                handler.send_header = Mock()
                handler.end_headers = Mock()
                handler.wfile = Mock(spec=io.BytesIO)
                handler.wfile.write.side_effect = error(10053, "client closed")
                handler.close_connection = False

                def reply(_):
                    try:
                        raise QuotaError("quota unavailable")
                    except QuotaError:
                        handler._send(503, b'{"error":"quota unavailable"}')

                with patch.object(BaseHTTPRequestHandler, "handle_one_request", reply):
                    handler.handle_one_request()
                self.assertTrue(handler.close_connection)
                handler.wfile.write.assert_called_once()
                handler._record_unexpected_exception.assert_not_called()
                handler._release_request_budget.assert_called_once()
                handler._record_http_request_once.assert_called_once()
                self.assertEqual(journal.event.call_args.args[:2], ("info", "client_disconnected"))
