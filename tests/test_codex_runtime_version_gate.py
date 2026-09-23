import io
import json
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest.mock import Mock, patch

from tools import test_codex_runtime


class CodexRuntimeVersionGateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.binary = Path(self.temporary.name) / "codex"
        self.binary.write_text("fixture binary path", encoding="utf-8")

    def _run(self, version, arguments):
        version_result = Mock(
            returncode=0,
            stdout=f"codex-cli {version}\n",
            stderr="",
        )
        catalog = json.dumps({"models": [{"slug": "fixture/model"}]}).encode()
        output = io.StringIO()
        with patch.object(
            test_codex_runtime.subprocess, "run", return_value=version_result
        ) as run, patch.object(
            test_codex_runtime.subprocess, "check_output", return_value=catalog
        ) as models, patch.object(
            test_codex_runtime.subprocess, "call", return_value=0
        ) as tests, redirect_stderr(output):
            code = test_codex_runtime.main([str(self.binary), *arguments])
        result_line = next(
            line
            for line in output.getvalue().splitlines()
            if line.startswith("CODEX_RUNTIME_RESULT=")
        )
        result = json.loads(result_line.split("=", 1)[1])
        return code, result, run, models, tests

    def test_required_version_passes_and_records_actual_version(self):
        selected_test = (
            "tests.test_codex_metadata_cli.CodexMetadataCliTests."
            "test_native_upstream_websocket_metadata_and_tool_followup"
        )
        code, result, version_run, _, tests = self._run(
            "0.156.1", ["--require-version", "0.156.1", "--test", selected_test]
        )
        self.assertEqual(code, 0)
        self.assertEqual(result["version"], "0.156.1")
        self.assertEqual(result["expected_version"], "0.156.1")
        self.assertEqual(result["status"], "passed")
        self.assertEqual(version_run.call_args.args[0][-1], "--version")
        self.assertTrue(any(selected_test in argument for argument in tests.call_args.args[0]))

    def test_required_version_mismatch_stops_before_consumer_tests(self):
        code, result, _, models, tests = self._run(
            "0.155.4", ["--require-version", "0.156.1"]
        )
        self.assertEqual(code, 2)
        self.assertEqual(result["version"], "0.155.4")
        self.assertEqual(result["status"], "version_mismatch")
        models.assert_not_called()
        tests.assert_not_called()

    def test_compatibility_mode_keeps_unrecognized_version_nonfatal(self):
        code, result, _, models, tests = self._run(
            "0.156.1-unknown",
            ["--test", "tests.test_codex_metadata_cli.CodexMetadataCliTests"],
        )
        self.assertEqual(code, 0)
        self.assertIsNone(result["version"])
        self.assertIsNone(result["expected_version"])
        models.assert_called_once()
        tests.assert_called_once()


if __name__ == "__main__":
    unittest.main()
