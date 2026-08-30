import contextlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider import __version__, main as cli
from easy_multi_provider.config import ConfigError
from easy_multi_provider.integration import (
    IntegrationManager,
    IntegrationResult,
    IntegrationStatus,
)


class IntegrationCliTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.codex_home = self.root / "codex-home"
        self.codex_home.mkdir()
        self.state_dir = self.root / "emp-state"
        self.environment = patch.dict(
            os.environ,
            {"CODEX_HOME": str(self.codex_home)},
            clear=False,
        )
        self.environment.start()
        self.addCleanup(self.environment.stop)

    @property
    def config_path(self):
        return self.codex_home / "config.toml"

    @property
    def lease_path(self):
        return self.state_dir / "lease.json"

    @property
    def default_state_dir(self):
        return self.codex_home / "easy-multi-provider" / "integration"

    @property
    def default_lease_path(self):
        return self.default_state_dir / "lease.json"

    def manager(self):
        return IntegrationManager(
            self.config_path,
            self.lease_path,
            instance_id="cli-test-instance",
            lock_path=self.state_dir / "lease.lock",
        )

    def default_manager(self):
        return IntegrationManager(
            self.config_path,
            self.default_lease_path,
            instance_id="cli-default-test-instance",
            lock_path=self.default_state_dir / "lease.lock",
        )

    def enable(self):
        return self.manager().enable(
            "http://127.0.0.1:43123/v1",
            "catalog-value",
            service_ready=True,
        )

    def invoke(self, *arguments):
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = cli.main(list(arguments))
        return code, stdout.getvalue(), stderr.getvalue()

    def invoke_from(self, cwd, *arguments):
        previous_cwd = Path.cwd()
        try:
            os.chdir(cwd)
            return self.invoke(*arguments)
        finally:
            os.chdir(previous_cwd)

    def read_lease(self):
        return json.loads(self.lease_path.read_text(encoding="utf-8"))

    def set_lease_status(self, status):
        payload = self.read_lease()
        payload["status"] = status
        self.lease_path.write_text(json.dumps(payload), encoding="utf-8")

    def doctor(self, *extra):
        return self.invoke("doctor", "--state-dir", str(self.state_dir), *extra)

    def restore(self, *extra):
        return self.invoke("restore", "--state-dir", str(self.state_dir), *extra)

    def test_help_lists_commands_and_path_contract(self):
        root_output = io.StringIO()
        with patch("sys.stdout", root_output):
            with self.assertRaises(SystemExit) as raised:
                cli.main(["--help"])
        self.assertEqual(raised.exception.code, 0)

        self.assertIn("serve", root_output.getvalue())
        self.assertIn("doctor", root_output.getvalue())
        self.assertIn("restore", root_output.getvalue())

        output = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with patch("sys.stdout", output):
                cli.main(["doctor", "--help"])
        self.assertEqual(raised.exception.code, 0)
        help_text = output.getvalue()
        normalized_help = " ".join(help_text.split())
        self.assertIn("doctor", help_text)
        self.assertIn("--state-dir", help_text)
        self.assertIn("CODEX_HOME/easy- multi-provider/integration", normalized_help)
        self.assertIn("relative explicit values resolve from cwd", normalized_help)
        self.assertNotIn("enable", help_text)

    def test_version_reports_runtime_package_version(self):
        output = io.StringIO()
        with self.assertRaises(SystemExit) as raised:
            with patch("sys.stdout", output):
                cli.main(["--version"])
        self.assertEqual(raised.exception.code, 0)
        self.assertEqual(
            output.getvalue().strip(), "EasyMultiProvider %s" % __version__
        )

    def test_serve_subcommand_keeps_existing_service_call_shape(self):
        with patch("easy_multi_provider.server.serve") as serve:
            code, stdout, stderr = self.invoke(
                "serve",
                "--config",
                str(self.root / "emp.json"),
                "--host",
                "127.0.0.1",
                "--port",
                "43123",
            )
        self.assertEqual(code, 0)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "")
        serve.assert_called_once_with(
            self.root / "emp.json",
            "127.0.0.1",
            43123,
            open_browser=False,
        )

    def test_packaged_no_argument_launch_uses_visible_desktop_service(self):
        desktop_config = self.root / "desktop" / "config.json"
        with (
            patch.object(cli.sys, "frozen", True, create=True),
            patch.object(cli, "resolve_desktop_config_path", return_value=desktop_config),
            patch("easy_multi_provider.server.serve") as serve,
        ):
            code, stdout, stderr = self.invoke()

        self.assertEqual(code, 0)
        self.assertEqual(stdout, "")
        self.assertEqual(stderr, "")
        serve.assert_called_once_with(
            desktop_config,
            None,
            None,
            open_browser=True,
        )

    def test_desktop_config_path_follows_platform_user_directories(self):
        home = self.root / "user"
        self.assertEqual(
            cli.resolve_desktop_config_path(
                environ={"LOCALAPPDATA": str(self.root / "local")},
                user_home=home,
                platform_name="win32",
            ),
            self.root / "local" / "EasyMultiProvider" / "config.json",
        )
        self.assertEqual(
            cli.resolve_desktop_config_path(
                environ={},
                user_home=home,
                platform_name="darwin",
            ),
            home
            / "Library"
            / "Application Support"
            / "EasyMultiProvider"
            / "config.json",
        )
        self.assertEqual(
            cli.resolve_desktop_config_path(
                environ={"XDG_CONFIG_HOME": str(self.root / "xdg")},
                user_home=home,
                platform_name="linux",
            ),
            self.root / "xdg" / "easy-multi-provider" / "config.json",
        )

    def test_serve_reports_existing_owner_without_traceback(self):
        with patch(
            "easy_multi_provider.server.serve",
            side_effect=ConfigError(
                "another EMP service owns this configuration"
            ),
        ):
            code, stdout, stderr = self.invoke(
                "serve", "--config", str(self.root / "emp.json")
            )

        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertEqual(
            stderr.strip(), "another EMP service owns this configuration"
        )
        self.assertNotIn("Traceback", stderr)

    def test_explicit_subcommand_is_required_and_legacy_options_never_serve(self):
        with patch("easy_multi_provider.server.serve") as serve:
            code, stdout, stderr = self.invoke()
            self.assertEqual(code, 2)
            self.assertEqual(stdout, "")
            self.assertIn("usage:", stderr)

            with self.assertRaises(SystemExit) as raised:
                self.invoke("--config", str(self.root / "emp.json"))
            self.assertEqual(raised.exception.code, 2)

        serve.assert_not_called()

    def test_path_resolution_uses_active_home_and_explicit_local_state(self):
        first_default = cli.resolve_integration_paths(
            environ={"CODEX_HOME": str(self.codex_home)},
            cwd=self.root / "one",
        )
        second_default = cli.resolve_integration_paths(
            environ={"CODEX_HOME": str(self.codex_home)},
            cwd=self.root / "two",
        )
        self.assertEqual(first_default.state_dir, self.default_state_dir)
        self.assertEqual(first_default.lease_path, self.default_lease_path)
        self.assertEqual(first_default.lock_path, self.default_state_dir / "lease.lock")
        self.assertEqual(first_default, second_default)

        paths = cli.resolve_integration_paths(
            environ={"CODEX_HOME": str(self.codex_home)},
            state_dir=Path("emp-state"),
            cwd=self.root,
        )
        self.assertEqual(paths.codex_home, self.codex_home.resolve())
        self.assertEqual(paths.config_path, self.codex_home / "config.toml")
        self.assertEqual(paths.state_dir, (self.root / "emp-state").resolve())
        self.assertEqual(paths.lease_path, paths.state_dir / "lease.json")
        self.assertEqual(paths.lock_path, paths.state_dir / "lease.lock")

        same_state = self.root / "shared-state"
        first = cli.resolve_integration_paths(
            environ={"CODEX_HOME": str(self.codex_home)},
            state_dir=same_state,
            cwd=self.root / "one",
        )
        second = cli.resolve_integration_paths(
            environ={"CODEX_HOME": str(self.codex_home)},
            state_dir=same_state,
            cwd=self.root / "two",
        )
        self.assertEqual(first.lease_path, second.lease_path)

    def test_default_doctor_and_restore_share_lease_across_invocation_cwds(self):
        first_cwd = self.root / "first-cwd"
        second_cwd = self.root / "second-cwd"
        first_cwd.mkdir()
        second_cwd.mkdir()
        self.default_manager().enable(
            "http://127.0.0.1:43123/v1",
            "catalog-value",
            service_ready=True,
        )

        with patch("pathlib.Path.home", side_effect=AssertionError("home accessed")):
            code, stdout, stderr = self.invoke_from(first_cwd, "doctor", "--json")
            doctor_payload = json.loads(stdout)
            self.assertEqual(code, 0)
            self.assertEqual(doctor_payload["state"], "active")
            self.assertEqual(stderr, "")

            code, stdout, stderr = self.invoke_from(second_cwd, "restore", "--json")
            restore_payload = json.loads(stdout)
            self.assertEqual(code, 0)
            self.assertEqual(restore_payload["configuration"]["action"], "restored")
            self.assertEqual(restore_payload["configuration"]["state"], "restored")
            self.assertEqual(stderr, "")

        lease = json.loads(self.default_lease_path.read_text(encoding="utf-8"))
        self.assertEqual(lease["status"], "restored")
        self.assertFalse((first_cwd / "state").exists())
        self.assertFalse((second_cwd / "state").exists())

    def test_empty_home_falls_back_to_user_codex_directory_without_access(self):
        fallback_home = self.root / "user-home"
        with patch("pathlib.Path.home", side_effect=AssertionError("home accessed")):
            resolved = cli.resolve_codex_home(
                environ={"CODEX_HOME": str(self.codex_home)},
                user_home=fallback_home,
            )
        self.assertEqual(resolved, self.codex_home.resolve())

        resolved_fallback = cli.resolve_codex_home(
            environ={"CODEX_HOME": ""},
            user_home=fallback_home,
        )
        self.assertEqual(resolved_fallback, (fallback_home / ".codex").resolve())
        self.assertFalse((fallback_home / ".codex").exists())

    def test_native_doctor_is_human_readable_and_zero(self):
        with patch("pathlib.Path.home", side_effect=AssertionError("home accessed")):
            code, stdout, stderr = self.doctor()
        self.assertEqual(code, 0)
        self.assertIn("state: native", stdout)
        self.assertIn("relation: unleased", stdout)
        self.assertIn("service health: not_checked", stdout)
        self.assertIn("runtime state: not_checked", stdout)
        self.assertIn("runtime confidence: offline", stdout)
        self.assertIn("next action: none", stdout)
        self.assertEqual(stderr, "")
        self.assertFalse(self.config_path.exists())

    def test_json_doctor_reports_all_healthy_and_recoverable_states(self):
        self.enable()
        expected_codes = {"prepared": 1, "active": 0, "restoring": 1}
        expected_actions = {
            "prepared": "run restore",
            "active": "confirm service health or run restore",
            "restoring": "run restore",
        }
        expected_schema = {
            "state",
            "relation",
            "config_exists",
            "lease_status",
            "conflicts",
            "service_health",
            "runtime",
            "next_action",
        }
        for phase in ("prepared", "active", "restoring"):
            self.set_lease_status(phase)
            code, stdout, stderr = self.doctor("--json")
            payload = json.loads(stdout)
            self.assertEqual(code, expected_codes[phase])
            self.assertEqual(set(payload), expected_schema)
            self.assertEqual(payload["state"], phase)
            self.assertEqual(payload["relation"], "applied")
            self.assertEqual(payload["lease_status"], phase)
            self.assertEqual(payload["service_health"], "not_checked")
            self.assertEqual(payload["runtime"]["state"], "not_checked")
            self.assertEqual(payload["runtime"]["confidence"], "offline")
            self.assertEqual(payload["next_action"], expected_actions[phase])
            self.assertEqual(stderr, "")

        self.manager().restore()
        code, stdout, stderr = self.doctor("--json")
        payload = json.loads(stdout)
        self.assertEqual(code, 0)
        self.assertEqual(set(payload), expected_schema)
        self.assertEqual(payload["state"], "restored")
        self.assertEqual(payload["relation"], "original")
        self.assertEqual(payload["lease_status"], "restored")
        self.assertEqual(payload["service_health"], "not_checked")
        self.assertEqual(payload["runtime"]["state"], "not_checked")
        self.assertEqual(payload["runtime"]["confidence"], "offline")
        self.assertEqual(payload["next_action"], "none")
        self.assertEqual(stderr, "")

    def test_human_doctor_reports_health_next_action_and_exit_for_each_lease_state(self):
        self.enable()
        expected = {
            "prepared": (1, "run restore"),
            "active": (0, "confirm service health or run restore"),
            "restoring": (1, "run restore"),
        }
        for phase, (expected_code, expected_action) in expected.items():
            with self.subTest(phase=phase):
                self.set_lease_status(phase)
                code, stdout, stderr = self.doctor()
                self.assertEqual(code, expected_code)
                self.assertIn("state: %s" % phase, stdout)
                self.assertIn("service health: not_checked", stdout)
                self.assertIn("next action: %s" % expected_action, stdout)
                self.assertEqual(stderr, "")

        self.manager().restore()
        code, stdout, stderr = self.doctor()
        self.assertEqual(code, 0)
        self.assertIn("state: restored", stdout)
        self.assertIn("service health: not_checked", stdout)
        self.assertIn("next action: none", stdout)
        self.assertEqual(stderr, "")

    def test_restored_mismatch_and_conflict_have_nonzero_doctor_exit(self):
        self.enable()
        self.manager().restore()
        self.config_path.write_text(
            'openai_base_url = "user-value"\n'
            'model_catalog_json = "catalog-value"\n',
            encoding="utf-8",
        )

        code, stdout, stderr = self.doctor()
        self.assertEqual(code, 1)
        self.assertIn("state: conflict", stdout)
        self.assertIn("service health: not_checked", stdout)
        self.assertIn(
            "next action: manually inspect; EMP will not overwrite user changes",
            stdout,
        )
        self.assertEqual(stderr, "")

        code, stdout, stderr = self.doctor("--json")

        payload = json.loads(stdout)
        self.assertEqual(code, 1)
        self.assertEqual(payload["state"], "conflict")
        self.assertIn("lease_state_mismatch", payload["conflicts"])
        self.assertEqual(
            set(payload),
            {
                "state",
                "relation",
                "config_exists",
                "lease_status",
                "conflicts",
                "service_health",
                "runtime",
                "next_action",
            },
        )
        self.assertEqual(payload["service_health"], "not_checked")
        self.assertEqual(payload["runtime"]["state"], "not_checked")
        self.assertEqual(payload["runtime"]["confidence"], "offline")
        self.assertEqual(
            payload["next_action"],
            "manually inspect; EMP will not overwrite user changes",
        )
        self.assertNotIn("lease", payload)
        self.assertNotIn("fields", payload)
        self.assertNotIn("config_path", payload)
        self.assertEqual(stderr, "")

    def test_restore_is_offline_human_then_json_noop(self):
        self.enable()
        code, stdout, stderr = self.restore()
        self.assertEqual(code, 0)
        self.assertIn("action: restored", stdout)
        self.assertIn("state: restored", stdout)
        self.assertIn("runtime state: reload_required", stdout)
        self.assertEqual(stderr, "")

    def test_offline_doctor_and_restore_never_invoke_codex(self):
        self.enable()
        fake_bin = self.root / "bin"
        fake_bin.mkdir()
        marker = self.root / "codex-was-called"
        codex = fake_bin / "codex"
        codex.write_text(
            "#!/bin/sh\nprintf called > \"$EMP_OFFLINE_CODEX_MARKER\"\nexit 99\n",
            encoding="utf-8",
        )
        codex.chmod(0o700)
        with patch.dict(
            os.environ,
            {
                "PATH": str(fake_bin),
                "EMP_OFFLINE_CODEX_MARKER": str(marker),
            },
        ):
            self.assertEqual(self.doctor("--json")[0], 0)
            self.assertEqual(self.restore("--json")[0], 0)
        self.assertFalse(marker.exists())

        code, stdout, stderr = self.restore("--json")
        payload = json.loads(stdout)
        self.assertEqual(code, 0)
        self.assertEqual(payload["configuration"]["action"], "noop")
        self.assertEqual(payload["configuration"]["state"], "restored")
        self.assertEqual(payload["configuration"]["state"], "restored")
        self.assertEqual(payload["runtime"]["state"], "reload_required")
        self.assertFalse(payload["runtime"]["verified"])
        self.assertEqual(payload["runtime"]["confidence"], "offline")
        self.assertEqual(payload["next_action"], "none")
        self.assertEqual(stderr, "")

    def test_json_summaries_do_not_call_model_to_dict(self):
        self.enable()
        with patch.object(
            IntegrationStatus,
            "to_dict",
            side_effect=AssertionError("status.to_dict called"),
        ):
            code, stdout, stderr = self.doctor("--json")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(stdout)["state"], "active")
        self.assertEqual(stderr, "")

        with patch.object(
            IntegrationResult,
            "to_dict",
            side_effect=AssertionError("result.to_dict called"),
        ):
            code, stdout, stderr = self.restore("--json")
        self.assertEqual(code, 0)
        self.assertEqual(
            json.loads(stdout)["configuration"]["state"], "restored"
        )
        self.assertEqual(stderr, "")

    def test_active_and_restore_json_hide_config_and_lease_markers(self):
        config_marker = "private-config-marker-2f713c"
        lease_marker = "private-lease-marker-9a84de"
        self.config_path.write_text(
            'native_setting = "%s"\n' % config_marker,
            encoding="utf-8",
        )
        self.enable()
        lease = self.read_lease()
        lease["lease_id"] = lease_marker
        lease["instance_id"] = lease_marker + "-instance"
        lease["created_at"] = lease_marker + "-created"
        lease["updated_at"] = lease_marker + "-updated"
        self.lease_path.write_text(json.dumps(lease), encoding="utf-8")

        code, stdout, stderr = self.doctor("--json")
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(stdout)["state"], "active")
        self.assertNotIn(config_marker, stdout + stderr)
        self.assertNotIn(lease_marker, stdout + stderr)

        code, stdout, stderr = self.restore("--json")
        self.assertEqual(code, 0)
        payload = json.loads(stdout)
        self.assertEqual(payload["configuration"]["action"], "restored")
        self.assertEqual(payload["configuration"]["state"], "restored")
        self.assertNotIn(config_marker, stdout + stderr)
        self.assertNotIn(lease_marker, stdout + stderr)

    def test_restore_conflict_is_nonzero_and_does_not_overwrite_user_value(self):
        self.enable()
        marker = "lease-marker"
        lease = self.read_lease()
        lease["fields"]["openai_base_url"]["applied"]["value"] = marker
        self.lease_path.write_text(json.dumps(lease), encoding="utf-8")
        self.config_path.write_text(
            'openai_base_url = "user-value"\n'
            'model_catalog_json = "catalog-value"\n',
            encoding="utf-8",
        )

        code, stdout, stderr = self.restore("--json")

        payload = json.loads(stdout)
        self.assertEqual(code, 1)
        self.assertEqual(payload["configuration"]["state"], "conflict")
        self.assertEqual(
            payload["next_action"],
            "manually inspect; EMP will not overwrite user changes",
        )
        self.assertNotIn("lease", payload["configuration"])
        self.assertNotIn("fields", payload["configuration"])
        self.assertNotIn("config_path", payload["configuration"])
        self.assertNotIn(marker, stdout)
        self.assertNotIn(marker, stderr)
        self.assertIn("user-value", self.config_path.read_text(encoding="utf-8"))
        self.assertEqual(stderr, "")

    def test_malformed_lease_symlink_and_read_errors_are_nonzero_without_leak(self):
        self.state_dir.mkdir(parents=True)
        marker = "opaque-invalid-marker"
        self.lease_path.write_text(marker, encoding="utf-8")
        code, stdout, stderr = self.doctor()
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn("lease", stderr)
        self.assertNotIn(marker, stderr)

        self.lease_path.unlink()
        target = self.root / "native-config.toml"
        target.write_text('title = "native"\n', encoding="utf-8")
        self.config_path.symlink_to(target)
        code, stdout, stderr = self.doctor()
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn("symlink", stderr)
        self.assertEqual(target.read_text(encoding="utf-8"), 'title = "native"\n')


if __name__ == "__main__":
    unittest.main()
