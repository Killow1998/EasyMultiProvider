import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "tools" / "benchmark_emp_runtime.py"
SPEC = importlib.util.spec_from_file_location("benchmark_emp_runtime", SCRIPT)
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkEmpRuntimeTests(unittest.TestCase):
    def test_percentile_uses_interpolation_and_empty_is_null(self):
        self.assertIsNone(benchmark.percentile([], 0.5))
        self.assertEqual(benchmark.percentile([1, 2, 3, 4], 0), 1)
        self.assertEqual(benchmark.percentile([1, 2, 3, 4], 1), 4)
        self.assertEqual(benchmark.percentile([0, 10], 0.95), 9.5)
        with self.assertRaises(ValueError):
            benchmark.percentile([1], 1.1)

    def test_latency_summary_schema_contains_only_metrics(self):
        summary = benchmark.latency_summary([1.0, 3.0, 5.0], 2.0, 0)
        self.assertEqual(summary, {
            "count": 3,
            "errors": 0,
            "throughput_requests_per_second": 1.5,
            "p50_ms": 3.0,
            "p95_ms": 4.8,
            "p99_ms": 4.96,
        })
        self.assertFalse({"request", "body", "headers", "authorization"} & set(summary))

    def test_semantic_diff_reports_paths_without_values_or_secrets(self):
        left = {"status": 200, "authorization": benchmark.FIXTURE_CALLER_KEY}
        right = {"status": 502, "authorization": benchmark.FIXTURE_PROVIDER_KEY}
        differences = benchmark.semantic_diff(left, right)
        self.assertEqual(differences, ["$.authorization", "$.status"])
        rendered = repr(differences)
        self.assertNotIn(benchmark.FIXTURE_CALLER_KEY, rendered)
        self.assertNotIn(benchmark.FIXTURE_PROVIDER_KEY, rendered)

    def test_bootstrap_target_uses_runtime_specific_contract(self):
        url = "http://127.0.0.1:54321/?bootstrap=fixture-token"
        self.assertEqual(benchmark._bootstrap_target("python", url), "/")
        self.assertEqual(
            benchmark._bootstrap_target("rust", url), "/?bootstrap=fixture-token"
        )

    def test_models_semantic_reads_openai_data_ids(self):
        result = {
            "status": 200,
            "body": benchmark.json.dumps({
                "object": "list",
                "data": [{"id": "benchmark/model", "object": "model"}],
            }).encode(),
        }
        self.assertEqual(
            benchmark._management_semantic(result, "models"),
            {"status": 200, "model_slugs": ["benchmark/model"]},
        )

    def test_safe_error_name_never_serializes_exception_details(self):
        error = OSError("Authorization: " + benchmark.FIXTURE_PROVIDER_KEY)
        self.assertEqual(benchmark.safe_error_name(error), "OSError")
        safe = benchmark.BenchmarkError("semantic_mismatch")
        self.assertEqual(benchmark.safe_error_name(safe), "semantic_mismatch")
        self.assertNotIn(benchmark.FIXTURE_PROVIDER_KEY, benchmark.safe_error_name(error))

    def test_fake_upstream_rejects_wrong_auth_model_and_path(self):
        body = {"model": benchmark.UPSTREAM_MODEL, "stream": False}
        self.assertEqual(
            benchmark._fake_upstream_status(
                "/v1/responses", "Bearer wrong-fixture-key", body
            ),
            401,
        )
        self.assertEqual(
            benchmark._fake_upstream_status(
                "/v1/responses", "Bearer " + benchmark.FIXTURE_PROVIDER_KEY,
                {"model": "wrong-model"},
            ),
            400,
        )
        self.assertEqual(
            benchmark._fake_upstream_status(
                "/wrong-path", "Bearer " + benchmark.FIXTURE_PROVIDER_KEY, body
            ),
            404,
        )

    def test_metric_and_error_report_json_redacts_fixture_secrets_and_payload(self):
        public = {
            "metrics": benchmark.latency_summary([1.0], 1.0, 0),
            "errors": [benchmark.safe_error_name(
                OSError("Bearer " + benchmark.FIXTURE_CALLER_KEY)
            )],
            "semantic_difference_paths": ["$.status"],
        }
        serialized = json.dumps(public)
        for secret in (
            benchmark.FIXTURE_CALLER_KEY,
            benchmark.FIXTURE_PROVIDER_KEY,
            "PRIVATE_REQUEST_BODY_SENTINEL",
        ):
            self.assertNotIn(secret, serialized)

    def test_sse_fixture_has_completed_terminal_and_stable_text(self):
        events = [
            benchmark.json.loads(line[6:])
            for line in benchmark._sse_payload(benchmark.UPSTREAM_MODEL).decode().splitlines()
            if line.startswith("data: ")
        ]
        self.assertEqual(events[0]["type"], "response.created")
        self.assertEqual(events[-1]["type"], "response.completed")
        self.assertEqual(
            [event["type"] for event in events].count("response.output_text.delta"), 1
        )
        self.assertEqual(
            events[-1]["response"]["output"][0]["content"][0]["text"],
            benchmark.REPLY_TEXT,
        )

    def test_measure_case_reports_cpu_latency_and_upstream_count(self):
        service = SimpleNamespace(
            port=4200,
            cookie="fixture-cookie",
            process=SimpleNamespace(pid=42),
        )
        upstream = SimpleNamespace(request_count=0)

        def response(*_args):
            upstream.request_count += 1
            return {
                "status": 200,
                "elapsed_ms": 2.0,
                "first_event_ms": 0.5,
                "terminal": "response.completed",
            }

        with patch.object(benchmark, "_workload_request", side_effect=response), patch.object(
            benchmark, "_tree_metrics",
            side_effect=[{"cpu_seconds": 1.0}, {"cpu_seconds": 1.25}],
        ):
            result = benchmark._measure_case(
                service, upstream, "responses_sse", b"fixture payload", 3, 0, 1
            )
        self.assertEqual(result["count"], 3)
        self.assertEqual(result["errors"], 0)
        self.assertEqual(result["cpu_seconds"], 0.25)
        self.assertEqual(result["upstream_requests"], 3)
        self.assertTrue(result["upstream_count_matches"])
        self.assertEqual(result["first_event_p50_ms"], 0.5)

    def test_cli_accepts_benchmark_paths_iterations_and_concurrency(self):
        args = benchmark._parse_args([
            "--python-root", "/python/oracle",
            "--python", "/python/bin/python",
            "--rust-binary", "/rust/EMP",
            "--output", "/tmp/result.json",
            "--iterations", "7",
            "--concurrency", "1", "16",
        ])
        self.assertEqual(args.python_root, Path("/python/oracle"))
        self.assertEqual(args.python, Path("/python/bin/python"))
        self.assertEqual(args.rust_binary, Path("/rust/EMP"))
        self.assertEqual(args.output, Path("/tmp/result.json"))
        self.assertEqual(args.iterations, 7)
        self.assertEqual(args.concurrency, [1, 16])

    def test_python_launcher_preserves_venv_symlink_path(self):
        with tempfile.TemporaryDirectory(prefix="benchmark-python-launcher-") as directory:
            root = Path(directory)
            target = root / "system-python"
            target.write_text("launcher target", encoding="utf-8")
            venv = root / ".venv" / "bin"
            venv.mkdir(parents=True)
            launcher = venv / "python"
            launcher.symlink_to(target)
            selected = benchmark._python_launcher(launcher, root)
            self.assertEqual(selected, launcher)
            self.assertNotEqual(selected, target)


if __name__ == "__main__":
    unittest.main()
