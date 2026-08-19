import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider import quota as quota_module
from easy_multi_provider.quota import (
    _trusted_codex_binary,
    account_refresh_lock,
    parse_app_server_output,
    read_account_quota,
)
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class QuotaTests(unittest.TestCase):
    @unittest.skipIf(os.name == "nt", "Unix path permission checks do not apply")
    def test_trusts_owner_managed_group_writable_codex_path(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            root = Path(directory)
            binary = root / "codex"
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(0o770)
            root.chmod(0o770)

            trusted, identity = _trusted_codex_binary(str(binary))

            self.assertEqual(trusted, binary.resolve())
            self.assertEqual(identity, (binary.stat().st_dev, binary.stat().st_ino))

    def test_account_refresh_lock_does_not_retain_identifier_keys(self):
        self.assertIs(account_refresh_lock("one"), account_refresh_lock("two"))

    def test_refreshes_different_accounts_without_cross_account_cooldown(self):
        with patch.object(quota_module, "_last_refresh", 0.0), patch.object(
            quota_module, "_last_refresh_key", b"", create=True
        ), patch.object(
            quota_module, "read_account_quota", side_effect=[{"id": "one"}, {"id": "two"}]
        ), patch.object(
            quota_module.time, "monotonic", side_effect=[100.0, 100.0, 100.0, 100.0]
        ):
            self.assertEqual(
                quota_module.refresh_account_quota({"id": "one"}), {"id": "one"}
            )
            self.assertEqual(
                quota_module.refresh_account_quota({"id": "two"}), {"id": "two"}
            )

    def test_repeated_same_account_still_respects_cooldown(self):
        with patch.object(quota_module, "_last_refresh", 0.0), patch.object(
            quota_module, "_last_refresh_key", b"", create=True
        ), patch.object(
            quota_module, "read_account_quota", return_value={"id": "one"}
        ), patch.object(quota_module.time, "monotonic", side_effect=[100.0, 100.0, 100.0]):
            quota_module.refresh_account_quota({"id": "one"})
            with self.assertRaisesRegex(quota_module.QuotaError, "cooling down"):
                quota_module.refresh_account_quota({"id": "one"})

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
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "ship"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            write_encrypted_json(auth_file, {"tokens": {"access_token": "secret"}})
            codex_file = Path(directory) / "codex"
            codex_file.write_text("#!/bin/sh\n", encoding="utf-8")
            codex_file.chmod(0o700)

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
            with patch.dict(os.environ, {"HTTPS_PROXY": "http://proxy.invalid"}, clear=False), patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ) as started:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary=str(codex_file)
                )
            self.assertEqual(value["plan_type"], "plus")
            kwargs = started.call_args.kwargs
            self.assertNotEqual(kwargs["env"]["CODEX_HOME"], str(account_dir))
            self.assertNotIn("EASY_MULTI_PROVIDER_MASTER_KEY", kwargs["env"])
            self.assertEqual(kwargs["env"]["HTTPS_PROXY"], "http://proxy.invalid")
            self.assertNotIn("secret", process.stdin.body)
            self.assertEqual(json.loads(process.stdin.body.splitlines()[1])["method"], "initialized")
            self.assertTrue(process.stdin.closed)


if __name__ == "__main__":
    unittest.main()
