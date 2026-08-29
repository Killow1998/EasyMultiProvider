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
    read_native_login_quota,
)
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class QuotaTests(unittest.TestCase):
    def test_windows_root_ca_bundle_exports_server_auth_roots(self):
        certificates = [
            (b"root-one", "x509_asn", True),
            (b"root-two", "x509_asn", {quota_module._SERVER_AUTH_OID}),
            (b"email-only", "x509_asn", {"1.3.6.1.5.5.7.3.4"}),
            (b"pkcs-seven", "pkcs_7_asn", True),
            (b"root-one", "x509_asn", True),
        ]

        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory, patch.object(
            quota_module.ssl,
            "enum_certificates",
            return_value=certificates,
            create=True,
        ), patch.object(
            quota_module.ssl,
            "DER_cert_to_PEM_cert",
            side_effect=lambda value: "PEM:" + value.decode("ascii") + "\n",
        ):
            bundle = quota_module._write_windows_root_ca_bundle(Path(directory))
            contents = bundle.read_text(encoding="ascii")

        self.assertEqual(contents.count("PEM:root-one"), 1)
        self.assertIn("PEM:root-two", contents)
        self.assertNotIn("email-only", contents)
        self.assertNotIn("pkcs-seven", contents)

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

    def test_parser_accepts_codex_multi_limit_response(self):
        output = json.dumps({
            "id": 3,
            "result": {
                "rateLimitsByLimitId": {
                    "codex": {
                        "planType": "pro",
                        "primary": {"usedPercent": 12},
                    }
                }
            },
        })

        value = parse_app_server_output(output)

        self.assertEqual(value["plan_type"], "pro")
        self.assertEqual(value["rate_limits"]["primary"]["usedPercent"], 12)

    def test_direct_usage_parser_normalizes_snake_case_without_identity_leak(self):
        value = quota_module.parse_direct_usage_payload({
            "email": "user@example.com",
            "user_id": "opaque-user-id",
            "account_id": "opaque-account-id",
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 53,
                    "limit_window_seconds": 18000,
                    "reset_at": 123,
                },
                "secondary_window": None,
            },
            "credits": {
                "has_credits": True,
                "unlimited": False,
                "balance": "42",
            },
            "rate_limit_reset_credits": {
                "available_count": 1,
                "credits": [{
                    "id": "opaque-reset-id",
                    "status": "available",
                    "expires_at": 456,
                }],
            },
        })

        self.assertEqual(value["account_label"], "u***@example.com")
        self.assertEqual(value["plan_type"], "plus")
        self.assertEqual(value["rate_limits"]["primary"]["usedPercent"], 53)
        self.assertEqual(value["rate_limits"]["primary"]["windowDurationMins"], 300)
        self.assertEqual(value["credits"]["balance"], "42")
        serialized = json.dumps(value)
        self.assertNotIn("opaque-user-id", serialized)
        self.assertNotIn("opaque-account-id", serialized)
        self.assertNotIn("opaque-reset-id", serialized)

    def test_direct_usage_get_keeps_credentials_in_request_headers_only(self):
        payload = json.dumps({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 18000,
                    "reset_at": 123,
                }
            },
        }).encode("utf-8")

        class FakeResponse:
            status = 200

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return False

            def getcode(self):
                return self.status

            def read(self, _size):
                return payload

        class FakeOpener:
            def __init__(self):
                self.request = None
                self.timeout = None

            def open(self, request, timeout):
                self.request = request
                self.timeout = timeout
                return FakeResponse()

        opener = FakeOpener()
        auth = {
            "tokens": {
                "access_token": "request-only-secret",
                "account_id": "request-only-account",
            }
        }
        with patch(
            "easy_multi_provider.quota._direct_usage_ssl_context"
        ), patch(
            "easy_multi_provider.quota.build_opener", return_value=opener
        ):
            value = quota_module._read_direct_chatgpt_quota(auth, 9)

        self.assertEqual(opener.timeout, 9)
        self.assertEqual(opener.request.get_method(), "GET")
        self.assertEqual(
            opener.request.get_header("Authorization"),
            "Bearer request-only-secret",
        )
        self.assertEqual(
            opener.request.get_header("Chatgpt-account-id"),
            "request-only-account",
        )
        self.assertNotIn("request-only-secret", json.dumps(value))
        self.assertNotIn("request-only-account", json.dumps(value))

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
            if os.name == "nt" and "SSL_CERT_FILE" not in os.environ:
                self.assertEqual(
                    Path(kwargs["env"]["SSL_CERT_FILE"]).parent,
                    Path(kwargs["env"]["CODEX_HOME"]),
                )
            self.assertNotIn("secret", process.stdin.body)
            self.assertEqual(json.loads(process.stdin.body.splitlines()[1])["method"], "initialized")
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

    def test_imported_quota_retries_without_refresh_for_external_codex_auth(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            write_encrypted_json(auth_file, {"tokens": {"access_token": "imported-secret"}})

            expected = {"plan_type": "plus"}
            with patch(
                "easy_multi_provider.quota._run_quota_query",
                side_effect=[
                    quota_module._CodexRequestError(2, -32600, "refresh unavailable"),
                    expected,
                ],
            ) as query, patch(
                "easy_multi_provider.quota.write_encrypted_json"
            ) as persisted:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary="codex"
                )

        self.assertIs(value, expected)
        self.assertEqual(query.call_count, 2)
        self.assertTrue(query.call_args_list[0].kwargs["allow_refresh"])
        self.assertFalse(query.call_args_list[1].kwargs["allow_refresh"])
        self.assertIsNone(query.call_args_list[1].kwargs["persist_path"])
        persisted.assert_not_called()

    def test_imported_quota_does_not_retry_rate_limit_request_errors(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            write_encrypted_json(auth_file, {"tokens": {"access_token": "imported-secret"}})

            with patch(
                "easy_multi_provider.quota._run_quota_query",
                side_effect=quota_module._CodexRequestError(
                    3, -32603, "rate limit transport failed"
                ),
            ) as query:
                with self.assertRaises(quota_module._CodexRequestError):
                    read_account_quota(
                        {"auth_file": str(auth_file)}, codex_binary="codex"
                    )

        self.assertEqual(query.call_count, 1)

    def test_imported_quota_falls_back_for_codex_0151_usage_transport_error(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            auth = {
                "tokens": {
                    "access_token": "imported-secret",
                    "account_id": "account-id",
                }
            }
            write_encrypted_json(auth_file, auth)
            expected = {"plan_type": "plus"}

            with patch(
                "easy_multi_provider.quota._run_quota_query",
                side_effect=quota_module._CodexRequestError(
                    3,
                    -32603,
                    "failed to fetch codex rate limits: error sending request for url "
                    "(https://chatgpt.com/backend-api/wham/usage)",
                ),
            ) as query, patch(
                "easy_multi_provider.quota._read_direct_chatgpt_quota",
                return_value=expected,
            ) as direct:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary="codex"
                )

        self.assertIs(value, expected)
        self.assertEqual(query.call_count, 1)
        direct.assert_called_once_with(auth, 45)

    def test_imported_quota_uses_direct_fallback_after_refresh_rejection(self):
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            account_dir = Path(directory) / "primary"
            account_dir.mkdir()
            auth_file = account_dir / "auth.json.enc"
            auth = {
                "tokens": {
                    "access_token": "imported-secret",
                    "account_id": "account-id",
                }
            }
            write_encrypted_json(auth_file, auth)
            expected = {"plan_type": "plus"}

            with patch(
                "easy_multi_provider.quota._run_quota_query",
                side_effect=[
                    quota_module._CodexRequestError(2, -32600, "refresh unavailable"),
                    quota_module._CodexRequestError(
                        3, -32603, "failed to fetch codex rate limits"
                    ),
                ],
            ) as query, patch(
                "easy_multi_provider.quota._read_direct_chatgpt_quota",
                return_value=expected,
            ) as direct:
                value = read_account_quota(
                    {"auth_file": str(auth_file)}, codex_binary="codex"
                )

        self.assertIs(value, expected)
        self.assertEqual(query.call_count, 2)
        direct.assert_called_once_with(auth, 45)


if __name__ == "__main__":
    unittest.main()
