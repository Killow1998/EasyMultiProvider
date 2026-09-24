import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.config import config_path, load, normalize, save


class ConfigPathTests(unittest.TestCase):
    def test_default_load_and_save_ignore_checkout_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            checkout = root / "checkout"
            checkout.mkdir()
            checkout_config = checkout / "config.json"
            checkout_config.write_text(json.dumps({"port": 4301}), encoding="utf-8")
            xdg_config = root / "xdg" / "easy-multi-provider" / "config.json"
            xdg_config.parent.mkdir(parents=True)
            xdg_config.write_text(json.dumps({"port": 4302}), encoding="utf-8")
            previous = Path.cwd()
            try:
                os.chdir(checkout)
                with patch.dict(os.environ, {
                    "XDG_CONFIG_HOME": str(root / "xdg"),
                    "EASY_MULTI_PROVIDER_CONFIG": "",
                }):
                    self.assertEqual(config_path(), xdg_config)
                    self.assertEqual(load()["port"], 4302)
                    saved = save(normalize({"port": 4303}))
            finally:
                os.chdir(previous)
            self.assertEqual(saved, xdg_config)
            self.assertEqual(json.loads(checkout_config.read_text())["port"], 4301)
            self.assertEqual(json.loads(xdg_config.read_text())["port"], 4303)


if __name__ == "__main__":
    unittest.main()
