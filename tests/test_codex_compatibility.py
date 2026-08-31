import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.codex_compatibility import classify_codex_version
from easy_multi_provider.codex_runtime import CodexRuntimeController, CommandResult


class RecordingRunner:
    def __init__(self, result):
        self.result = result
        self.calls = []

    def run(self, args, *, input_text="", timeout=0):
        self.calls.append((tuple(args), input_text, timeout))
        return self.result


class VersionRunner:
    def __init__(self, versions):
        self.versions = versions
        self.calls = []

    def run(self, args, *, input_text="", timeout=0):
        self.calls.append((tuple(args), input_text, timeout))
        output = self.versions.get(str(args[0]))
        return (
            CommandResult(0, "codex-cli %s\n" % output, "")
            if output
            else CommandResult(127, "", "command not found")
        )


class CodexCompatibilityTests(unittest.TestCase):
    def test_supported_release_lines_are_classified_without_source_metadata(self):
        cases = (
            ("codex-cli 0.149.0", "0.149.0", "supported"),
            ("codex-cli 0.150.8", "0.150.8", "supported"),
            ("codex-cli 0.151.2", "0.151.2", "recommended"),
            ("codex-cli 0.151.2+vendor.1", "0.151.2+vendor.1", "recommended"),
        )
        for output, installed, status in cases:
            with self.subTest(output=output):
                public = classify_codex_version(output).public()
                self.assertEqual(public["installed"], installed)
                self.assertEqual(public["status"], status)
                self.assertEqual(public["supported_range"], "0.149.x–0.151.x")
                self.assertEqual(public["recommended"], "0.151.x")
                self.assertEqual(
                    set(public),
                    {"installed", "status", "supported_range", "recommended"},
                )

    def test_outside_and_prerelease_versions_are_explicit(self):
        cases = (
            ("codex-cli 0.148.9", "unsupported"),
            ("codex-cli 0.131.0-alpha.9", "unsupported"),
            ("codex-cli 0.152.0", "unverified"),
            ("codex-cli 1.0.0", "unverified"),
            ("codex-cli 0.151.0-rc.1", "unverified"),
            ("not a version", "unknown"),
            ("codex-cli 0.151.0unexpected", "unknown"),
        )
        for output, status in cases:
            with self.subTest(output=output):
                self.assertEqual(classify_codex_version(output).status, status)

    def test_runtime_probe_is_bounded_and_short_lived_cached(self):
        with tempfile.TemporaryDirectory() as temporary, patch(
            "easy_multi_provider.codex_runtime.shutil.which", return_value=None
        ):
            runner = RecordingRunner(CommandResult(0, "codex-cli 0.150.1\n", ""))
            controller = CodexRuntimeController(
                runner=runner,
                codex_executable="controlled-codex",
                control_timeout=20,
                runtime_user_home=Path(temporary),
            )
            first = controller.compatibility()
            second = controller.compatibility()

        self.assertEqual(first, second)
        self.assertEqual(first["status"], "supported")
        self.assertEqual(
            runner.calls,
            [(("controlled-codex", "--version"), "", 2.0)],
        )

    def test_missing_cli_is_reported_without_command_output(self):
        with tempfile.TemporaryDirectory() as temporary, patch(
            "easy_multi_provider.codex_runtime.shutil.which", return_value=None
        ):
            runner = RecordingRunner(CommandResult(127, "", "command not found"))
            controller = CodexRuntimeController(
                runner=runner,
                codex_executable="missing-codex",
                runtime_user_home=Path(temporary),
            )
            result = controller.compatibility()

        self.assertEqual(result["status"], "unavailable")
        self.assertIsNone(result["installed"])
        self.assertIsNone(result["helper_source"])
        self.assertFalse(result["runtimes"][0]["selectable"])
        self.assertNotIn("command not found", str(result))

    def test_managed_runtime_is_selected_and_path_cli_is_listed_separately(self):
        with tempfile.TemporaryDirectory() as temporary:
            codex_home = Path(temporary)
            managed = (
                codex_home
                / "packages"
                / "standalone"
                / "current"
                / "bin"
                / ("codex.exe" if os.name == "nt" else "codex")
            )
            managed.parent.mkdir(parents=True)
            managed.write_bytes(b"managed")
            managed.chmod(0o755)
            path_cli = str(codex_home / "path" / "codex")
            runner = VersionRunner({str(managed): "0.151.4", path_cli: "0.146.0"})

            with patch(
                "easy_multi_provider.codex_runtime.shutil.which",
                return_value=path_cli,
            ):
                result = CodexRuntimeController(
                    runner=runner,
                    target_codex_home=codex_home,
                    runtime_user_home=codex_home / "empty-home",
                ).compatibility()

        self.assertEqual(result["installed"], "0.151.4")
        self.assertEqual(result["source"], "managed")
        runtimes = {item["source"]: item for item in result["runtimes"]}
        self.assertEqual(runtimes["path_cli"]["installed"], "0.146.0")
        self.assertEqual(runtimes["path_cli"]["status"], "unsupported")
        self.assertFalse(runtimes["path_cli"]["selectable"])
        self.assertEqual(
            {call[0] for call in runner.calls},
            {(str(managed), "--version"), (path_cli, "--version")},
        )

    def test_windows_app_and_cursor_extension_are_discovered_and_selectable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app_runtime = (
                root
                / "local-app-data"
                / "OpenAI"
                / "Codex"
                / "bin"
                / "build-identity"
                / "codex.exe"
            )
            app_runtime.parent.mkdir(parents=True)
            app_runtime.write_bytes(b"app")
            cursor_runtime = (
                root
                / "home"
                / ".cursor"
                / "extensions"
                / "openai.chatgpt-26.721.30844-win32-x64"
                / "bin"
                / "windows-x86_64"
                / "codex.exe"
            )
            cursor_runtime.parent.mkdir(parents=True)
            cursor_runtime.write_bytes(b"cursor")
            app_executable = str(app_runtime.resolve())
            cursor_executable = str(cursor_runtime.resolve())
            runner = VersionRunner(
                {app_executable: "0.151.0-alpha.7.2", cursor_executable: "0.150.0"}
            )

            with patch(
                "easy_multi_provider.codex_runtime.shutil.which", return_value=None
            ):
                controller = CodexRuntimeController(
                    runner=runner,
                    windows_local_app_data=root / "local-app-data",
                    runtime_user_home=root / "home",
                )
                automatic = controller.compatibility()
                controller.set_runtime_preferences(["codex_app", "cursor"])
                multiple = controller.compatibility()
                controller.set_runtime_preferences(["codex_app"])
                selected = controller.compatibility()
                selected_executable = controller.executable()

        automatic_runtimes = {
            item["source"]: item for item in automatic["runtimes"]
        }
        self.assertEqual(automatic["source"], "cursor")
        self.assertEqual(automatic["status"], "supported")
        self.assertEqual(automatic_runtimes["cursor"]["installed"], "0.150.0")
        self.assertTrue(automatic_runtimes["codex_app"]["selectable"])
        self.assertTrue(automatic_runtimes["cursor"]["selectable"])
        self.assertEqual(multiple["preferences"], ["codex_app", "cursor"])
        self.assertTrue(all(item["targeted"] for item in multiple["runtimes"]))
        self.assertEqual(multiple["helper_source"], "cursor")
        self.assertEqual(selected["preferences"], ["codex_app"])
        self.assertEqual(selected["helper_source"], "cursor")
        self.assertEqual(selected_executable, cursor_executable)

    def test_legacy_managed_runtime_path_remains_a_fallback(self):
        with tempfile.TemporaryDirectory() as temporary:
            codex_home = Path(temporary)
            legacy = (
                codex_home
                / "packages"
                / "standalone"
                / "current"
                / ("codex.exe" if os.name == "nt" else "codex")
            )
            legacy.parent.mkdir(parents=True)
            legacy.write_bytes(b"legacy-managed")
            legacy.chmod(0o755)
            path_cli = str(codex_home / "path" / "codex")
            runner = VersionRunner({str(legacy): "0.150.1", path_cli: "0.146.0"})

            with patch(
                "easy_multi_provider.codex_runtime.shutil.which",
                return_value=path_cli,
            ):
                result = CodexRuntimeController(
                    runner=runner,
                    target_codex_home=codex_home,
                    runtime_user_home=codex_home / "empty-home",
                ).compatibility()

        self.assertEqual(result["installed"], "0.150.1")
        self.assertEqual(result["source"], "managed")

    def test_same_executable_is_not_repeated_for_managed_and_path_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            codex_home = Path(temporary)
            managed = (
                codex_home
                / "packages"
                / "standalone"
                / "current"
                / "bin"
                / ("codex.exe" if os.name == "nt" else "codex")
            )
            managed.parent.mkdir(parents=True)
            managed.write_bytes(b"managed")
            managed.chmod(0o755)
            runner = VersionRunner({str(managed): "0.151.4"})

            with patch(
                "easy_multi_provider.codex_runtime.shutil.which",
                return_value=str(managed),
            ):
                result = CodexRuntimeController(
                    runner=runner,
                    target_codex_home=codex_home,
                    runtime_user_home=codex_home / "empty-home",
                ).compatibility()

        self.assertEqual(result["source"], "managed")
        self.assertEqual(len(result["runtimes"]), 1)
        self.assertEqual(len(runner.calls), 1)

    def test_runtime_allowlist_fails_closed_when_its_only_source_is_unsupported(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            managed = (
                root
                / "packages"
                / "standalone"
                / "current"
                / "bin"
                / ("codex.exe" if os.name == "nt" else "codex")
            )
            managed.parent.mkdir(parents=True)
            managed.write_bytes(b"managed")
            managed.chmod(0o755)
            path_cli = str(root / "path" / "codex")
            runner = VersionRunner({str(managed): "0.148.9", path_cli: "0.150.2"})

            with patch(
                "easy_multi_provider.codex_runtime.shutil.which",
                return_value=path_cli,
            ):
                result = CodexRuntimeController(
                    runner=runner,
                    target_codex_home=root,
                    runtime_user_home=root / "empty-home",
                    runtime_preferences=["managed"],
                ).compatibility()

        runtimes = {item["source"]: item for item in result["runtimes"]}
        self.assertFalse(runtimes["managed"]["selectable"])
        self.assertFalse(runtimes["managed"]["targeted"])
        self.assertFalse(runtimes["path_cli"]["targeted"])
        self.assertEqual(result["helper_source"], "path_cli")
        self.assertTrue(runtimes["path_cli"]["helper"])


if __name__ == "__main__":
    unittest.main()
