import json
import os
import subprocess
import sys
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from easy_multi_provider.codex_runtime import (
    CodexRuntimeController,
    EMP_LOADED,
    HostStopResult,
    NATIVE_LOADED,
    ProcessIdentity as _RuntimeProcessIdentity,
    PsutilProcessInventory as _RuntimePsutilProcessInventory,
    SubprocessRunner,
    STOP_FAILED,
    STOPPED_WAITING_FOR_START,
    TargetedCodexHostStopper as _RuntimeTargetedCodexHostStopper,
    UNSUPPORTED,
    VERIFICATION_FAILED,
)


TEST_CODEX_HOME = os.path.normcase(
    os.path.realpath(str(Path(tempfile.gettempdir()) / "emp-test-codex-home"))
)


def ProcessIdentity(*args):
    if len(args) == 6:
        args = (*args, TEST_CODEX_HOME)
    return _RuntimeProcessIdentity(*args)


def PsutilProcessInventory(*args, **kwargs):
    if not args and "target_codex_home" not in kwargs:
        kwargs["target_codex_home"] = Path(TEST_CODEX_HOME)
        kwargs.setdefault("default_codex_home", Path(TEST_CODEX_HOME))
    return _RuntimePsutilProcessInventory(*args, **kwargs)


def TargetedCodexHostStopper(*args, **kwargs):
    kwargs.setdefault("target_codex_home", Path(TEST_CODEX_HOME))
    return _RuntimeTargetedCodexHostStopper(*args, **kwargs)


_MISSING_CODEX_HOME = object()


class ControlledPsutilCodexProcess:
    """Codex-shaped psutil seam backed by one controlled temporary child."""

    def __init__(self, child, username, codex_home=_MISSING_CODEX_HOME):
        self.child = child
        self.pid = child.pid
        self._username = username
        self.codex_home = codex_home
        self.terminate_called = False

    @staticmethod
    def ppid():
        return 0

    def username(self):
        return self._username

    @staticmethod
    def create_time():
        return 1.0

    @staticmethod
    def exe():
        return "/opt/codex"

    @staticmethod
    def cmdline():
        return ["/opt/codex", "remote-control", "--json"]

    def environ(self):
        environment = {"UNRELATED": "discard-me"}
        if self.codex_home is not _MISSING_CODEX_HOME:
            environment["CODEX_HOME"] = self.codex_home
        return environment

    def terminate(self):
        self.terminate_called = True
        self.child.terminate()

    def wait(self, timeout):
        return self.child.wait(timeout=timeout)


class NoResidualHosts:
    def __init__(self):
        self.calls = 0

    def stop_stale_codex_hosts(self):
        self.calls += 1
        return HostStopResult("none")


class ForbiddenResidualScan:
    @staticmethod
    def stop_stale_codex_hosts():
        raise AssertionError("residual scan must not run")


def write_fake_codex(path: Path) -> Path:
    path.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
stdin = sys.stdin.read()
log_path = Path(os.environ["EMP_FAKE_CODEX_LOG"])
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps({"args": args, "stdin": stdin}) + "\\n")
mode = os.environ.get("EMP_FAKE_CODEX_MODE", "")
if args == ["remote-control", "stop", "--json"]:
    if mode == "no_runtime":
        print(json.dumps({"status": "notRunning"}))
        raise SystemExit(0)
    if mode == "stop_permission":
        print("permission denied", file=sys.stderr)
        raise SystemExit(1)
    if mode == "stop_malformed":
        print("not-json")
        raise SystemExit(0)
    if mode == "unmanaged_host":
        print(
            "app server is running but is not managed by codex remote-control",
            file=sys.stderr,
        )
        raise SystemExit(1)
    print(json.dumps({"status": "stopped"}))
    raise SystemExit(0)
if args == ["app-server", "proxy"]:
    count_path = Path(os.environ.get("CODEX_HOME", ".")) / "fake-proxy-count"
    count_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        count = int(count_path.read_text(encoding="utf-8")) + 1
    except (FileNotFoundError, ValueError):
        count = 1
    count_path.write_text(str(count), encoding="utf-8")
    if count <= int(os.environ.get("EMP_FAKE_UNAVAILABLE_POLLS", "0")):
        print("connection refused", file=sys.stderr)
        raise SystemExit(1)
    if mode in ("unavailable", "no_runtime", "unmanaged_host"):
        print("connection refused", file=sys.stderr)
        raise SystemExit(1)
    if mode == "observe_permission":
        print("permission denied", file=sys.stderr)
        raise SystemExit(1)
    if mode == "observe_malformed":
        print("not-json")
        raise SystemExit(0)
    request = [json.loads(line) for line in stdin.splitlines() if line.strip()]
    model_request = next(item for item in request if item.get("method") == "model/list")
    cursor = model_request.get("params", {}).get("cursor")
    pages = json.loads(os.environ.get("EMP_FAKE_CODEX_PAGES", "{}"))
    key = "" if cursor is None else str(cursor)
    page = pages.get(key, {"data": []})
    print(json.dumps({"id": 2, "result": page}))
    raise SystemExit(0)
