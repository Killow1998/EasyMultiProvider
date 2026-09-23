import importlib.util
import gzip
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch


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

    def test_sampler_case_peak_reset_preserves_overall_peak(self):
        sampler = benchmark.ResourceSampler(42)
        sampler.reset_peak(100)
        with patch.object(benchmark, "_tree_metrics", return_value={"rss_bytes": 150}):
            self.assertEqual(sampler.case_peak(), 150)
            self.assertEqual(sampler.overall_peak(), 150)
        with patch.object(benchmark, "_tree_metrics", return_value={"rss_bytes": 260}):
            self.assertEqual(sampler.case_peak(), 260)
        sampler.reset_peak(120)
        with patch.object(benchmark, "_tree_metrics", return_value={"rss_bytes": 140}):
            self.assertEqual(sampler.case_peak(), 140)
            self.assertEqual(sampler.overall_peak(), 260)

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

    def test_request_payload_encodings_roundtrip(self):
        payload = b'{"input":"compressible fixture"}' * 100
        self.assertEqual(benchmark._encode_request_payload(payload, "identity"), payload)
        gzip_payload = benchmark._encode_request_payload(payload, "gzip")
        self.assertEqual(gzip.decompress(gzip_payload), payload)
        try:
            import zstandard
        except ImportError:
            self.skipTest("zstandard is required for the zstd encoding roundtrip")
        zstd_payload = benchmark._encode_request_payload(payload, "zstd")
        self.assertEqual(zstandard.ZstdDecompressor().decompress(zstd_payload), payload)

    def test_request_headers_only_add_nonidentity_content_encoding_for_payloads(self):
        identity = benchmark._request_headers(payload_present=True)
        self.assertNotIn("Content-Encoding", identity)
        self.assertEqual(identity["Authorization"], "Bearer " + benchmark.FIXTURE_CALLER_KEY)
        for encoding in ("gzip", "zstd"):
            headers = benchmark._request_headers(
                payload_present=True, content_encoding=encoding
            )
            self.assertEqual(headers["Content-Encoding"], encoding)
            self.assertEqual(headers["Authorization"], "Bearer " + benchmark.FIXTURE_CALLER_KEY)
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark._request_headers(payload_present=False, content_encoding="gzip")

    def test_workload_request_forwards_encoding_and_timeout(self):
        service = SimpleNamespace(port=4200)
        with patch.object(benchmark, "_request", return_value={}) as request:
            benchmark._workload_request(
                service, "responses_nonstream", b"wire fixture",
                content_encoding="zstd", timeout=45.0,
            )
        request.assert_called_once_with(
            4200, "POST", "/v1/responses", payload=b"wire fixture",
            operation="responses_nonstream", content_encoding="zstd", timeout=45.0,
        )

    def test_large_history_cases_report_encoding_and_wire_logical_sizes(self):
        service = SimpleNamespace(
            process=SimpleNamespace(pid=42),
            sampler=SimpleNamespace(
                reset_peak=Mock(), case_peak=lambda: 120, overall_peak=lambda: 120
            ),
        )
        upstream = SimpleNamespace(request_count=0)
        observed_encodings = []

        def fake_workload(_service, kind, _payload, *, content_encoding, timeout):
            if kind.startswith("responses_"):
                upstream.request_count += 1
                observed_encodings.append((kind, content_encoding))
            if kind == "responses_sse":
                return {
                    "status": 200, "elapsed_ms": 1.0, "first_event_ms": 0.5,
                    "event_types": benchmark.EXPECTED_SSE_EVENT_TYPES,
                    "terminal": "response.completed", "text": benchmark.REPLY_TEXT,
                }
            return {
                "status": 200, "elapsed_ms": 1.0, "first_event_ms": None,
                "body": json.dumps(benchmark._response_value(benchmark.UPSTREAM_MODEL)).encode(),
                "content_encoding": content_encoding, "timeout": timeout,
            }

        with patch.object(
            benchmark, "_tree_metrics",
            return_value={"cpu_seconds": 1.0, "rss_bytes": 100},
        ), patch.object(benchmark, "_workload_request", side_effect=fake_workload):
            result = benchmark._run_measurements(
                service, upstream, {}, iterations=1, warmup=0,
                concurrencies=[], payload_sizes=[], large_history_sizes_mib=[1],
                large_iterations=1,
            )

        cases = result["cases"]
        for kind in ("responses_nonstream", "responses_sse"):
            encoded = {
                name.split("_")[-3]: metrics
                for name, metrics in cases.items()
                if name.startswith(kind + "_large_history_")
            }
            self.assertEqual(set(encoded), {"identity", "gzip", "zstd"})
            for encoding, metrics in encoded.items():
                self.assertEqual(metrics["content_encoding"], encoding)
                self.assertGreater(metrics["logical_payload_bytes"], 0)
                self.assertEqual(metrics["wire_payload_bytes"], metrics["request_payload_bytes"])
                if encoding != "identity":
                    self.assertLess(metrics["wire_payload_bytes"], metrics["logical_payload_bytes"])
        self.assertEqual(upstream.request_count, 6)
        self.assertCountEqual(
            observed_encodings,
            [
                (kind, encoding)
                for kind in ("responses_nonstream", "responses_sse")
                for encoding in ("identity", "gzip", "zstd")
            ],
        )
        serialized_cases = json.dumps(cases)
        for sensitive_value in (
            benchmark.FIXTURE_CALLER_KEY,
            benchmark.FIXTURE_PROVIDER_KEY,
            benchmark.REPLY_TEXT,
            "input_text",
        ):
            self.assertNotIn(sensitive_value, serialized_cases)
        self.assertEqual(benchmark.BENCHMARK_CONTEXT_WINDOW, 100_000_000)

    def test_measure_case_marks_wrong_response_content_invalid(self):
        service = SimpleNamespace(
            port=4200,
            cookie="fixture-cookie",
            process=SimpleNamespace(pid=42),
            sampler=SimpleNamespace(reset_peak=Mock(), case_peak=lambda: 120),
        )
        upstream = SimpleNamespace(request_count=0)
        bad_value = benchmark._response_value(benchmark.UPSTREAM_MODEL)
        bad_value["output"][0]["content"][0]["text"] = "wrong fixture text"

        def wrong_content(*_args, **_kwargs):
            upstream.request_count += 1
            return {
                "status": 200, "elapsed_ms": 1.0, "first_event_ms": None,
                "body": json.dumps(bad_value).encode(),
            }

        with patch.object(benchmark, "_workload_request", side_effect=wrong_content), patch.object(
            benchmark, "_tree_metrics",
            side_effect=[
                {"cpu_seconds": 1.0, "rss_bytes": 100},
                {"cpu_seconds": 1.25, "rss_bytes": 110},
            ],
        ):
            result = benchmark._measure_case(
                service, upstream, "responses_nonstream", b"wire", 1, 0, 1,
                logical_history_bytes=16 * 1024 * 1024,
                logical_payload_bytes=100,
                content_encoding="gzip",
            )
        self.assertEqual(result["errors"], 1)
        self.assertEqual(result["error_kinds"], {"content_mismatch": 1})
        self.assertTrue(result["upstream_count_matches"])

    def test_measure_case_reports_cpu_latency_and_upstream_count(self):
        service = SimpleNamespace(
            port=4200,
            cookie="fixture-cookie",
            process=SimpleNamespace(pid=42),
            sampler=SimpleNamespace(reset_peak=Mock(), case_peak=lambda: 120),
        )
        upstream = SimpleNamespace(request_count=0)

        def response(*_args, **_kwargs):
            upstream.request_count += 1
            return {
                "status": 200,
                "elapsed_ms": 2.0,
                "first_event_ms": 0.5,
                "event_types": benchmark.EXPECTED_SSE_EVENT_TYPES,
                "text": benchmark.REPLY_TEXT,
                "terminal": "response.completed",
            }

        with patch.object(benchmark, "_workload_request", side_effect=response), patch.object(
            benchmark, "_tree_metrics",
            side_effect=[
                {"cpu_seconds": 1.0, "rss_bytes": 100},
                {"cpu_seconds": 1.25, "rss_bytes": 110},
            ],
        ):
            result = benchmark._measure_case(
                service, upstream, "responses_sse", b"fixture payload", 3, 0, 1,
                logical_history_bytes=7,
            )
        self.assertEqual(result["count"], 3)
        self.assertEqual(result["errors"], 0)
        self.assertEqual(result["cpu_seconds"], 0.25)
        self.assertEqual(result["upstream_requests"], 3)
        self.assertTrue(result["upstream_count_matches"])
        self.assertEqual(result["first_event_p50_ms"], 0.5)
        self.assertEqual(service.sampler.reset_peak.call_args.args, (100,))
        self.assertEqual(result["case_start_rss_bytes"], 100)
        self.assertEqual(result["case_peak_rss_bytes"], 120)
        self.assertEqual(result["logical_history_bytes"], 7)
        self.assertEqual(result["content_encoding"], "identity")
        self.assertEqual(result["request_payload_bytes"], len(b"fixture payload"))
        self.assertNotIn("fixture payload", json.dumps(result))

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
        self.assertEqual(args.large_history_sizes, [])
        self.assertEqual(args.large_iterations, 1)

    def test_large_history_cli_validation_is_opt_in_and_bounded(self):
        args = benchmark._parse_args([
            "--rust-binary", "/rust/EMP",
            "--iterations", "60",
            "--concurrency", "1", "16",
            "--large-history-sizes", "16", "64", "128",
            "--large-iterations", "2",
        ])
        benchmark._validate_parameters(
            args.iterations, args.warmup, args.concurrency, args.payload_sizes,
            args.large_history_sizes, args.large_iterations,
        )
        self.assertEqual(args.large_history_sizes, [16, 64, 128])
        self.assertEqual(args.large_iterations, 2)
        for sizes, count in (([15], 1), ([129], 1), ([16], 4)):
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark._validate_parameters(1, 0, [1], [1024], sizes, count)

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
