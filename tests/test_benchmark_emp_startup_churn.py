import unittest

from tools.benchmark_emp_startup_churn import (
    ChurnError,
    CycleState,
    _parse_args,
    _startup_path,
    linear_slope,
    post_warmup_slopes,
    readiness_gate,
    readiness_summary,
    resource_slope_gate,
    shutdown_summary,
    strict_json_equal,
    validate_parameters,
)


class StartupChurnBenchmarkTests(unittest.TestCase):
    def test_cli_supports_short_cycles_and_one_hour_duration(self):
        args = _parse_args(["--rust-binary", "/tmp/EMP", "--cycles", "3"])
        self.assertEqual(args.cycles, 3)
        self.assertEqual(args.duration_seconds, 0)
        self.assertEqual(args.warmup_cycles, 1)
        long_run = _parse_args([
            "--rust-binary", "/tmp/EMP", "--duration-seconds", "3600",
        ])
        self.assertEqual(long_run.duration_seconds, 3600)

    def test_parameter_validation_keeps_warmup_and_timeouts_bounded(self):
        validate_parameters(3, 0, 1, 30, 8)
        validate_parameters(5, 3600, 1, 30, 8)
        with self.assertRaisesRegex(ChurnError, "warmup_cycles"):
            validate_parameters(1, 0, 1, 30, 8)
        with self.assertRaisesRegex(ChurnError, "duration"):
            validate_parameters(5, -1, 1, 30, 8)
        with self.assertRaisesRegex(ChurnError, "startup_timeout"):
            validate_parameters(5, 0, 1, 121, 8)

    def test_cycle_state_machine_accepts_only_observable_order(self):
        state = CycleState()
        for next_state in ("spawned", "listening", "ready", "forwarded",
                           "shutdown_requested", "exited"):
            state.transition(next_state)
        self.assertEqual(state.events, [
            "new", "spawned", "listening", "ready", "forwarded",
            "shutdown_requested", "exited",
        ])
        with self.assertRaisesRegex(ChurnError, "invalid_cycle_transition"):
            state.transition("ready")

    def test_strict_semantics_ignore_object_order_but_preserve_json_types_and_arrays(self):
        self.assertTrue(strict_json_equal({"a": 1, "b": [True, 2]},
                                          {"b": [True, 2], "a": 1}))
        self.assertFalse(strict_json_equal({"value": 1}, {"value": 1.0}))
        self.assertFalse(strict_json_equal({"value": True}, {"value": 1}))
        self.assertFalse(strict_json_equal({"items": [1, 2]}, {"items": [2, 1]}))

    def test_percentiles_readiness_gate_and_resource_slopes(self):
        python_cycles = [
            {"spawn_to_ready_ms": value} for value in (100.0, 110.0, 120.0, 130.0)
        ]
        rust_cycles = [
            {"spawn_to_ready_ms": value} for value in (150.0, 160.0, 170.0, 180.0)
        ]
        self.assertEqual(readiness_summary(python_cycles)["p50_ms"], 115.0)
        self.assertTrue(readiness_gate(python_cycles, rust_cycles, allowance_ms=100))
        self.assertFalse(readiness_gate(python_cycles, rust_cycles, allowance_ms=20))
        self.assertAlmostEqual(linear_slope([10, 12, 14]), 2)
        self.assertIsNone(linear_slope([10, None, 14]))

        cycles = [
            {"resources": {"rss_bytes": rss, "threads": 4, "descriptors": 8},
             "artifact_bytes": artifacts}
            for rss, artifacts in ((100, 20), (200, 30), (220, 42), (260, 48))
        ]
        self.assertEqual(post_warmup_slopes(cycles, 1), {
            "rss_bytes": 30.0,
            "threads": 0.0,
            "descriptors": 0.0,
            "artifact_bytes": 9.0,
        })

    def test_shutdown_distribution_and_resource_slope_gate_are_explicit(self):
        exits = [
            {"shutdown_elapsed_ms": value, "exit_code": 0,
             "process_tree_residue": []}
            for value in (10.0, 20.0, 30.0)
        ]
        self.assertEqual(shutdown_summary(exits), {
            "count": 3,
            "p50_ms": 20.0,
            "p95_ms": 29.0,
            "p99_ms": 29.8,
            "all_exit_zero": True,
            "no_process_residue": True,
        })
        def make_cycles(values):
            return [
                {"resources": {"rss_bytes": value, "threads": 4,
                               "descriptors": 8,
                               "unused": 0}, "artifact_bytes": value}
                for value in values
            ]
        gates = resource_slope_gate(
            make_cycles([100, 200, 300]), make_cycles([100, 200, 300]), 1
        )
        self.assertTrue(all(item["passed"] for item in gates.values()))
        failed = resource_slope_gate(
            make_cycles([100, 100, 100]), make_cycles([100, 5_000_000, 10_000_000]), 1
        )
        self.assertFalse(failed["rss_bytes"]["passed"])
        self.assertEqual(failed["rss_bytes"]["absolute_allowance_per_cycle"], 4 * 1024 * 1024)

    def test_resource_gate_rejects_absolute_growth_even_if_python_grows_faster(self):
        def cycles(rss_values):
            return [
                {"resources": {"rss_bytes": rss, "threads": 4, "descriptors": 8},
                 "artifact_bytes": 0}
                for rss in rss_values
            ]
        result = resource_slope_gate(
            cycles([0, 20_000_000, 40_000_000]),
            cycles([0, 5_000_000, 10_000_000]),
            0,
        )["rss_bytes"]
        self.assertFalse(result["absolute_pass"])
        self.assertTrue(result["comparative_pass"])
        self.assertFalse(result["passed"])

    def test_bootstrap_path_and_readiness_gate_require_both_runtime_samples(self):
        url = "http://127.0.0.1:43210/?bootstrap=opaque-token"
        self.assertEqual(_startup_path("python", url), (43210, "/"))
        self.assertEqual(_startup_path("rust", url), (43210, "/?bootstrap=opaque-token"))
        self.assertFalse(readiness_gate([], [{"spawn_to_ready_ms": 1.0}]))


if __name__ == "__main__":
    unittest.main()
