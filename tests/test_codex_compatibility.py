import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.codex_compatibility import classify_codex_version
from easy_multi_provider.codex_runtime import (
    CodexRuntimeController,
    CommandResult,
)


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
        runner = RecordingRunner(CommandResult(0, "codex-cli 0.150.1\n", ""))
        controller = CodexRuntimeController(
            runner=runner,
            codex_executable="controlled-codex",
            control_timeout=20,
        )

        first = controller.compatibility()
        second = controller.compatibility()

        self.assertEqual(first, second)
        self.assertEqual(first["status"], "supported")
        self.assertEqual(
            runner.calls,
            [(("controlled-codex", "--version"), "", 5.0)],
        )

    def test_missing_cli_is_reported_without_command_output(self):
        runner = RecordingRunner(CommandResult(127, "", "command not found"))
        controller = CodexRuntimeController(
            runner=runner,
            codex_executable="missing-codex",
        )

        result = controller.compatibility()

        self.assertEqual(result["status"], "unavailable")
        self.assertIsNone(result["installed"])
        self.assertNotIn("command not found", str(result))

    def test_app_managed_runtime_is_preferred_and_path_cli_is_reported_separately(self):
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
                controller = CodexRuntimeController(
                    runner=runner,
                    target_codex_home=codex_home,
                )
                result = controller.compatibility()

        self.assertEqual(result["installed"], "0.151.4")
        self.assertEqual(result["source"], "managed")
        self.assertEqual(result["path_cli"]["installed"], "0.146.0")
        self.assertEqual(result["path_cli"]["status"], "unsupported")
        self.assertEqual(runner.calls[0][0], (str(managed), "--version"))
        self.assertEqual(runner.calls[1][0], (path_cli, "--version"))

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
                ).compatibility()

        self.assertEqual(result["installed"], "0.150.1")
        self.assertEqual(result["source"], "managed")
        self.assertEqual(runner.calls[0][0], (str(legacy), "--version"))

    def test_path_cli_is_not_repeated_when_it_is_the_selected_managed_runtime(self):
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
                ).compatibility()

        self.assertEqual(result["source"], "managed")
        self.assertNotIn("path_cli", result)
        self.assertEqual(len(runner.calls), 1)


if __name__ == "__main__":
    unittest.main()
