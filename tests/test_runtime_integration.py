import json
import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.codex_runtime import (
    EMP_LOADED,
    RELOAD_REQUIRED,
    STOPPED_WAITING_FOR_START,
    STOPPING,
    RuntimeRecoveryStore,
    RuntimeSyncResult,
)
from easy_multi_provider.config import normalize, save
from easy_multi_provider.integration import IntegrationManager
from easy_multi_provider.server import AppState, _integration_summary


class RecordingRuntimeController:
    def __init__(self, result=None):
        self.result = result or RuntimeSyncResult(
            STOPPED_WAITING_FOR_START, "emp", False, "test runtime stopped"
        )
        self.calls = []

    def reload(self, expected_models, target, *, confirm_reload):
        self.calls.append((tuple(expected_models), target, confirm_reload))
        return RuntimeSyncResult(
            self.result.state,
            target,
            self.result.verified,
            self.result.detail,
            self.result.observed_models,
        )


class RuntimeIntegrationTests(unittest.TestCase):
    def make_state(self, runtime=None):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        config_path = root / "emp.json"
        save(
            normalize(
                {
                    "native_catalog_path": str(root / "native.json"),
                    "providers": [
                        {
                            "id": "external",
                            "base_url": "https://example.invalid/v1",
                            "protocol": "responses",
                        }
                    ],
                    "models": [
                        {
                            "id": "external/model-a",
                            "provider": "external",
                            "upstream_id": "model-a",
                            "enabled": True,
                        }
                    ],
                }
            ),
            config_path,
        )
        state_dir = root / "codex" / "easy-multi-provider" / "integration"
        manager = IntegrationManager(
            root / "codex" / "config.toml",
            state_dir / "lease.json",
            instance_id="runtime-integration-test",
            lock_path=state_dir / "lease.lock",
        )
        controller = runtime or RecordingRuntimeController()
        state = AppState(
            config_path,
            integration_manager=manager,
            catalog_path=root / "catalog.json",
            runtime_controller=controller,
        )
        return root, state, manager, controller

    @staticmethod
    def activate(state, manager):
        state.mark_service_ready()
        manager.enable(
            "http://127.0.0.1:43123/v1",
            str(state.integration_catalog_path),
            service_ready=True,
        )

    def test_catalog_refresh_only_marks_reload_required(self):
        _root, state, manager, runtime = self.make_state()
        self.activate(state, manager)

        state.refresh_catalog()

        self.assertEqual(runtime.calls, [])
        summary = _integration_summary(state)
        self.assertEqual(summary["runtime"]["state"], RELOAD_REQUIRED)
        self.assertEqual(summary["runtime"]["target"], "emp")
        persisted = json.loads(
            manager.lease_path.with_name("runtime.json").read_text(encoding="utf-8")
        )
        self.assertEqual(persisted["state"], RELOAD_REQUIRED)

    def test_reload_action_is_confirmed_and_records_live_result(self):
        runtime = RecordingRuntimeController(
            RuntimeSyncResult(EMP_LOADED, "emp", True, "complete catalog")
        )
        _root, state, manager, _runtime = self.make_state(runtime)
        self.activate(state, manager)
        state.refresh_catalog()

        result = state.sync_integration_runtime(True)

        self.assertEqual(result.state, EMP_LOADED)
        self.assertEqual(runtime.calls, [(('external/model-a',), "emp", True)])
        summary = _integration_summary(state)
        self.assertEqual(summary["runtime"]["state"], EMP_LOADED)
        self.assertEqual(summary["runtime"]["confidence"], "live")

    def test_restore_uses_expected_slugs_from_recovery_record(self):
        runtime = RecordingRuntimeController()
        root, state, manager, _runtime = self.make_state(runtime)
        self.activate(state, manager)
        RuntimeRecoveryStore(manager.lease_path.with_name("runtime.json")).save(
            STOPPED_WAITING_FOR_START,
            "emp",
            "applied",
            ("external/recorded-model",),
            False,
            "waiting",
        )
        reopened = AppState(
            state.path,
            integration_manager=manager,
            catalog_path=root / "catalog.json",
            runtime_controller=runtime,
        )

        reopened.restore_integration(confirm_reload=True)

        self.assertEqual(
            runtime.calls,
            [(('external/recorded-model',), "native", True)],
        )

    def test_interrupted_stopping_phase_is_stale_reload_required_after_restart(self):
        root, state, manager, runtime = self.make_state()
        RuntimeRecoveryStore(manager.lease_path.with_name("runtime.json")).save(
            STOPPING,
            "emp",
            "applied",
            ("external/model-a",),
            False,
            "in progress",
        )

        reopened = AppState(
            state.path,
            integration_manager=manager,
            catalog_path=root / "catalog.json",
            runtime_controller=runtime,
        )
        summary = _integration_summary(reopened)

        self.assertEqual(summary["runtime"]["state"], RELOAD_REQUIRED)
        self.assertEqual(summary["runtime"]["confidence"], "stale")
        self.assertFalse(summary["runtime"]["verified"])
        self.assertEqual(summary["runtime"]["last_known"]["state"], STOPPING)


if __name__ == "__main__":
    unittest.main()
