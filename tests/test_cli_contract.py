"""Deterministic unit coverage for the overnight CLI supervisor's oracle."""

import json
import tempfile
import unittest
from pathlib import Path

from tools.overnight_cli import (
    CASE_MANIFEST,
    LIVE_WRITE_EXPECTED_BYTES,
    LIVE_WRITE_PROMPT,
    MOCK_SENTINEL,
    SCHEMA,
    _response_stream,
    atomic_json,
    fault_stream_oracle,
    json_error_message,
    parse_jsonl,
    redact,
)


class CliContractTests(unittest.TestCase):
    def test_jsonl_parser_requires_one_thread_and_completed_turn(self):
        output = "\n".join(
            [
                json.dumps({"type": "thread.started", "thread_id": "thread-test"}),
                json.dumps({"type": "turn.completed"}),
            ]
        )
        parsed = parse_jsonl(output)
        self.assertEqual(parsed["thread_started"], 1)
        self.assertEqual(parsed["turn_completed"], 1)
        self.assertEqual(parsed["failures"], 0)
        self.assertEqual(parsed["terminal_count"], 1)
        self.assertEqual(parsed["terminal_types"], ["turn.completed"])
        self.assertTrue(parsed["events"])
        self.assertEqual(parsed["thread_id"], "thread-test")
        failed = parse_jsonl(
            "\n".join(
                [
                    json.dumps({"type": "thread.started", "thread_id": "thread-test"}),
                    json.dumps({"type": "error", "message": "upstream failed"}),
                ]
            )
        )
        self.assertEqual(failed["turn_completed"], 0)
        self.assertEqual(failed["failures"], 1)
        self.assertEqual(failed["terminal_count"], 1)
        for scenario in ("empty", "half", "invalid"):
            malformed = parse_jsonl(
                "\n".join(
                    [
                        json.dumps({"type": "thread.started", "thread_id": "thread-test"}),
                        json.dumps({"type": "error", "message": "malformed %s" % scenario}),
                    ]
                )
            )
            oracle = fault_stream_oracle(
                scenario,
                {"parsed": malformed, "checks": {"returncode_zero": False, "timed_out": False}, "stderr": "upstream malformed"},
            )
            self.assertTrue(oracle["explicit_failure"])
            self.assertTrue(oracle["no_success_terminal"])
            self.assertTrue(oracle["not_silent"])
            self.assertTrue(oracle["semantic_pass"])
        non_sse = parse_jsonl(
            "\n".join(
                [
                    json.dumps({"type": "thread.started", "thread_id": "thread-test"}),
                    json.dumps({"type": "turn.completed"}),
                ]
            )
        )
        self.assertTrue(
            fault_stream_oracle(
                "non-sse-json",
                {
                    "parsed": non_sse,
                    "checks": {
                        "returncode_zero": True,
                        "timed_out": False,
                        "schema_json": True,
                        "sentinel_matches": True,
                        "nonce_matches": True,
                    },
                    "stderr": "",
                },
            )["conversion_to_legal_completion"]
        )
        self.assertFalse(
            fault_stream_oracle(
                "invalid",
                {
                    "parsed": parse_jsonl(
                        "\n".join(
                            [
                                json.dumps({"type": "thread.started", "thread_id": "thread-test"}),
                                json.dumps({"type": "error", "message": "invalid"}),
                                json.dumps({"type": "response.completed"}),
                            ]
                        )
                    ),
                    "checks": {"returncode_zero": False, "timed_out": False},
                    "stderr": "upstream invalid",
                },
            )["semantic_pass"]
        )

    def test_redaction_removes_bearer_and_token_values(self):
        value = redact('Authorization: Bearer real-secret access_token=real-access bootstrap=abc123 sk-abcdefghijklmnop')
        self.assertNotIn("real-secret", value)
        self.assertNotIn("real-access", value)
        self.assertNotIn("abc123", value)
        self.assertNotIn("sk-abcdefghijklmnop", value)
        self.assertIn("<redacted>", value)
        self.assertEqual(json_error_message(b'{"error":{"message":"injected upstream 404"}}'), "injected upstream 404")

    def test_fixed_response_stream_is_utf8_sse_with_completion(self):
        stream = _response_stream("gpt-5.6-luna", json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "中文🚀"}, ensure_ascii=False))
        text = stream.decode("utf-8")
        self.assertIn("response.created", text)
        self.assertIn("response.completed", text)
        self.assertIn("中文🚀", text)

    def test_manifest_is_scoped_to_cli_track_and_schema_is_closed(self):
        manifest_text = json.dumps(CASE_MANIFEST, ensure_ascii=False).lower()
        self.assertNotIn("gemini", manifest_text)
        required_ids = {item["id"] for item in CASE_MANIFEST if item["required"]}
        self.assertTrue({"LIVE-04", "LIVE-06"}.issubset(required_ids))
        self.assertEqual(SCHEMA["additionalProperties"], False)
        self.assertEqual(set(SCHEMA["required"]), set(SCHEMA["properties"]))
        self.assertEqual(LIVE_WRITE_EXPECTED_BYTES, b"LIVE_WRITE_MARKER\n")
        self.assertIn("18 bytes total", LIVE_WRITE_PROMPT)
        self.assertIn("Do not remove the LF", LIVE_WRITE_PROMPT)

    def test_atomic_json_round_trip(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nested" / "value.json"
            atomic_json(path, {"nonce": "fixed", "unicode": "中文🚀"})
            self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["unicode"], "中文🚀")


if __name__ == "__main__":
    unittest.main()
