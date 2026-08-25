import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.search_integration import (
    SearchFeatureManager,
    SearchIntegrationError,
)


class SearchIntegrationTests(unittest.TestCase):
    def test_apply_and_restore_only_owned_codex_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config.toml"
            lease = root / "search.json"
            config.write_text(
                'model = "native"\n[features]\nunified_exec = true\n',
                encoding="utf-8",
            )
            manager = SearchFeatureManager(config, lease)

            manager.apply(True)
            applied = config.read_text(encoding="utf-8")
            self.assertIn('web_search = "live"', applied)
            self.assertIn("standalone_web_search = true", applied)
            self.assertIn("unified_exec = true", applied)

            manager.restore()
            restored = config.read_text(encoding="utf-8")
            self.assertIn('model = "native"', restored)
            self.assertIn("unified_exec = true", restored)
            self.assertNotIn("web_search", restored)

    def test_restore_fails_closed_if_owned_field_changed_externally(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "config.toml"
            lease = root / "search.json"
            config.write_text('model = "native"\n', encoding="utf-8")
            manager = SearchFeatureManager(config, lease)
            manager.apply(True)
            config.write_text(
                'model = "native"\nweb_search = "disabled"\n'
                "[features]\nstandalone_web_search = true\n",
                encoding="utf-8",
            )

            with self.assertRaises(SearchIntegrationError):
                manager.restore()

            self.assertIn(
                'web_search = "disabled"', config.read_text(encoding="utf-8")
            )


if __name__ == "__main__":
    unittest.main()
