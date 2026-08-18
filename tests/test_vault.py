import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider.vault import MASTER_KEY_ENV, VaultError, read_encrypted_json, write_encrypted_json


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
            with patch.dict(os.environ, {}, clear=True):
                with self.assertRaises(VaultError):
                    write_encrypted_json(Path(directory) / "secret.enc", {"value": "x"})


if __name__ == "__main__":
    unittest.main()
