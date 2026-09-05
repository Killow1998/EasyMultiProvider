"""Keep the desktop bundle and an older standalone Codex runtime distinct."""
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.codex_runtime import CodexRuntimeController
from tests.test_codex_compatibility import VersionRunner


class MacRuntimeDiscoveryTests(unittest.TestCase):
    def controller(self, files, versions=None, system="Darwin"):
        for target, value in (("platform.system", system), ("shutil.which", None)):
            mock = patch("easy_multi_provider.codex_runtime." + target, return_value=value)
            mock.start()
            self.addCleanup(mock.stop)
        mock = patch.object(CodexRuntimeController, "_runtime_file", side_effect=lambda p: files.get(str(p)))
        mock.start()
        self.addCleanup(mock.stop)
        mock = patch.object(CodexRuntimeController, "_editor_codex_executable", return_value=None)
        mock.start()
        self.addCleanup(mock.stop)
        return CodexRuntimeController(runtime_user_home=Path("/fixture-home"),
            target_codex_home=Path("/fixture-home/.codex"), runner=VersionRunner(versions or {}))

    def test_app_and_standalone_versions_are_separate_and_rescan_reads_new_binary(self):
        app = str(Path("/Applications/ChatGPT.app/Contents/Resources/codex"))
        old = str(Path("/fixture-home/.codex/packages/standalone/current/bin/codex"))
        controller = self.controller({app: app}, {app: "0.153.1", old: "0.149.0"})
        with patch.object(controller, "_codex_home_managed_executable", return_value=old):
            initial = {r["source"]: r for r in controller.compatibility()["runtimes"]}
            self.assertEqual(initial["codex_app"]["installed"], "0.153.1")
            self.assertEqual(initial["managed"]["installed"], "0.149.0")
            controller.runner = VersionRunner({app: "0.153.4", old: "0.149.0"})
            updated = controller.refresh_compatibility()
            self.assertEqual(updated["helper_source"], "codex_app")
            self.assertEqual(next(r for r in updated["runtimes"] if r["source"] == "codex_app")["installed"], "0.153.4")

    def test_user_app_and_legacy_codex_app_locations(self):
        for folder in ("/fixture-home/Applications/ChatGPT.app", "/Applications/Codex.app"):
            with self.subTest(folder=folder):
                binary = str(Path(folder) / "Contents/Resources/codex")
                controller = self.controller({binary: binary})
                self.assertEqual(controller._mac_app_codex_executable(), binary)

    def test_plugin_copy_is_only_used_when_no_bundle_exists(self):
        app = str(Path("/Applications/ChatGPT.app/Contents/Resources/codex"))
        controller = self.controller({app: app})
        plugin = str(Path(controller.target_codex_home) / "plugins/.plugin-appserver/codex")
        with patch.object(controller, "_runtime_file", side_effect=lambda p: {app: app, plugin: plugin}.get(str(p))):
            self.assertEqual(controller._mac_app_codex_executable(), app)
        with patch.object(controller, "_runtime_file", side_effect=lambda p: {plugin: plugin}.get(str(p))):
            self.assertEqual(controller._mac_app_codex_executable(), plugin)

    def test_other_platforms_do_not_probe_mac_apps(self):
        controller = self.controller({}, system="Linux")
        self.assertIsNone(controller._mac_app_codex_executable())
        controller._runtime_file.assert_not_called()
