import json
import tempfile
import unittest
import ssl
import time
from urllib.error import URLError
from unittest.mock import patch
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from types import SimpleNamespace

from easy_multi_provider.codex_dispatch import CodexRequestDispatcher
from easy_multi_provider.destination_context import DestinationContextCompactor
from easy_multi_provider.diagnostic_journal import create_journal
from easy_multi_provider.history_continuity import HistoryContinuityEngine
from easy_multi_provider.router_errors import HistoryReconstructionError, UpstreamHTTPError
from tests.test_destination_context import _blocked, _message


class HistoryDiagnosticsTests(unittest.TestCase):
    def run_case(self, journal, summary, request_id):
        provider = {"id": "external", "protocol": "responses", "api_key": "PRIVATE-KEY"}
        model = {"id": "external/model", "upstream_id": "model", "max_output_tokens": 64}
        dispatcher = CodexRequestDispatcher(
            lambda: {}, None, None, HistoryContinuityEngine(None),
            DestinationContextCompactor(summary), lambda *a, **k: None,
            lambda *a, **k: None, lambda *a, **k: None, journal=journal,
        )
        body = {"model": model["id"], "input": [
            _message("PRIVATE-PROMPT " * 40) for _ in range(12)
        ]}
        prepare, compact = dispatcher._history_callbacks(body, {"X-EMP-Request-ID": request_id})
        prepared = prepare({}, provider, model, model["id"], body, {})
        return compact(provider, model, model["id"], prepared, _blocked(provider, model))

    def test_request_correlated_success_and_failures_do_not_record_private_data(self):
        def good(_):
            return {"status": "completed", "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "PRIVATE-SUMMARY"}
            ]}]}

        def timeout(_):
            raise TimeoutError("PRIVATE-EXCEPTION with PRIVATE-KEY")

        def quota(_):
            raise UpstreamHTTPError("PRIVATE-UPSTREAM-BODY", 429, "quota_exhausted", "rate_limit")

        cases = [(good, None), (timeout, "summary_call_failed"),
                 (quota, "summary_call_failed"), (lambda _: {"status": "completed", "output": []}, "summary_text_missing")]
        with tempfile.TemporaryDirectory() as directory:
            journal = create_journal(directory)
            self.assertTrue(journal.enabled)
            for index, (summary, reason) in enumerate(cases):
                if reason:
                    with self.assertRaises(HistoryReconstructionError) as raised:
                        self.run_case(journal, summary, f"{index:016x}")
                    self.assertEqual(raised.exception.reason, reason)
                else:
                    self.run_case(journal, summary, f"{index:016x}")
            journal.close()
            text = "".join(p.read_text(encoding="utf-8") for p in Path(directory).glob("state/logs/*.jsonl"))
            self.assertNotIn("PRIVATE-", text)
            events = [json.loads(line)["fields"] for line in text.splitlines()]
            for index in range(4):
                group = [e for e in events if e["request_id"] == f"{index:016x}"]
                self.assertEqual(group[0]["stage"], "prepare")
                self.assertTrue(any(e["stage"] == "summary_call" for e in group))
                self.assertTrue(any(e["stage"] == "compact_metrics" for e in group))
                self.assertEqual(group[-1]["stage"], "compact")
                self.assertEqual(group[-1]["result"], "completed" if index == 0 else "failed")
            rate_limit = next(e for e in events if e["request_id"] == "0000000000000002"
                              and e["stage"] == "summary_call" and e["result"] == "failed")
            self.assertEqual(rate_limit["status"], 429)
            self.assertEqual(rate_limit["reason"], "quota_exhausted")
            self.assertEqual(rate_limit["exception_chain"][0]["type"], "UpstreamHTTPError")

    def test_concurrent_requests_keep_their_own_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            journal = create_journal(directory)
            with ThreadPoolExecutor(max_workers=2) as pool:
                list(pool.map(lambda i: self.run_case(journal, lambda _: "summary", f"{i:016x}"), [10, 11]))
            journal.close()
            events = [json.loads(line)["fields"] for path in Path(directory).glob("state/logs/*.jsonl")
                      for line in path.read_text(encoding="utf-8").splitlines()]
            for i in (10, 11):
                group = [e for e in events if e["request_id"] == f"{i:016x}"]
                self.assertEqual(group[0]["stage"], "prepare")
                self.assertEqual(group[-1]["stage"], "compact")
                self.assertEqual(group[-1]["result"], "completed")

    def test_logging_failure_does_not_break_model_request(self):
        def fail(*args, **kwargs):
            raise OSError("log disk unavailable")
        self.run_case(SimpleNamespace(event=fail), lambda _: "summary", "a" * 16)

    def test_tls_cause_is_logged_even_after_route_observation_and_in_lazy_stream(self):
        events = []
        journal = SimpleNamespace(event=lambda level, event, **fields: events.append((event, fields)))
        dispatcher = CodexRequestDispatcher(lambda: {}, None,
            SimpleNamespace(prepare=lambda body, scope: body), None, None,
            lambda *a, **k: None, lambda *a, **k: None, lambda *a, **k: None, journal=journal)
        dispatcher._route = lambda snapshot, body: None

        def fail(*args, **kwargs):
            from easy_multi_provider.transport_failures import TransportFailure
            try:
                raise URLError(ssl.SSLError(1, "PRIVATE-TLS-MESSAGE"))
            except URLError as cause:
                raise TransportFailure("tls_failure", 502, "connect", "tls_failure") from cause

        def proxy(*args, **kwargs):
            args[3]({"status": 502, "error_class": "tls_failure"})
            fail()

        with patch('easy_multi_provider.codex_dispatch.proxy', proxy), \
             patch('easy_multi_provider.codex_dispatch.provider_replay_scope', return_value=None):
            with self.assertRaises(Exception):
                dispatcher.route({"model": "external/model"}, {"X-EMP-Request-ID": "b" * 16})
        self.assertEqual(len(events), 1)
        self.assertEqual([e['type'] for e in events[0][1]['exception_chain']],
                         ['TransportFailure', 'URLError', 'SSLError'])
        self.assertEqual(events[0][1]['request_id'], 'b' * 16)
        self.assertNotIn('PRIVATE-', json.dumps(events))

        closed = []
        def stream():
            try:
                yield b'data: started\n\n'
                fail()
            finally:
                closed.append(True)
        with self.assertRaises(Exception):
            list(dispatcher._log_stream_failures(stream(), {'request_id': 'c' * 16}, time.monotonic()))
        self.assertEqual(closed, [True])
        self.assertEqual(events[-1][1]['request_id'], 'c' * 16)


if __name__ == "__main__":
    unittest.main()
