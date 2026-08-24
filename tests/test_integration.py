import json
import multiprocessing
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import easy_multi_provider.integration as integration
from easy_multi_provider.integration import (
    IntegrationManager,
    LeaseError,
    LockTimeout,
    ServiceNotReady,
    SymlinkConfigError,
    atomic_write_text,
)


def _hold_lock(lock_path, ready, release):
    with integration._FileLock(Path(lock_path), timeout=1.0):
        ready.set()
        release.wait(2.0)


def _hold_operation_lock(config_path, lease_path, ready, release):
    manager = IntegrationManager(
        Path(config_path),
        Path(lease_path),
        instance_id="lock-holder",
        lock_timeout=1.0,
    )
    with manager.operation_lock():
        ready.set()
        release.wait(2.0)


class IntegrationTests(unittest.TestCase):
    SERVICE_URL = "http://127.0.0.1:43123/v1"
    CATALOG = "catalog-value"

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.codex_home = self.root / "codex-home"
        self.codex_home.mkdir()
        self.config_path = self.codex_home / "config.toml"
        self.lease_path = self.codex_home / ".integration" / "lease.json"
        self.environment = patch.dict(
            os.environ, {"CODEX_HOME": str(self.codex_home)}, clear=False
        )
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def manager(self, instance_id="instance-one", lock_timeout=5.0):
        return IntegrationManager(
            self.config_path,
            self.lease_path,
            instance_id=instance_id,
            lock_timeout=lock_timeout,
        )

    def write_config(self, value, mode=0o600):
        self.config_path.write_text(value, encoding="utf-8")
        os.chmod(str(self.config_path), mode)

    def read_lease(self):
        return json.loads(self.lease_path.read_text(encoding="utf-8"))

    def set_lease_status(self, status):
        payload = self.read_lease()
        payload["status"] = status
        self.lease_path.write_text(json.dumps(payload), encoding="utf-8")

    def enable(self, manager=None):
        return (manager or self.manager()).enable(
            self.SERVICE_URL,
            self.CATALOG,
            service_ready=lambda: True,
        )

    def test_missing_config_is_read_without_creating_files(self):
        result = self.manager().status()
        self.assertEqual(result.state, "native")
        self.assertEqual(result.relation, "unleased")
        self.assertFalse(result.config_exists)
        self.assertFalse(self.config_path.exists())
        self.assertFalse(self.lease_path.exists())

    def test_operation_lock_serializes_separate_processes(self):
        ready = multiprocessing.Event()
        release = multiprocessing.Event()
        process = multiprocessing.Process(
            target=_hold_operation_lock,
            args=(str(self.config_path), str(self.lease_path), ready, release),
        )
        process.start()
        self.addCleanup(lambda: process.is_alive() and process.terminate())
        self.assertTrue(ready.wait(1.0))
        contender = self.manager("contender", lock_timeout=0.05)
        with self.assertRaises(LockTimeout):
            with contender.operation_lock():
                self.fail("contender acquired an already-held operation lock")
        release.set()
        process.join(1.0)
        self.assertEqual(process.exitcode, 0)

    def test_round_trip_preserves_comments_nested_config_and_other_fields(self):
        original = (
            "# keep this header\n"
            "title = \"keep\"  # keep this comment\n"
            "openai_base_url   = \"native\"  # keep this inline comment\n"
            "multiline = '''line one\nline two'''\n"
            "\n"
            "[nested]\n"
            "openai_base_url = \"nested-value\"\n"
            "enabled = true\n"
            "\n"
            "[[items]]\n"
            "name = \"unchanged\"\n"
        )
        self.write_config(original)
        self.enable()
        updated = self.config_path.read_text(encoding="utf-8")
        self.assertIn("# keep this header\n", updated)
        self.assertIn('title = "keep"  # keep this comment\n', updated)
        self.assertIn(
            'openai_base_url   = "http://127.0.0.1:43123/v1"  # keep this inline comment\n',
            updated,
        )
        self.assertIn("multiline = '''line one\nline two'''\n", updated)
        self.assertIn('[nested]\nopenai_base_url = "nested-value"\n', updated)
        self.assertIn('[[items]]\nname = "unchanged"\n', updated)
        self.assertIn('model_catalog_json = "catalog-value"', updated)

    def test_enable_requires_service_confirmation_and_commits_active(self):
        manager = self.manager()
        with self.assertRaises(ServiceNotReady):
            manager.enable(self.SERVICE_URL, self.CATALOG)
        self.assertFalse(self.config_path.exists())
        self.assertFalse(self.lease_path.exists())

        result = self.enable(manager)
        self.assertEqual(result.action, "enabled")
        self.assertEqual(result.state, "active")
        self.assertEqual(self.read_lease()["status"], "active")
        self.assertEqual(
            result.fields["openai_base_url"].value,
            self.SERVICE_URL,
        )

    def test_clean_restore_and_missing_original_fields(self):
        original = (
            "# native\n"
            "openai_base_url = \"native\"\n"
            "model_catalog_json = \"native-catalog\"\n"
            "[other]\n"
            "value = 3\n"
        )
        self.write_config(original)
        manager = self.manager()
        self.enable(manager)
        result = manager.restore()
        self.assertEqual(result.action, "restored")
        self.assertEqual(result.state, "restored")
        restored = self.config_path.read_text(encoding="utf-8")
        self.assertIn('# native\nopenai_base_url = "native"\n', restored)
        self.assertIn('model_catalog_json = "native-catalog"\n[other]', restored)
        self.assertEqual(manager.status().state, "restored")

        self.write_config("# keep\n[other]\nvalue = true\n")
        self.enable(manager)
        manager.restore()
        restored = self.config_path.read_text(encoding="utf-8")
        self.assertNotIn("openai_base_url =", restored)
        self.assertNotIn("model_catalog_json =", restored)
        self.assertIn("# keep\n", restored)
        self.assertIn("[other]\nvalue = true\n", restored)

    def test_same_instance_status_and_stale_re_adopt(self):
        first = self.manager("first-instance")
        self.enable(first)
        old_lease_id = self.read_lease()["lease_id"]
        second = self.manager("second-instance")
        status = second.status()
        self.assertEqual(status.state, "active")
        self.assertEqual(status.relation, "applied")
        self.assertFalse(status.same_instance)

        result = second.recover(re_adopt=True, service_ready=lambda: True)
        self.assertEqual(result.action, "re_adopted")
        self.assertEqual(result.state, "active")
        self.assertNotEqual(result.lease.lease_id, old_lease_id)
        self.assertTrue(second.status().same_instance)

    def test_re_adopt_without_ready_does_not_change_lease(self):
        first = self.manager("first-instance")
        self.enable(first)
        before = self.lease_path.read_bytes()
        second = self.manager("second-instance")
        with self.assertRaises(ServiceNotReady):
            second.recover(re_adopt=True, service_ready=False)
        self.assertEqual(self.lease_path.read_bytes(), before)

    def test_offline_restore_is_idempotent_and_never_needs_service(self):
        manager = self.manager()
        self.enable(manager)
        first = manager.restore()
        second = manager.restore()
        self.assertEqual(first.action, "restored")
        self.assertEqual(second.action, "noop")
        self.assertEqual(second.state, "restored")

    def test_missing_config_is_removed_when_restore_has_no_user_content(self):
        manager = self.manager()
        self.assertFalse(self.config_path.exists())
        self.enable(manager)
        self.assertFalse(self.read_lease()["config_existed"])
        self.assertTrue(self.config_path.exists())

        result = manager.restore()

        self.assertEqual(result.state, "restored")
        self.assertFalse(self.config_path.exists())
        self.assertFalse(manager.status().config_exists)
        self.assertEqual(manager.restore().action, "noop")

    def test_missing_config_restore_keeps_user_content(self):
        manager = self.manager()
        self.enable(manager)
        self.config_path.write_text(
            'openai_base_url = "http://127.0.0.1:43123/v1"\n'
            'model_catalog_json = "catalog-value"\n'
            "\n"
            "# user note\n"
            "[user_settings]\n"
            "enabled = true\n",
            encoding="utf-8",
        )

        result = manager.restore()

        restored = self.config_path.read_text(encoding="utf-8")
        self.assertEqual(result.state, "restored")
        self.assertTrue(self.config_path.exists())
        self.assertNotIn("openai_base_url", restored)
        self.assertNotIn("model_catalog_json", restored)
        self.assertIn("# user note\n", restored)
        self.assertIn("[user_settings]\nenabled = true\n", restored)

    def test_deleted_config_before_restored_commit_converges(self):
        manager = self.manager()
        self.enable(manager)
        real_write_lease = manager._write_lease

        def fail_restored_commit(lease):
            if lease.status == "restored":
                raise RuntimeError("crash after config deletion")
            return real_write_lease(lease)

        with patch.object(manager, "_write_lease", side_effect=fail_restored_commit):
            with self.assertRaises(RuntimeError):
                manager.restore()
        self.assertFalse(self.config_path.exists())
        self.assertEqual(self.read_lease()["status"], "restoring")

        result = manager.restore()

        self.assertEqual(result.state, "restored")
        self.assertFalse(self.config_path.exists())
        self.assertEqual(self.read_lease()["status"], "restored")

    def test_status_distinguishes_recoverable_phases_and_restored_mismatch(self):
        manager = self.manager()
        self.enable(manager)
        configurations = {
            "original": "",
            "applied": (
                'openai_base_url = "http://127.0.0.1:43123/v1"\n'
                'model_catalog_json = "catalog-value"\n'
            ),
            "mixed": 'model_catalog_json = "catalog-value"\n',
            "other": (
                'openai_base_url = "user-value"\n'
                'model_catalog_json = "catalog-value"\n'
            ),
        }

        for phase in ("prepared", "active", "restoring"):
            for relation, content in configurations.items():
                self.write_config(content)
                self.set_lease_status(phase)
                observed = manager.status()
                self.assertEqual(observed.relation, relation)
                if relation == "other":
                    self.assertEqual(observed.state, "conflict")
                    self.assertIn("lease_state_mismatch", observed.conflicts)
                else:
                    self.assertEqual(observed.state, phase)
                    self.assertEqual(observed.conflicts, ())

        self.set_lease_status("restored")
        for relation, content in configurations.items():
            self.write_config(content)
            observed = manager.status()
            self.assertEqual(observed.relation, relation)
            if relation == "original":
                self.assertEqual(observed.state, "restored")
                self.assertEqual(observed.conflicts, ())
            else:
                self.assertEqual(observed.state, "conflict")
                self.assertIn("lease_state_mismatch", observed.conflicts)

    def test_user_change_is_conflict_and_is_never_overwritten(self):
        manager = self.manager()
        self.enable(manager)
        self.write_config(
            'openai_base_url = "user-value"\nmodel_catalog_json = "catalog-value"\n'
        )
        result = manager.restore()
        self.assertEqual(result.action, "conflict")
        self.assertEqual(result.state, "conflict")
        self.assertIn("openai_base_url", result.conflicts)
        self.assertIn('openai_base_url = "user-value"', self.config_path.read_text())
        self.assertEqual(self.read_lease()["status"], "active")

    def test_lock_serializes_operations_and_times_out_clearly(self):
        first = self.manager("first-instance", lock_timeout=0.5)
        second = self.manager("second-instance", lock_timeout=0.05)
        with first._transaction_lock():
            with self.assertRaisesRegex(LockTimeout, "timed out"):
                second.status()

    def test_lock_is_cross_process(self):
        context_name = "fork" if "fork" in multiprocessing.get_all_start_methods() else "spawn"
        context = multiprocessing.get_context(context_name)
        ready = context.Event()
        release = context.Event()
        process = context.Process(
            target=_hold_lock,
            args=(str(self.manager().lock_path), ready, release),
        )
        process.start()
        try:
            self.assertTrue(ready.wait(2.0))
            with self.assertRaises(LockTimeout):
                self.manager("contender", lock_timeout=0.05).status()
        finally:
            release.set()
            process.join(3.0)
            if process.is_alive():
                process.terminate()
                process.join(3.0)
        self.assertEqual(process.exitcode, 0)

    def test_atomic_replace_failure_keeps_old_file_and_mode_is_pre_replace(self):
        original = 'openai_base_url = "native"\n'
        self.write_config(original, mode=0o644)
        observed = []
        original_replace = integration.os.replace

        def check_mode_and_replace(source, target):
            observed.append(stat.S_IMODE(os.stat(source).st_mode))
            return original_replace(source, target)

        with patch.object(integration.os, "replace", side_effect=OSError("blocked")):
            with self.assertRaises(OSError):
                atomic_write_text(self.config_path, 'openai_base_url = "changed"\n')
        self.assertEqual(self.config_path.read_text(encoding="utf-8"), original)
        self.assertEqual(stat.S_IMODE(self.config_path.stat().st_mode), 0o644)

        with patch.object(integration.os, "replace", side_effect=check_mode_and_replace):
            atomic_write_text(self.config_path, 'openai_base_url = "changed"\n')
        self.assertEqual(observed, [0o600])
        self.assertEqual(stat.S_IMODE(self.config_path.stat().st_mode), 0o600)

    @unittest.skipUnless(os.name == "posix", "directory fsync semantics are platform-specific")
    def test_atomic_post_replace_directory_fsync_failure_is_success(self):
        calls = []
        original_fsync = integration.os.fsync

        def fail_directory_fsync(descriptor):
            calls.append(descriptor)
            if len(calls) == 2:
                raise OSError("directory fsync unavailable")
            return original_fsync(descriptor)

        with patch.object(integration.os, "fsync", side_effect=fail_directory_fsync):
            atomic_write_text(self.config_path, "value = true\n")
        self.assertGreaterEqual(len(calls), 2)
        self.assertEqual(self.config_path.read_text(encoding="utf-8"), "value = true\n")

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are unavailable")
    def test_config_symlink_is_rejected_without_touching_target(self):
        target = self.root / "native.toml"
        target.write_text('openai_base_url = "native"\n', encoding="utf-8")
        self.config_path.symlink_to(target)
        manager = self.manager()
        with self.assertRaises(SymlinkConfigError):
            manager.status()
        self.assertTrue(self.config_path.is_symlink())
        self.assertEqual(target.read_text(encoding="utf-8"), 'openai_base_url = "native"\n')
        self.assertFalse(self.lease_path.exists())

    @unittest.skipUnless(hasattr(os, "symlink"), "symlinks are unavailable")
    def test_file_lock_rejects_symlinked_parent_before_chmod_or_create(self):
        victim = self.root / "victim"
        victim.mkdir(mode=0o755)
        original_mode = stat.S_IMODE(victim.stat().st_mode)
        linked_state = self.root / "linked-state"
        linked_state.symlink_to(victim, target_is_directory=True)

        with self.assertRaises(SymlinkConfigError):
            with integration._FileLock(linked_state / "service.lock", timeout=0):
                pass

        self.assertEqual(stat.S_IMODE(victim.stat().st_mode), original_mode)
        self.assertFalse((victim / "service.lock").exists())

    def test_config_and_lease_use_strict_permissions(self):
        self.write_config('openai_base_url = "native"\n', mode=0o644)
        self.enable()
        self.assertEqual(stat.S_IMODE(self.config_path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.lease_path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.lease_path.parent.stat().st_mode), 0o700)

    def test_lease_schema_is_strict_and_has_no_request_state(self):
        self.enable()
        payload = self.read_lease()
        self.assertEqual(
            set(payload),
            {
                "schema",
                "version",
                "config_path",
                "config_existed",
                "fields",
                "lease_id",
                "instance_id",
                "pid",
                "status",
                "created_at",
                "updated_at",
            },
        )
        self.assertEqual(set(payload["fields"]), set(integration.MANAGED_FIELDS))
        raw = self.lease_path.read_text(encoding="utf-8").lower()
        for forbidden in ("prompt", "history", "tool result", "credential", "request"):
            self.assertNotIn(forbidden, raw)

        payload["status"] = "invalid"
        self.lease_path.write_text(json.dumps(payload), encoding="utf-8")
        with self.assertRaises(LeaseError):
            self.manager().status()

    def test_prepared_before_config_can_restore_or_re_adopt(self):
        manager = self.manager("prepared-instance")
        with patch.object(manager, "_write_config", side_effect=RuntimeError("crash")):
            with self.assertRaises(RuntimeError):
                self.enable(manager)
        self.assertEqual(self.read_lease()["status"], "prepared")
        self.assertEqual(manager.status().state, "prepared")
        restored = manager.restore()
        self.assertEqual(restored.state, "restored")

        with patch.object(manager, "_write_config", side_effect=RuntimeError("crash")):
            with self.assertRaises(RuntimeError):
                self.enable(manager)
        recovered = manager.recover(re_adopt=True, service_ready=lambda: True)
        self.assertEqual(recovered.action, "recovered_restored")
        self.assertEqual(recovered.state, "restored")

    def test_config_applied_but_active_commit_missing_can_re_adopt(self):
        manager = self.manager("prepared-applied")
        real_write_lease = manager._write_lease

        def fail_active_commit(lease):
            if lease.status == "active":
                raise RuntimeError("crash after config replacement")
            return real_write_lease(lease)

        with patch.object(manager, "_write_lease", side_effect=fail_active_commit):
            with self.assertRaises(RuntimeError):
                self.enable(manager)
        self.assertEqual(self.read_lease()["status"], "prepared")
        self.assertEqual(manager.status().relation, "applied")
        adopted = manager.recover(re_adopt=True, service_ready=lambda: True)
        self.assertEqual(adopted.action, "re_adopted")
        self.assertEqual(self.read_lease()["status"], "active")

    def test_restoring_before_config_replacement_is_retryable(self):
        manager = self.manager()
        self.enable(manager)
        with patch.object(manager, "_remove_config", side_effect=RuntimeError("crash")):
            with self.assertRaises(RuntimeError):
                manager.restore()
        self.assertEqual(self.read_lease()["status"], "restoring")
        self.assertEqual(manager.status().relation, "applied")
        result = manager.restore()
        self.assertEqual(result.state, "restored")
        self.assertEqual(self.read_lease()["status"], "restored")

    def test_config_original_before_restored_commit_is_not_conflict(self):
        original = 'openai_base_url = "native"\nmodel_catalog_json = "native-catalog"\n'
        self.write_config(original)
        manager = self.manager()
        self.enable(manager)
        self.write_config(original)
        self.assertEqual(manager.status().state, "active")
        self.assertEqual(manager.status().relation, "original")
        result = manager.restore()
        self.assertEqual(result.action, "restored")
        self.assertEqual(result.state, "restored")

    def test_recoverable_states_report_conflict_for_unrecognized_values(self):
        manager = self.manager()
        self.enable(manager)
        self.write_config('openai_base_url = "user"\nmodel_catalog_json = "catalog-value"\n')
        self.assertEqual(manager.status().state, "conflict")
        self.assertEqual(manager.restore().state, "conflict")

        self.lease_path.unlink()
        self.write_config('openai_base_url = "native"\nmodel_catalog_json = "native-catalog"\n')
        manager = self.manager("restoring-instance")
        self.enable(manager)
        with patch.object(manager, "_write_config", side_effect=RuntimeError("crash")):
            with self.assertRaises(RuntimeError):
                manager.restore()
        self.write_config('openai_base_url = "user"\nmodel_catalog_json = "catalog-value"\n')
        self.assertEqual(manager.restore().state, "conflict")
        self.assertEqual(self.read_lease()["status"], "restoring")

    def test_no_implicit_home_access(self):
        with patch("pathlib.Path.home", side_effect=AssertionError("implicit home access")):
            manager = self.manager("explicit-instance")
            self.enable(manager)
        self.assertEqual(manager.config_path, self.codex_home / "config.toml")
        self.assertEqual(manager.lease_path, self.codex_home / ".integration" / "lease.json")


if __name__ == "__main__":
    unittest.main()
