import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider import quota as quota_module
from easy_multi_provider.quota import (
    _query_app_server,
    _trusted_codex_binary,
    account_refresh_lock,
    parse_app_server_output,
    read_account_quota,
    read_native_login_quota,
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

    def test_windows_root_export_is_bounded_to_server_auth_certificates(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory, patch.object(
            quota_module.ssl,
            "enum_certificates",
            return_value=[
                (b"server", "x509_asn", {quota_module._SERVER_AUTH_OID}),
                (b"duplicate", "x509_asn", True),
                (b"duplicate", "x509_asn", True),
                (b"client-only", "x509_asn", {"1.3.6.1.5.5.7.3.2"}),
            ],
            create=True,
        ), patch.object(
            quota_module.ssl,
            "DER_cert_to_PEM_cert",
            side_effect=lambda value: "CERT-" + value.decode("ascii"),
        ):
            bundle = quota_module._write_windows_root_ca_bundle(Path(directory))

            self.assertIsNotNone(bundle)
            self.assertEqual(
                bundle.read_text(encoding="ascii"),
                "CERT-server\nCERT-duplicate\n",
            )

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

    def test_parser_accepts_codex_multi_bucket_and_rolling_update_shapes(self):
        output = "\n".join(
            [
                json.dumps(
                    {
                        "id": 3,
                        "result": {
                            "rateLimitsByLimitId": {
                                "codex": {
                                    "primary": {"usedPercent": 20},
                                    "planType": "plus",
                                }
                            }
                        },
                    }
                ),
                json.dumps(
                    {
                        "method": "account/rateLimits/updated",
                        "params": {
                            "rateLimits": {
                                "primary": {"usedPercent": 21},
                                "planType": "plus",
                            }
                        },
                    }
                ),
            ]
        )

        value = parse_app_server_output(output)

        self.assertEqual(value["plan_type"], "plus")
        self.assertEqual(value["rate_limits"]["primary"]["usedPercent"], 21)

    def test_quota_process_is_pinned_to_account_directory(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
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
            with patch.dict(
                os.environ,
                {
                    "HTTPS_PROXY": "http://proxy.invalid",
                    "CODEX_CA_CERTIFICATE": str(Path(directory) / "codex-ca.pem"),
                    "SYSTEMROOT": "C:\\Windows",
                    "OPENAI_API_KEY": "unrelated-private-key",
                },
                clear=False,
            ), patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ) as started:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary=str(codex_file)
                )
            self.assertEqual(value["plan_type"], "plus")
            kwargs = started.call_args.kwargs
            self.assertNotEqual(kwargs["env"]["CODEX_HOME"], str(account_dir))
            self.assertNotIn("EASY_MULTI_PROVIDER_MASTER_KEY", kwargs["env"])
            self.assertNotIn("OPENAI_API_KEY", kwargs["env"])
            self.assertEqual(kwargs["env"]["SYSTEMROOT"], "C:\\Windows")
            self.assertEqual(kwargs["env"]["HTTPS_PROXY"], "http://proxy.invalid")
            self.assertEqual(
                kwargs["env"]["CODEX_CA_CERTIFICATE"],
                str(Path(directory) / "codex-ca.pem"),
            )
            self.assertNotIn("secret", process.stdin.body)
            self.assertEqual(json.loads(process.stdin.body.splitlines()[1])["method"], "initialized")
            rate_limit_request = json.loads(process.stdin.body.splitlines()[3])
            self.assertEqual(rate_limit_request["method"], "account/rateLimits/read")
            self.assertIsNone(rate_limit_request["params"])
            self.assertTrue(process.stdin.closed)


class NativeLoginQuotaTests(unittest.TestCase):
    def _fake_process_factory(self, refresh_token_value):
        sent = []

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
                sent.append(value)

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

        return FakeProcess(), sent

    def test_quota_rpc_errors_are_classified_without_exposing_response_bodies(self):
        requests = [
            {"id": 1, "method": "initialize"},
            {"method": "initialized"},
            {"id": 2, "method": "account/read"},
            {"id": 3, "method": "account/rateLimits/read"},
        ]
        for error_text, code in (
            ("codex account authentication required to read rate limits", "quota_auth_required"),
            ("failed to fetch codex rate limits: GET https://example.invalid failed: 401 Unauthorized; content-type=text/plain; body=private-token", "quota_auth_required"),
            ("failed to fetch codex rate limits: GET https://example.invalid failed: 403 Forbidden; content-type=text/plain; body=private-token", "quota_access_denied"),
            ("failed to fetch codex rate limits: GET https://example.invalid failed: 429 Too Many Requests; content-type=text/plain; body=private-token", "quota_rate_limited"),
            ("failed to fetch codex rate limits: private-token", "quota_fetch_failed"),
        ):
            with self.subTest(code=code):
                process, _ = self._fake_process_factory(True)
                process.stdout = io.StringIO("\n".join(json.dumps(value) for value in (
                    {"id": 1, "result": {}},
                    {"id": 2, "result": {"account": {"type": "chatgpt"}}},
                    {"id": 3, "error": {"code": -32603, "message": error_text}},
                )) + "\n")
                with self.assertRaises(quota_module.QuotaError) as raised:
                    _query_app_server(process, requests, 2)
                self.assertEqual(raised.exception.code, code)
                self.assertNotIn("private-token", str(raised.exception))
                self.assertNotIn("example.invalid", str(raised.exception))
                self.assertTrue(process.stdin.closed)

    def test_missing_account_stops_before_quota_request(self):
        process, _ = self._fake_process_factory(True)
        process.stdout = io.StringIO(
            '{"id":1,"result":{}}\n'
            '{"id":2,"result":{"account":null,"requiresOpenaiAuth":true}}\n'
        )
        with self.assertRaises(quota_module.QuotaError) as raised:
            _query_app_server(process, [
                {"id": 1, "method": "initialize"}, {"method": "initialized"},
                {"id": 2, "method": "account/read"},
                {"id": 3, "method": "account/rateLimits/read"},
            ], 2)
        self.assertEqual(raised.exception.code, "quota_auth_required")
        self.assertNotIn("account/rateLimits/read", process.stdin.body)

    def test_rotated_imported_credentials_survive_quota_query_failure(self):
        original = {"tokens": {"access_token": "old-access", "refresh_token": "old-refresh"}}
        rotated = {"tokens": {"access_token": "new-access", "refresh_token": "new-refresh"}}
        with tempfile.TemporaryDirectory() as directory:
            auth_file = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_file, original)
            process, _ = self._fake_process_factory(True)

            def start(*_args, **kwargs):
                # Codex rotates and saves auth, then quota retrieval fails.
                (Path(kwargs["cwd"]) / "auth.json").write_text(json.dumps(rotated), encoding="utf-8")
                return process

            with patch.object(quota_module, "_trusted_codex_binary", return_value=(Path('/codex'), (1, 2))), patch.object(
                quota_module, "_verify_codex_binary"
            ), patch.object(quota_module.subprocess, "Popen", side_effect=start), patch.object(
                quota_module, "_query_app_server", side_effect=quota_module.QuotaError("quota fetch failed")
            ):
                with self.assertRaisesRegex(quota_module.QuotaError, "quota fetch failed"):
                    read_account_quota({"auth_file": str(auth_file)})
            self.assertEqual(quota_module.load_auth({"auth_file": str(auth_file)}), rotated)

    def test_invalid_refreshed_credentials_do_not_replace_saved_login(self):
        original = {"tokens": {"access_token": "old-access", "refresh_token": "old-refresh"}}
        with tempfile.TemporaryDirectory() as directory:
            auth_file = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_file, original)
            process, _ = self._fake_process_factory(True)

            def start(*_args, **kwargs):
                (Path(kwargs["cwd"]) / "auth.json").write_text('{}', encoding="utf-8")
                return process

            with patch.object(quota_module, "_trusted_codex_binary", return_value=(Path('/codex'), (1, 2))), patch.object(
                quota_module, "_verify_codex_binary"
            ), patch.object(quota_module.subprocess, "Popen", side_effect=start):
                with self.assertRaises(quota_module.QuotaError) as raised:
                    read_account_quota({"auth_file": str(auth_file)})
            self.assertEqual(raised.exception.code, "quota_credentials_save_failed")
            self.assertEqual(quota_module.load_auth({"auth_file": str(auth_file)}), original)

    def test_native_login_quota_uses_refresh_token_false(self):
        process, _sent = self._fake_process_factory(False)
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            codex_home = Path(directory) / "codex-home"
            codex_home.mkdir()
            native_auth = codex_home / "auth.json"
            native_auth.write_text(
                json.dumps({"tokens": {"access_token": "native-current-secret"}}),
                encoding="utf-8",
            )
            codex_file = Path(directory) / "codex"
            codex_file.write_text("#!/bin/sh\n", encoding="utf-8")
            codex_file.chmod(0o700)
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}, clear=False), patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ) as started:
                value = read_native_login_quota(codex_binary=str(codex_file))
            self.assertEqual(value["plan_type"], "plus")
            body_lines = process.stdin.body.splitlines()
            account_read = json.loads(body_lines[2])
            self.assertEqual(account_read["method"], "account/read")
            self.assertIs(account_read["params"]["refreshToken"], False)
            # The temporary auth file must not be persisted to an EMP vault path
            self.assertNotEqual(started.call_args.kwargs["env"]["CODEX_HOME"], str(codex_home))
            self.assertNotIn("native-current-secret", json.dumps(value))

    def test_native_login_quota_does_not_mutate_native_auth(self):
        process, _sent = self._fake_process_factory(False)
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            codex_home = Path(directory) / "codex-home"
            codex_home.mkdir()
            native_auth = codex_home / "auth.json"
            native_payload = {"tokens": {"access_token": "native-current-secret"}}
            native_auth.write_text(json.dumps(native_payload), encoding="utf-8")
            before = native_auth.read_text(encoding="utf-8")
            codex_file = Path(directory) / "codex"
            codex_file.write_text("#!/bin/sh\n", encoding="utf-8")
            codex_file.chmod(0o700)
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}, clear=False), patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ), patch(
                "easy_multi_provider.quota.write_encrypted_json"
            ) as persisted:
                read_native_login_quota(codex_binary=str(codex_file))
            self.assertEqual(native_auth.read_text(encoding="utf-8"), before)
            persisted.assert_not_called()

    def test_native_login_quota_does_not_persist_temporary_state(self):
        # Ensure the temporary codex home auth.json is NOT copied back to native auth.
        process, _sent = self._fake_process_factory(False)
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            codex_home = Path(directory) / "codex-home"
            codex_home.mkdir()
            native_auth = codex_home / "auth.json"
            native_auth.write_text(
                json.dumps({"tokens": {"access_token": "native-current-secret"}}),
                encoding="utf-8",
            )
            codex_file = Path(directory) / "codex"
            codex_file.write_text("#!/bin/sh\n", encoding="utf-8")
            codex_file.chmod(0o700)
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}, clear=False), patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ), patch(
                "easy_multi_provider.quota.write_encrypted_json"
            ) as persisted:
                read_native_login_quota(codex_binary=str(codex_file))
            persisted.assert_not_called()

    def test_imported_quota_still_uses_refresh_token_true_and_persists(self):
        # Non-duplicate imported account must keep refreshToken=True and persist
        # a validated refreshed credential back to its EMP encrypted auth file.
        process, _sent = self._fake_process_factory(True)
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            write_encrypted_json(auth_file, {"tokens": {"access_token": "imported-secret"}})
            codex_file = Path(directory) / "codex"
            codex_file.write_text("#!/bin/sh\n", encoding="utf-8")
            codex_file.chmod(0o700)
            with patch(
                "easy_multi_provider.quota.subprocess.Popen", return_value=process
            ) as started, patch(
                "easy_multi_provider.quota.write_encrypted_json",
                wraps=write_encrypted_json,
            ) as persisted:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary=str(codex_file)
                )
            self.assertEqual(value["plan_type"], "plus")
            body_lines = process.stdin.body.splitlines()
            account_read = json.loads(body_lines[2])
            self.assertEqual(account_read["method"], "account/read")
            self.assertIs(account_read["params"]["refreshToken"], True)
            # The imported path must persist a refreshed credential back to the
            # target EMP auth_file, and the written value must be a validated
            # dict (not the raw account, not a path, not None).
            persisted.assert_called_once()
            call_args, call_kwargs = persisted.call_args
            persisted_path = Path(call_args[0]) if call_args else Path(call_kwargs["path"])
            self.assertEqual(persisted_path.resolve(), auth_file.resolve())
            written_value = call_args[1] if len(call_args) > 1 else call_kwargs["value"]
            self.assertIsInstance(written_value, dict)
            self.assertIn("tokens", written_value)
            self.assertIn("access_token", written_value["tokens"])


if __name__ == "__main__":
    unittest.main()
