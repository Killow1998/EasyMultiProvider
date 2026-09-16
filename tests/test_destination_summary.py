import json
import unittest
from unittest.mock import patch

from easy_multi_provider.destination_summary import DestinationSummaryAdapter
from easy_multi_provider.history_compaction import SummaryRequest, HistoryCompactionError, _extract_summary


class DestinationSummaryTests(unittest.TestCase):
    def test_standard_responses_message_content_is_a_usable_checkpoint(self):
        response = {
            "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "Completed the analysis."},
                {"type": "output_text", "text": "Next: verify the figures."},
            ]}],
        }
        self.assertEqual(_extract_summary(response),
                         "Completed the analysis.\nNext: verify the figures.")
        for content in ([], [{"type": "refusal", "refusal": "Cannot summarize"}],
                        [{"type": "reasoning", "text": "private reasoning"}]):
            response["output"][0]["content"] = content
            with self.subTest(content=content), self.assertRaises(HistoryCompactionError):
                _extract_summary(response)

    def test_history_failure_reason_survives_transport_classification(self):
        from easy_multi_provider.router_errors import HistoryReconstructionError
        from easy_multi_provider.transport_failures import failure_from_exception
        event = failure_from_exception(HistoryReconstructionError("history_compaction_failed")).terminal()
        self.assertEqual(event["failure_reason"], "history_compaction_failed")
        self.assertEqual(event["error_class"], "history_reconstruction_failed")

    def test_summary_call_bypasses_history_tools_streaming_and_retry(self):
        request = SummaryRequest(
            provider={"id": "external", "protocol": "responses"},
            model={"id": "external/model", "upstream_id": "model"},
            protocol="responses",
            body={
                "model": "model",
                "input": [{"type": "message", "role": "user", "content": "history"}],
                "tools": [{"type": "function", "name": "shell"}],
                "stream": True,
                "previous_response_id": "opaque",
            },
            stage="map",
            safe_input_budget=4096,
            output_limit=512,
            source_fingerprint="sha256:test",
        )
        response = json.dumps(
            {
                "id": "resp_summary",
                "object": "response",
                "status": "completed",
                "output": [],
                "output_text": "portable checkpoint",
            }
        ).encode()

        with patch(
            "easy_multi_provider.destination_summary.forward_responses",
            return_value=(200, "application/json", response),
        ) as forwarded:
            result = DestinationSummaryAdapter()(request)

        body = forwarded.call_args.args[1]
        self.assertFalse(body["stream"])
        self.assertEqual(body["tools"], [])
        self.assertNotIn("previous_response_id", body)
        self.assertFalse(forwarded.call_args.kwargs["allow_retries"])
        self.assertEqual(result["output_text"], "portable checkpoint")


if __name__ == "__main__":
    unittest.main()
