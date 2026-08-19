import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider.vault import (
    MASTER_KEY_ENV,
    MASTER_KEY_FILE_ENV,
    VaultError,
    read_encrypted_json,
    write_encrypted_json,
)


class VaultTests(unittest.TestCase):
    def test_encrypted_file_round_trip_never_writes_plaintext(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json.enc"
            value = {"tokens": {"access_token": "do-not-store-plain"}}
            with patch.dict(os.environ, {MASTER_KEY_ENV: Fernet.generate_key().decode("ascii")}, clear=False):
                write_encrypted_json(path, value)
                self.assertNotIn("do-not-store-plain", path.read_text(encoding="utf-8"))
                self.assertEqual(read_encrypted_json(path), value)
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_missing_master_key_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = str(Path(directory) / "missing.key")
            with patch.dict(os.environ, {MASTER_KEY_FILE_ENV: missing}, clear=True):
                with self.assertRaises(VaultError):
                    write_encrypted_json(Path(directory) / "secret.enc", {"value": "x"})

    def test_private_master_key_file_is_used_when_env_is_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            key_path = Path(directory) / "master.key"
            key_path.write_text(Fernet.generate_key().decode("ascii"), encoding="utf-8")
            if os.name != "nt":
                key_path.chmod(0o600)
            value_path = Path(directory) / "secret.enc"
            with patch.dict(os.environ, {MASTER_KEY_FILE_ENV: str(key_path)}, clear=True):
                write_encrypted_json(value_path, {"value": "file-key"})
                self.assertEqual(read_encrypted_json(value_path), {"value": "file-key"})


if __name__ == "__main__":
    unittest.main()