print("unexpected command", file=sys.stderr)
raise SystemExit(97)
""",
        encoding="utf-8",
    )
    path.chmod(0o700)
    return path


def read_calls(path: Path):
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class CodexRuntimeProcessTests(unittest.TestCase):
    def test_subprocess_runner_caps_both_outputs_without_capture_output(self):
        stdout_cap = 2 * 1024 * 1024
        stderr_cap = 32 * 1024
        script = (
            "import sys; "
            "sys.stdout.write('O' * %d); "
            "sys.stderr.write('E' * %d); "
            "raise SystemExit(7)"
        ) % (stdout_cap + 8192, stderr_cap + 8192)
        original_run = subprocess.run

        def bounded_sink_guard(*args, **kwargs):
            self.assertFalse(kwargs.get("capture_output", False))
            self.assertNotEqual(kwargs.get("stdout"), subprocess.PIPE)
            self.assertNotEqual(kwargs.get("stderr"), subprocess.PIPE)
            self.assertFalse(kwargs.get("shell", False))
            self.assertTrue(hasattr(kwargs.get("stdout"), "fileno"))
            self.assertTrue(hasattr(kwargs.get("stderr"), "fileno"))
            return original_run(*args, **kwargs)

        with patch(
            "easy_multi_provider.codex_runtime.subprocess.run",
            side_effect=bounded_sink_guard,
        ):
            result = SubprocessRunner().run(
                [sys.executable, "-c", script], timeout=10
            )

        self.assertEqual(result.returncode, 7)
        self.assertEqual(len(result.stdout), stdout_cap)
        self.assertEqual(len(result.stderr), stderr_cap)
        self.assertEqual(set(result.stdout), {"O"})
        self.assertEqual(set(result.stderr), {"E"})

    def test_subprocess_runner_preserves_timeout_result(self):
        result = SubprocessRunner().run(
            [sys.executable, "-c", "import time; time.sleep(5)"],
            timeout=0.05,
        )

        self.assertEqual(result.returncode, 124)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "command timed out")

    def test_targeted_stopper_selects_matching_custom_codex_home(self):
        with tempfile.TemporaryDirectory() as directory:
            target_home = str((Path(directory) / "target-codex-home").resolve())
            identity = ProcessIdentity(
                42000,
                0,
                "test-user",
                1.0,
                "/opt/codex",
                ("/opt/codex", "remote-control", "--json"),
                target_home,
            )

            class MatchingHomeInventory:
                current_username = "test-user"

                def __init__(self):
                    self.terminated = []

                @staticmethod
                def list_processes():
                    return (identity,)

                def terminate(self, expected, _timeout):
                    self.terminated.append(expected)
                    return "stopped"

            inventory = MatchingHomeInventory()
            result = TargetedCodexHostStopper(
                target_codex_home=Path(target_home),
                process_inventory=inventory,
                termination_timeout=0.2,
            ).stop_stale_codex_hosts()

            self.assertEqual(result.status, "stopped")
            self.assertEqual(inventory.terminated, [identity])

    def test_targeted_stopper_excludes_different_custom_codex_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target_home = str((root / "target-codex-home").resolve())
            other_home = str((root / "other-codex-home").resolve())
            identity = ProcessIdentity(
                42000,
                0,
                "test-user",
                1.0,
                "/opt/codex",
                ("/opt/codex", "remote-control", "--json"),
                other_home,
            )

            class DifferentHomeInventory:
                current_username = "test-user"

                @staticmethod
                def list_processes():
                    return (identity,)

                @staticmethod
                def terminate(_expected, _timeout):
                    raise AssertionError("different CODEX_HOME was selected")

            result = TargetedCodexHostStopper(
                target_codex_home=Path(target_home),
                process_inventory=DifferentHomeInventory(),
                termination_timeout=0.2,
            ).stop_stale_codex_hosts()

            self.assertEqual(result.status, "none")

    def test_psutil_revalidation_rejects_changed_identity_without_termination(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={**os.environ, "CODEX_HOME": TEST_CODEX_HOME},
        )

        def cleanup():
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=2)

        self.addCleanup(cleanup)
        inventory = PsutilProcessInventory(pids=(child.pid,))
        process = ControlledPsutilCodexProcess(
            child, inventory.current_username, TEST_CODEX_HOME
        )
        with patch.object(inventory._psutil, "Process", return_value=process):
            identity = inventory.list_processes()[0]
            result = inventory.terminate(
                replace(identity, argv=identity.argv + ("changed",)), 0.2
            )

        self.assertEqual(result, "raced")
        self.assertIsNone(child.poll())

    def test_psutil_inventory_excludes_default_home_when_target_is_custom(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child_environment = {
                key: value for key, value in os.environ.items() if key != "CODEX_HOME"
            }
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=child_environment,
            )

            def cleanup():
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

            self.addCleanup(cleanup)
            inventory = PsutilProcessInventory(
                root / "custom-codex-home",
                pids=(child.pid,),
                default_codex_home=root / "default-codex-home",
            )
            process = ControlledPsutilCodexProcess(
                child, inventory.current_username, _MISSING_CODEX_HOME
            )
            with patch.object(inventory._psutil, "Process", return_value=process):
                identities = inventory.list_processes()

            self.assertEqual(identities, ())
            self.assertIsNone(child.poll())

    def test_psutil_inventory_never_reads_environment_for_non_codex_process(self):
        inventory = _RuntimePsutilProcessInventory(
            Path(TEST_CODEX_HOME), pids=(42005,), default_codex_home=Path(TEST_CODEX_HOME)
        )

        class NonCodexProcess:
            pid = 42005

            @staticmethod
            def ppid():
                return 0

            def username(self):
                return inventory.current_username

            @staticmethod
            def create_time():
                return 1.0

            @staticmethod
            def exe():
                return "/usr/bin/python"

            @staticmethod
            def cmdline():
                return ["python", "worker.py"]

            @staticmethod
            def environ():
                raise AssertionError("environment read for a non-candidate process")

        with patch.object(
            inventory._psutil, "Process", return_value=NonCodexProcess()
        ):
            identities = inventory.list_processes()

        self.assertEqual(identities, ())

    def test_psutil_inventory_excludes_candidate_with_unreadable_codex_home(self):
        inventory = _RuntimePsutilProcessInventory(
            Path(TEST_CODEX_HOME), pids=(42006,), default_codex_home=Path(TEST_CODEX_HOME)
        )

        class UnreadableHomeProcess:
            pid = 42006

            @staticmethod
            def ppid():
                return 0

            def username(self):
                return inventory.current_username

            @staticmethod
            def create_time():
                return 1.0

            @staticmethod
            def exe():
                return "/opt/codex"

            @staticmethod
            def cmdline():
                return ["/opt/codex", "remote-control", "--json"]

            @staticmethod
            def environ():
                raise inventory._psutil.AccessDenied(42006)

        with patch.object(
            inventory._psutil, "Process", return_value=UnreadableHomeProcess()
        ):
            identities = inventory.list_processes()

        self.assertEqual(identities, ())

    def test_psutil_inventory_selects_matching_custom_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target_home = root / "custom-codex-home"
            child_environment = dict(os.environ)
            child_environment["CODEX_HOME"] = str(target_home)
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=child_environment,
            )

            def cleanup():
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

            self.addCleanup(cleanup)
            inventory = PsutilProcessInventory(target_home, pids=(child.pid,))
            process = ControlledPsutilCodexProcess(
                child, inventory.current_username, str(target_home)
            )
            with patch.object(inventory._psutil, "Process", return_value=process):
                identities = inventory.list_processes()

            self.assertEqual([identity.pid for identity in identities], [child.pid])
            self.assertEqual(identities[0].effective_codex_home, str(target_home.resolve()))

    def test_psutil_inventory_excludes_different_custom_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child_environment = dict(os.environ)
            child_environment["CODEX_HOME"] = str(root / "other-codex-home")
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=child_environment,
            )

            def cleanup():
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

            self.addCleanup(cleanup)
            inventory = PsutilProcessInventory(
                root / "target-codex-home", pids=(child.pid,)
            )
            process = ControlledPsutilCodexProcess(
                child, inventory.current_username, str(root / "other-codex-home")
            )
            with patch.object(inventory._psutil, "Process", return_value=process):
                identities = inventory.list_processes()

            self.assertEqual(identities, ())
            self.assertIsNone(child.poll())

    def test_psutil_inventory_accepts_missing_env_for_default_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            default_home = root / "default-codex-home"
            child_environment = {
                key: value for key, value in os.environ.items() if key != "CODEX_HOME"
            }
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=child_environment,
            )

            def cleanup():
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

            self.addCleanup(cleanup)
            inventory = PsutilProcessInventory(
                default_home,
                pids=(child.pid,),
                default_codex_home=default_home,
            )
            process = ControlledPsutilCodexProcess(
                child, inventory.current_username, _MISSING_CODEX_HOME
            )
            with patch.object(inventory._psutil, "Process", return_value=process):
                identities = inventory.list_processes()

            self.assertEqual([identity.pid for identity in identities], [child.pid])
            self.assertEqual(identities[0].effective_codex_home, str(default_home.resolve()))

    def test_psutil_revalidation_rejects_changed_codex_home_as_raced(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target_home = root / "target-codex-home"
            inventory = PsutilProcessInventory(target_home, pids=())

            class MutableProcess:
                pid = 42003

                def __init__(self):
                    self.codex_home = str(target_home)
                    self.terminate_called = False

                @staticmethod
                def ppid():
                    return 0

                def username(self):
                    return inventory.current_username

                @staticmethod
                def create_time():
                    return 1.0

                @staticmethod
                def exe():
                    return "/opt/codex"

                @staticmethod
                def cmdline():
                    return ["/opt/codex", "remote-control", "--json"]

                def environ(self):
                    return {"CODEX_HOME": self.codex_home, "UNRELATED": "discard-me"}

                def terminate(self):
                    self.terminate_called = True

                @staticmethod
                def wait(_timeout):
                    return 0

            process = MutableProcess()
            identity = inventory._snapshot(process)
            process.codex_home = str(root / "different-codex-home")

            with patch.object(inventory._psutil, "Process", return_value=process):
                result = inventory.terminate(identity, 0.1)

            self.assertEqual(result, "raced")
            self.assertFalse(process.terminate_called)

    def test_graceful_timeout_is_stop_failed_without_hard_kill(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={**os.environ, "CODEX_HOME": TEST_CODEX_HOME},
        )

        def cleanup():
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=2)

        self.addCleanup(cleanup)
        identity = ProcessIdentity(
            child.pid,
            0,
            "test-user",
            1.0,
            "/opt/codex",
            ("/opt/codex", "remote-control", "--json"),
        )

        class TimeoutInventory:
            current_username = "test-user"

            @staticmethod
            def list_processes():
                return (identity,)

            @staticmethod
            def terminate(_expected, _timeout):
                return "timeout"

            @staticmethod
            def kill(_expected):
                raise AssertionError("hard kill must never be called")

        result = TargetedCodexHostStopper(
            process_inventory=TimeoutInventory(), termination_timeout=0.05
        ).stop_stale_codex_hosts()

        self.assertEqual(result.status, "failed")
        self.assertEqual(result.stopped_count, 0)
        self.assertIsNone(child.poll())
        self.assertNotIn(str(child.pid), result.detail)
        self.assertNotIn("/opt/codex", result.detail)

    def test_non_unmanaged_lifecycle_error_never_activates_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"

            class ForbiddenStopper:
                @staticmethod
                def stop_stale_codex_hosts():
                    raise AssertionError("fallback was activated")

            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=ForbiddenStopper(),
                control_timeout=0.2,
                observation_timeout=0.01,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "stop_permission",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, STOP_FAILED)

    def test_excluded_codex_client_roles_are_never_terminated(self):
        shapes = (
            ("test-user", "/opt/codex", ("/opt/codex",)),
            ("test-user", "/opt/codex", ("/opt/codex", "exec", "task")),
            ("test-user", "/opt/codex", ("/opt/codex", "resume")),
            ("test-user", "/opt/codex", ("/opt/codex", "review")),
            ("test-user", "/opt/codex", ("/opt/codex", "app-server", "proxy")),
            ("test-user", "/opt/codex", ("/opt/codex", "app-server", "daemon", "restart")),
            ("test-user", "/opt/codex", ("/opt/codex", "app-server", "generate-json-schema")),
            ("test-user", "/opt/codex", ("/opt/codex", "remote-control", "stop")),
            ("other-user", "/opt/codex", ("/opt/codex", "remote-control", "--json")),
            ("test-user", "/usr/bin/python", ("codex", "remote-control", "--json")),
            ("test-user", "/opt/codex", ("/opt/codex", "app-server")),
        )
        children = [
            subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            for _shape in shapes
        ]

        def cleanup():
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

        self.addCleanup(cleanup)
        identities = tuple(
            ProcessIdentity(
                child.pid,
                0,
                username,
                float(index + 1),
                executable,
                argv,
            )
            for index, (child, (username, executable, argv)) in enumerate(
                zip(children, shapes)
            )
        )

        class ExcludedInventory:
            current_username = "test-user"

            @staticmethod
            def list_processes():
                return identities

            @staticmethod
            def terminate(_expected, _timeout):
                raise AssertionError("excluded process was selected")

        result = TargetedCodexHostStopper(
            process_inventory=ExcludedInventory(), termination_timeout=0.2
        ).stop_stale_codex_hosts()

        self.assertEqual(result.status, "none")
        self.assertTrue(all(child.poll() is None for child in children))

    def test_same_user_listening_app_server_is_gracefully_terminated(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        def cleanup():
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=2)

        self.addCleanup(cleanup)
        identity = ProcessIdentity(
            child.pid,
            0,
            "test-user",
            1.0,
            "/opt/codex",
            ("/opt/codex", "app-server", "--listen", "127.0.0.1:0"),
        )

        class ListenerInventory:
            current_username = "test-user"

            @staticmethod
            def list_processes():
                return (identity,)

            @staticmethod
            def terminate(expected, timeout):
                if expected != identity:
                    return "raced"
                child.terminate()
                child.wait(timeout=timeout)
                return "stopped"

        result = TargetedCodexHostStopper(
            process_inventory=ListenerInventory(), termination_timeout=0.5
        ).stop_stale_codex_hosts()

        self.assertEqual(result.status, "stopped")
        self.assertEqual(result.stopped_count, 1)

    def test_native_desktop_host_with_config_root_option_is_classified(self):
        identity = ProcessIdentity(
            42001,
            0,
            "test-user",
            1.0,
            "codex",
            (
                "codex",
                "-c",
                "features.some=true",
                "app-server",
                "--listen",
                "unix://runtime.sock",
            ),
        )

        class NativeRootOptionInventory:
            current_username = "test-user"

            def __init__(self):
                self.terminated = []

            @staticmethod
            def list_processes():
                return (identity,)

            def terminate(self, expected, _timeout):
                self.terminated.append(expected)
                return "stopped"

        inventory = NativeRootOptionInventory()
        result = TargetedCodexHostStopper(
            process_inventory=inventory, termination_timeout=0.2
        ).stop_stale_codex_hosts()

        self.assertEqual(result.status, "stopped")
        self.assertEqual(inventory.terminated, [identity])

    def test_canonical_node_shim_with_config_root_option_is_classified(self):
        with tempfile.TemporaryDirectory() as directory:
            package_bin = Path(directory) / "node_modules" / "@openai" / "codex" / "bin"
            package_bin.mkdir(parents=True)
            launcher_target = package_bin / "codex.js"
            launcher_target.write_text("// controlled test launcher\n", encoding="utf-8")
            launcher_shim = package_bin / "codex"
            launcher_shim.symlink_to(launcher_target.name)
            identity = ProcessIdentity(
                42002,
                0,
                "test-user",
                1.0,
                "node",
                (
                    "node",
                    str(launcher_shim),
                    "-c",
                    "features.some=true",
                    "app-server",
                    "--listen",
                    "unix://runtime.sock",
                ),
            )

            class NodeRootOptionInventory:
                current_username = "test-user"

                def __init__(self):
                    self.terminated = []

                @staticmethod
                def list_processes():
                    return (identity,)

                def terminate(self, expected, _timeout):
                    self.terminated.append(expected)
                    return "stopped"

            inventory = NodeRootOptionInventory()
            result = TargetedCodexHostStopper(
                process_inventory=inventory, termination_timeout=0.2
            ).stop_stale_codex_hosts()

            self.assertEqual(result.status, "stopped")
            self.assertEqual(inventory.terminated, [identity])

    def test_known_value_taking_root_option_forms_are_classified(self):
        option_prefixes = (
            ("--config", "features.some=true"),
            ("--enable", "feature_a"),
            ("--disable", "feature_b"),
            ("--config=features.some=true",),
            ("--enable=feature_a",),
            ("--disable=feature_b",),
        )
        for index, prefix in enumerate(option_prefixes):
            with self.subTest(prefix=prefix):
                identity = ProcessIdentity(
                    42100 + index,
                    0,
                    "test-user",
                    1.0,
                    "codex",
                    (
                        "codex",
                        *prefix,
                        "app-server",
                        "--listen=unix://runtime.sock",
                    ),
                )

                class KnownRootOptionInventory:
                    current_username = "test-user"

                    def __init__(self):
                        self.terminated = []

                    @staticmethod
                    def list_processes():
                        return (identity,)

                    def terminate(self, expected, _timeout):
                        self.terminated.append(expected)
                        return "stopped"

                inventory = KnownRootOptionInventory()
                result = TargetedCodexHostStopper(
                    process_inventory=inventory, termination_timeout=0.2
                ).stop_stale_codex_hosts()

                self.assertEqual(result.status, "stopped")
                self.assertEqual(inventory.terminated, [identity])

    def test_ambiguous_root_prefixes_are_rejected_without_loose_scanning(self):
        ambiguous_prefixes = (
            ("--unknown", "value"),
            ("-c",),
            ("-c", "app-server"),
            ("--config", "--enable"),
            ("--config=",),
            ("--enable=",),
            ("--disable=",),
            ("-c=features.some=true",),
            ("unrelated",),
        )
        for index, prefix in enumerate(ambiguous_prefixes):
            with self.subTest(prefix=prefix):
                identity = ProcessIdentity(
                    42200 + index,
                    0,
                    "test-user",
                    1.0,
                    "codex",
                    (
                        "codex",
                        *prefix,
                        "app-server",
                        "--listen",
                        "unix://runtime.sock",
                    ),
                )

                class AmbiguousRootOptionInventory:
                    current_username = "test-user"

                    @staticmethod
                    def list_processes():
                        return (identity,)

                    @staticmethod
                    def terminate(_expected, _timeout):
                        raise AssertionError("ambiguous root prefix was selected")

                result = TargetedCodexHostStopper(
                    process_inventory=AmbiguousRootOptionInventory(),
                    termination_timeout=0.2,
                ).stop_stale_codex_hosts()

                self.assertEqual(result.status, "none")

    def test_psutil_inventory_can_be_scoped_to_one_controlled_process(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env={**os.environ, "CODEX_HOME": TEST_CODEX_HOME},
        )

        def cleanup():
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=2)

        self.addCleanup(cleanup)
        inventory = PsutilProcessInventory(pids=(child.pid,))
        process = ControlledPsutilCodexProcess(
            child, inventory.current_username, TEST_CODEX_HOME
        )
        with patch.object(inventory._psutil, "Process", return_value=process):
            identities = inventory.list_processes()
            outcome = inventory.terminate(identities[0], 1)

        self.assertEqual([identity.pid for identity in identities], [child.pid])
        self.assertEqual(outcome, "stopped")
        self.assertIsNotNone(child.poll())

    def test_native_child_is_terminated_before_official_node_launcher(self):
        package_temp = tempfile.TemporaryDirectory()
        self.addCleanup(package_temp.cleanup)
        package_bin = (
            Path(package_temp.name) / "node_modules" / "@openai" / "codex" / "bin"
        )
        package_bin.mkdir(parents=True)
        launcher_script = package_bin / "codex.js"
        launcher_script.write_text("// controlled test launcher\n", encoding="utf-8")
        children = [
            subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            for _ in range(2)
        ]

        def cleanup():
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

        self.addCleanup(cleanup)
        launcher, runtime = children
        launcher_identity = ProcessIdentity(
            launcher.pid,
            0,
            "test-user",
            1.0,
            "/usr/bin/node",
            (
                "/usr/bin/node",
                str(launcher_script),
                "remote-control",
                "--json",
            ),
        )
        runtime_identity = ProcessIdentity(
            runtime.pid,
            launcher.pid,
            "test-user",
            2.0,
            "/opt/codex",
            ("/opt/codex", "remote-control", "--json"),
        )

        class FamilyInventory:
            current_username = "test-user"

            def __init__(self):
                self.processes = {launcher.pid: launcher, runtime.pid: runtime}
                self.order = []

            def list_processes(self):
                return (launcher_identity, runtime_identity)

            def terminate(self, expected, timeout):
                process = self.processes[expected.pid]
                process.terminate()
                process.wait(timeout=timeout)
                self.order.append(expected.pid)
                return "stopped"

        inventory = FamilyInventory()
        result = TargetedCodexHostStopper(
            process_inventory=inventory, termination_timeout=1
        ).stop_stale_codex_hosts()

        self.assertEqual(result.status, "stopped")
        self.assertEqual(result.stopped_count, 2)
        self.assertEqual(inventory.order, [runtime.pid, launcher.pid])

    def test_official_node_bin_codex_symlink_is_classified_by_canonical_target(self):
        with tempfile.TemporaryDirectory() as directory:
            package_bin = Path(directory) / "node_modules" / "@openai" / "codex" / "bin"
            package_bin.mkdir(parents=True)
            launcher_target = package_bin / "codex.js"
            launcher_target.write_text("// controlled test launcher\n", encoding="utf-8")
            launcher_shim = package_bin / "codex"
            launcher_shim.symlink_to(launcher_target.name)
            identity = ProcessIdentity(
                41001,
                0,
                "test-user",
                1.0,
                "/usr/bin/node",
                (
                    "/usr/bin/node",
                    str(launcher_shim),
                    "remote-control",
                    "--json",
                ),
            )

            class ShimInventory:
                current_username = "test-user"

                def __init__(self):
                    self.terminated = []

                @staticmethod
                def list_processes():
                    return (identity,)

                def terminate(self, expected, _timeout):
                    self.terminated.append(expected)
                    return "stopped"

            inventory = ShimInventory()
            result = TargetedCodexHostStopper(
                process_inventory=inventory, termination_timeout=0.2
            ).stop_stale_codex_hosts()

            self.assertEqual(result.status, "stopped")
            self.assertEqual(inventory.terminated, [identity])

    def test_node_bin_codex_symlink_to_lookalike_script_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package_bin = root / "node_modules" / "@openai" / "codex" / "bin"
            package_bin.mkdir(parents=True)
            lookalike = root / "lookalike" / "codex.js"
            lookalike.parent.mkdir()
            lookalike.write_text("// not the package launcher\n", encoding="utf-8")
            launcher_shim = package_bin / "codex"
            launcher_shim.symlink_to(lookalike)
            identity = ProcessIdentity(
                41002,
                0,
                "test-user",
                1.0,
                "/usr/bin/node",
                (
                    "/usr/bin/node",
                    str(launcher_shim),
                    "remote-control",
                    "--json",
                ),
            )

            class LookalikeInventory:
                current_username = "test-user"

                @staticmethod
                def list_processes():
                    return (identity,)

                @staticmethod
                def terminate(_expected, _timeout):
                    raise AssertionError("lookalike launcher was selected")

            result = TargetedCodexHostStopper(
                process_inventory=LookalikeInventory(), termination_timeout=0.2
            ).stop_stale_codex_hosts()

            self.assertEqual(result.status, "none")

    def test_same_user_foreground_remote_control_is_gracefully_terminated(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        def cleanup():
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=2)

        self.addCleanup(cleanup)
        identity = SimpleNamespace(
            pid=child.pid,
            parent_pid=0,
            username="test-user",
            create_time=1.0,
            executable="/opt/codex",
            argv=("/opt/codex", "remote-control", "--json"),
            effective_codex_home=TEST_CODEX_HOME,
        )

        class ScopedControlledInventory:
            current_username = "test-user"

            def __init__(self):
                self.terminated = []

            def list_processes(self):
                return (identity,)

            def terminate(self, expected, timeout):
                self.assert_identity(expected)
                child.terminate()
                child.wait(timeout=timeout)
                self.terminated.append(expected.pid)
                return "stopped"

            @staticmethod
            def assert_identity(expected):
                if expected != identity or child.poll() is not None:
                    raise AssertionError("controlled process identity changed")

        inventory = ScopedControlledInventory()
        stopper = TargetedCodexHostStopper(
            process_inventory=inventory,
            termination_timeout=0.5,
        )

        result = stopper.stop_stale_codex_hosts()

        self.assertEqual(result.status, "stopped")
        self.assertEqual(result.stopped_count, 1)
        self.assertEqual(inventory.terminated, [child.pid])
        self.assertIsNotNone(child.poll())

    def test_documented_unmanaged_host_error_activates_targeted_fallback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"

            class RecordingStopper:
                def __init__(self):
                    self.calls = 0

                def stop_stale_codex_hosts(self):
                    self.calls += 1
                    return SimpleNamespace(
                        status="stopped", stopped_count=1, detail=""
                    )

            stopper = RecordingStopper()
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=stopper,
                control_timeout=0.2,
                observation_timeout=0.01,
                poll_interval=0.002,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "unmanaged_host",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(stopper.calls, 1)
            self.assertEqual(result.state, STOPPED_WAITING_FOR_START)
            commands = [call["args"] for call in read_calls(log_path)]
            self.assertEqual(commands[0], ["remote-control", "stop", "--json"])
            self.assertTrue(all(command != ["remote-control", "start"] for command in commands))

    def test_unmanaged_host_error_with_no_safe_candidate_is_unsupported(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            stopper = NoResidualHosts()
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=stopper,
                control_timeout=0.2,
                observation_timeout=0.01,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "unmanaged_host",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(stopper.calls, 1)
            self.assertEqual(result.state, UNSUPPORTED)
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [["remote-control", "stop", "--json"]],
            )

    def test_successful_lifecycle_with_failed_residual_scan_is_stop_failed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"

            class FailedResidualScan:
                @staticmethod
                def stop_stale_codex_hosts():
                    return HostStopResult("failed", detail="Residual host did not stop")

            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=FailedResidualScan(),
                control_timeout=0.2,
                observation_timeout=0.01,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "no_runtime",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, STOP_FAILED)
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [["remote-control", "stop", "--json"]],
            )

    def test_real_camel_case_not_running_is_success_without_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            stopper = NoResidualHosts()
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=stopper,
                control_timeout=0.2,
                observation_timeout=0.05,
                poll_interval=0.005,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "no_runtime",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",),
                    "emp",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, STOPPED_WAITING_FOR_START)
            self.assertEqual(stopper.calls, 1)
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [["remote-control", "stop", "--json"]],
            )

    def test_stopped_status_always_performs_residual_scan_before_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            stopper = NoResidualHosts()
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=stopper,
                control_timeout=0.2,
                observation_timeout=0.05,
                poll_interval=0.005,
            )
            pages = {"": {"data": [{"id": "external/model-a"}]}}
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "ready",
                    "EMP_FAKE_CODEX_PAGES": json.dumps(pages),
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(stopper.calls, 1)
            self.assertEqual(result.state, EMP_LOADED)
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [
                    ["remote-control", "stop", "--json"],
                    ["app-server", "proxy"],
                ],
            )

    def test_not_running_still_terminates_controlled_residual_foreground_host(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            child = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

            def cleanup():
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=2)

            self.addCleanup(cleanup)
            identity = ProcessIdentity(
                child.pid,
                0,
                "test-user",
                1.0,
                "/opt/codex",
                ("/opt/codex", "remote-control", "--json"),
            )

            class ControlledInventory:
                current_username = "test-user"

                @staticmethod
                def list_processes():
                    return (identity,)

                @staticmethod
                def terminate(expected, timeout):
                    if expected != identity:
                        return "raced"
                    child.terminate()
                    child.wait(timeout=timeout)
                    return "stopped"

            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=TargetedCodexHostStopper(
                    process_inventory=ControlledInventory(),
                    termination_timeout=0.5,
                ),
                control_timeout=0.2,
                observation_timeout=0.01,
                poll_interval=0.002,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "no_runtime",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, STOPPED_WAITING_FOR_START)
            self.assertIsNotNone(child.poll())
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [
                    ["remote-control", "stop", "--json"],
                    ["app-server", "proxy"],
                ],
            )

    def test_model_list_permission_failure_is_not_reported_as_waiting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.03,
                poll_interval=0.005,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "observe_permission",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",),
                    "emp",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, VERIFICATION_FAILED)
            commands = [call["args"] for call in read_calls(log_path)]
            self.assertEqual(commands[0], ["remote-control", "stop", "--json"])
            self.assertEqual(commands[1], ["app-server", "proxy"])
            self.assertEqual(len(commands), 2)

    def test_stop_permission_failure_stays_failed_and_never_observes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=ForbiddenResidualScan(),
                control_timeout=0.2,
                observation_timeout=0.01,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "stop_permission",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, STOP_FAILED)
            self.assertEqual(
                [call["args"] for call in read_calls(log_path)],
                [["remote-control", "stop", "--json"]],
            )

    def test_malformed_stop_response_is_unsupported_without_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=ForbiddenResidualScan(),
                control_timeout=0.2,
                observation_timeout=0.01,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "stop_malformed",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, UNSUPPORTED)
            self.assertEqual(len(read_calls(log_path)), 1)

    def test_paginated_model_list_requires_every_expected_slug(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.2,
                poll_interval=0.005,
            )
            pages = {
                "": {"data": [{"id": "external/model-a"}], "nextCursor": "page-2"},
                "page-2": {"data": [{"id": "external/model-b"}]},
            }
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "ready",
                    "EMP_FAKE_CODEX_PAGES": json.dumps(pages),
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a", "external/model-b"),
                    "emp",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, EMP_LOADED)
            calls = read_calls(log_path)
            proxies = [call for call in calls if call["args"] == ["app-server", "proxy"]]
            self.assertEqual(len(proxies), 2)
            first = [json.loads(line) for line in proxies[0]["stdin"].splitlines()]
            second = [json.loads(line) for line in proxies[1]["stdin"].splitlines()]
            self.assertNotIn("cursor", first[-1]["params"])
            self.assertEqual(second[-1]["params"]["cursor"], "page-2")

    def test_delayed_external_restart_is_observed_without_start_command(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.3,
                poll_interval=0.005,
            )
            pages = {"": {"data": [{"id": "external/model-a"}]}}
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "ready",
                    "EMP_FAKE_UNAVAILABLE_POLLS": "2",
                    "EMP_FAKE_CODEX_PAGES": json.dumps(pages),
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

            self.assertEqual(result.state, EMP_LOADED)
            commands = [call["args"] for call in read_calls(log_path)]
            self.assertEqual(commands[0], ["remote-control", "stop", "--json"])
            self.assertEqual(commands[1:], [["app-server", "proxy"]] * 3)
            self.assertFalse(any("start" in command for call in commands for command in call))

    def test_partial_catalog_is_verification_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.1,
            )
            pages = {"": {"data": [{"id": "external/model-a"}]}}
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "ready",
                    "EMP_FAKE_CODEX_PAGES": json.dumps(pages),
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a", "external/model-b"),
                    "emp",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, VERIFICATION_FAILED)
            self.assertFalse(result.verified)

    def test_native_target_is_loaded_only_when_all_emp_slugs_are_absent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.1,
            )
            pages = {"": {"data": [{"id": "native-model"}]}}
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "ready",
                    "EMP_FAKE_CODEX_PAGES": json.dumps(pages),
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a", "external/model-b"),
                    "native",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, NATIVE_LOADED)
            self.assertTrue(result.verified)

    def test_malformed_model_list_is_verification_failure_not_waiting(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log_path = root / "calls.jsonl"
            controller = CodexRuntimeController(
                codex_executable=str(write_fake_codex(root / "codex")),
                host_stopper=NoResidualHosts(),
                control_timeout=0.2,
                observation_timeout=0.03,
            )
            with patch.dict(
                os.environ,
                {
                    "EMP_FAKE_CODEX_LOG": str(log_path),
                    "EMP_FAKE_CODEX_MODE": "observe_malformed",
                    "CODEX_HOME": str(root / "codex-home"),
                },
            ):
                result = controller.reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )
            self.assertEqual(result.state, VERIFICATION_FAILED)


if __name__ == "__main__":
    unittest.main()
