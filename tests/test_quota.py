import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider.quota import parse_app_server_output, read_account_quota
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class QuotaTests(unittest.TestCase):
    def test_parser_keeps_quota_and_masks_account_identity(self):
        output = "\n".join(
            [
                json.dumps({
                    "id": 1,
                    "result": {
                        "account": {"type": "chatgpt", "planType": "plus", "email": "user@example.com"}
                    },
                }),
                json.dumps({
                    "id": 2,
                    "result": {
                        "rateLimits": {
                            "primary": {"usedPercent": 20, "windowDurationMins": 300, "resetsAt": 123},
                            "secondary": None,
                        }
                    },
                }),
            ]
        )
        value = parse_app_server_output(output)
        self.assertEqual(value["plan_type"], "plus")
        self.assertEqual(value["rate_limits"]["primary"]["usedPercent"], 20)
        self.assertEqual(value["account_label"], "u***@example.com")
        self.assertNotIn("user@example.com", json.dumps(value))

    def test_parser_exposes_codex_credits_without_reset_ids(self):
        output = json.dumps({
            "id": 3,
            "result": {
                "rateLimits": {
                    "primary": {"usedPercent": 20},
                    "credits": {"hasCredits": True, "unlimited": False, "balance": "42"},
                    "individualLimit": {
                        "limit": "100",
                        "used": "58",
                        "remainingPercent": 42,
                        "resetsAt": 123,
                    },
                    "spendControlReached": False,
                },
                "rateLimitResetCredits": {
                    "availableCount": 2,
                    "credits": [{
                        "id": "opaque-reset-id",
                        "status": "available",
                        "expiresAt": 456,
                        "title": "Full reset",
                    }],
                },
            },
        })

        value = parse_app_server_output(output)

        self.assertEqual(value["credits"]["balance"], "42")
        self.assertEqual(value["credits"]["individual_limit"]["remaining_percent"], 42)
        self.assertEqual(value["credits"]["reset_credits"]["available_count"], 2)
        self.assertNotIn("opaque-reset-id", json.dumps(value))

    def test_quota_process_is_pinned_to_account_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            account_dir = Path(directory) / "ship"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            write_encrypted_json(auth_file, {"tokens": {"access_token": "secret"}})

            class FakeStream:
                def __init__(self, lines=()):
                    self.lines = iter(lines)

                def readline(self):
                    return next(self.lines, "")

            class FakeStdin:
                def __init__(self):
                    self.body = ""
                    self.closed = False

                def write(self, value):
                    self.body += value

                def flush(self):
                    return None

                def close(self):
                    self.closed = True

            class FakeProcess:
                def __init__(self):
                    self.returncode = 0
                    self.stdin = FakeStdin()
                    self.stdout = FakeStream(
                        [
                            json.dumps({"id": 1, "result": {}}) + "\n",
                            json.dumps({"id": 2, "result": {"account": {"planType": "plus"}}}) + "\n",
                            json.dumps({"id": 3, "result": {"rateLimits": {"primary": {"usedPercent": 0}}}}) + "\n",
                        ]
                    )
                    self.stderr = FakeStream()

                def wait(self, timeout=None):
                    return self.returncode

                def kill(self):
                    self.returncode = -9

            process = FakeProcess()
            with patch("easy_multi_provider.quota.subprocess.Popen", return_value=process) as started:
                value = read_account_quota({"auth_file": str(auth_file)})
            self.assertEqual(value["plan_type"], "plus")
            kwargs = started.call_args.kwargs
            self.assertNotEqual(kwargs["env"]["CODEX_HOME"], str(account_dir))
            self.assertNotIn("secret", process.stdin.body)
            self.assertEqual(json.loads(process.stdin.body.splitlines()[1])["method"], "initialized")
            self.assertTrue(process.stdin.closed)


if __name__ == "__main__":
    unittest.main()
