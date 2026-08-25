import json
import unittest
from unittest.mock import patch

from easy_multi_provider.destination_summary import DestinationSummaryAdapter
from easy_multi_provider.history_compaction import SummaryRequest


class DestinationSummaryTests(unittest.TestCase):
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
