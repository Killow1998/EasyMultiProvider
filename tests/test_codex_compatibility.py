import unittest

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


if __name__ == "__main__":
    unittest.main()
