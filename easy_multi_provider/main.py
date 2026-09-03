"""Command-line entry points for the local EMP process and integration doctor."""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping, Optional, Sequence

from . import __version__
from .config import ConfigError
from .codex_runtime import (
    RELOAD_REQUIRED,
    RuntimeRecoveryStore,
    RuntimeSyncError,
    offline_runtime_snapshot,
)
from .integration import IntegrationError, IntegrationManager, IntegrationResult, IntegrationStatus


DEFAULT_EMP_STATE_DIR = Path("easy-multi-provider") / "integration"


def _next_action(state: str) -> str:
    if state in ("prepared", "restoring"):
        return "run restore"
    if state == "active":
        return "confirm service health or run restore"
    if state == "conflict":
        return "manually inspect; EMP will not overwrite user changes"
    return "none"


@dataclass(frozen=True)
class IntegrationPaths:
    """The resolved, local paths used by offline integration commands."""

    codex_home: Path
    config_path: Path
    state_dir: Path
    lease_path: Path
    lock_path: Path
    runtime_path: Path


def resolve_codex_home(
    environ: Optional[Mapping[str, str]] = None,
    user_home: Optional[Path] = None,
) -> Path:
    """Resolve CODEX_HOME without consulting any other Codex state location."""

    environment = os.environ if environ is None else environ
    configured = environment.get("CODEX_HOME", "").strip()
    if configured:
        return Path(configured).expanduser().resolve()
    home = Path.home() if user_home is None else Path(user_home)
    return (home / ".codex").resolve()


def resolve_desktop_config_path(
    environ: Optional[Mapping[str, str]] = None,
    user_home: Optional[Path] = None,
    platform_name: Optional[str] = None,
) -> Path:
    """Return the writable per-user config used by packaged desktop launchers."""

    environment = os.environ if environ is None else environ
    home = Path.home() if user_home is None else Path(user_home)
    active_platform = sys.platform if platform_name is None else platform_name
    if active_platform == "win32":
        configured = (
            environment.get("LOCALAPPDATA", "").strip()
            or environment.get("APPDATA", "").strip()
        )
        root = (
            Path(configured).expanduser()
            if configured
            else home / "AppData" / "Local"
        )
        return root / "EasyMultiProvider" / "config.json"
    if active_platform == "darwin":
        return (
            home
            / "Library"
            / "Application Support"
            / "EasyMultiProvider"
            / "config.json"
        )

    configured = environment.get("XDG_CONFIG_HOME", "").strip()
    root = Path(configured).expanduser() if configured else home / ".config"
    return root / "easy-multi-provider" / "config.json"


def resolve_integration_paths(
    codex_home: Optional[Path] = None,
    state_dir: Optional[Path] = None,
    environ: Optional[Mapping[str, str]] = None,
    user_home: Optional[Path] = None,
    cwd: Optional[Path] = None,
) -> IntegrationPaths:
    """Resolve Codex config and one explicit EMP-local lease location.

    With no override, the EMP-local state directory is below ``CODEX_HOME``.
    A relative explicit ``state_dir`` is resolved against the invocation
    directory once. No directory is searched.
    """

    resolved_home = (
        resolve_codex_home(environ=environ, user_home=user_home)
        if codex_home is None
        else Path(codex_home).expanduser().resolve()
    )
    if state_dir is None:
        configured_state = resolved_home / DEFAULT_EMP_STATE_DIR
    else:
        configured_state = Path(state_dir).expanduser()
        if not configured_state.is_absolute():
            invocation_cwd = Path.cwd() if cwd is None else Path(cwd)
            configured_state = invocation_cwd / configured_state
    resolved_state = configured_state.resolve()
    return IntegrationPaths(
        codex_home=resolved_home,
        config_path=resolved_home / "config.toml",
        state_dir=resolved_state,
        lease_path=resolved_state / "lease.json",
        lock_path=resolved_state / "lease.lock",
        runtime_path=resolved_state / "runtime.json",
    )


def _add_serve_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--config", type=Path, help="EMP JSON configuration path")
    parser.add_argument("--host", help="override the local listen host")
    parser.add_argument("--port", type=int, help="override the local listen port")
    parser.add_argument(
        "--open-browser",
        action="store_true",
        help=argparse.SUPPRESS,
    )


def _add_integration_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--state-dir",
        type=Path,
        help=(
            "EMP local state directory "
            "(default: CODEX_HOME/easy-multi-provider/integration; "
            "relative explicit values resolve from cwd)"
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print the operation result as JSON",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="EMP Runtime Control Plane commands"
    )
    parser.add_argument(
        "--version",
        action="version",
        version="EMP %s" % __version__,
    )
    commands = parser.add_subparsers(dest="command", metavar="COMMAND")

    serve_parser = commands.add_parser("serve", help="start the existing EMP service")
    _add_serve_arguments(serve_parser)

    doctor_parser = commands.add_parser(
        "doctor", help="read Codex integration status without starting a service"
    )
    _add_integration_arguments(doctor_parser)

    restore_parser = commands.add_parser(
        "restore", help="restore native Codex fields without starting a service"
    )
    _add_integration_arguments(restore_parser)
    return parser


def _format_status(status: IntegrationStatus, runtime: Mapping[str, object]) -> str:
    lease_status = status.lease.status if status.lease is not None else "none"
    conflicts = ",".join(status.conflicts) if status.conflicts else "none"
    return "\n".join(
        (
            "state: %s" % status.state,
            "relation: %s" % status.relation,
            "config: %s" % ("present" if status.config_exists else "absent"),
            "lease: %s" % lease_status,
            "conflicts: %s" % conflicts,
            "service health: not_checked",
            "runtime state: %s" % runtime["state"],
            "runtime confidence: %s" % runtime["confidence"],
            "next action: %s" % _next_action(status.state),
        )
    )


def _format_result(result: IntegrationResult, runtime: Mapping[str, object]) -> str:
    conflicts = ",".join(result.conflicts) if result.conflicts else "none"
    return "\n".join(
        (
            "action: %s" % result.action,
            "state: %s" % result.state,
            "relation: %s" % result.relation,
            "conflicts: %s" % conflicts,
            "runtime state: %s" % runtime["state"],
            "runtime confidence: %s" % runtime["confidence"],
        )
    )


def _json_status(status: IntegrationStatus, runtime: Mapping[str, object]) -> dict:
    lease_status = status.lease.status if status.lease is not None else "none"
    return {
        "state": status.state,
        "relation": status.relation,
        "config_exists": status.config_exists,
        "lease_status": lease_status,
        "conflicts": list(status.conflicts),
        "service_health": "not_checked",
        "runtime": dict(runtime),
        "next_action": _next_action(status.state),
    }


def _json_result(result: IntegrationResult, runtime: Mapping[str, object]) -> dict:
    lease_status = result.lease.status if result.lease is not None else "none"
    return {
        "configuration": {
            "action": result.action,
            "state": result.state,
            "relation": result.relation,
            "lease_status": lease_status,
            "conflicts": list(result.conflicts),
        },
        "runtime": dict(runtime),
        "next_action": _next_action(result.state),
    }


def _manager_for(args: argparse.Namespace) -> IntegrationManager:
    paths = resolve_integration_paths(state_dir=args.state_dir)
    return IntegrationManager(
        paths.config_path,
        paths.lease_path,
        lock_path=paths.lock_path,
    )


def _run_doctor(args: argparse.Namespace) -> int:
    paths = resolve_integration_paths(state_dir=args.state_dir)
    status = IntegrationManager(
        paths.config_path, paths.lease_path, lock_path=paths.lock_path
    ).status()
    runtime = offline_runtime_snapshot(RuntimeRecoveryStore(paths.runtime_path).load())
    if args.json:
        print(json.dumps(_json_status(status, runtime), sort_keys=True))
    else:
        print(_format_status(status, runtime))
    return 0 if status.state in ("native", "active", "restored") else 1


def _run_restore(args: argparse.Namespace) -> int:
    paths = resolve_integration_paths(state_dir=args.state_dir)
    store = RuntimeRecoveryStore(paths.runtime_path)
    prior = store.load()
    result = IntegrationManager(
        paths.config_path, paths.lease_path, lock_path=paths.lock_path
    ).restore()
    if result.ok:
        store.save(
            RELOAD_REQUIRED,
            "native",
            result.relation,
            prior.expected_models if prior is not None else (),
            False,
            "Native configuration was restored offline; runtime was not checked",
        )
    runtime = offline_runtime_snapshot(store.load())
    if args.json:
        print(json.dumps(_json_result(result, runtime), sort_keys=True))
    else:
        print(_format_result(result, runtime))
    return 0 if result.ok else 1


def _run_serve(args: argparse.Namespace) -> int:
    # Keep the service import lazy so doctor/restore remain offline operations.
    from .tls_runtime import configure_system_trust

    configure_system_trust()
    from .server import serve

    serve(args.config, args.host, args.port, open_browser=args.open_browser)
    return 0


def _safe_error_message(error: BaseException) -> str:
    name = error.__class__.__name__
    if name == "LeaseError":
        return "integration lease is invalid or unreadable"
    if name == "SymlinkConfigError":
        return "symlink paths are not supported for Codex config or EMP state"
    if name == "LockTimeout":
        return "EMP integration lock is unavailable"
    if isinstance(error, ConfigError):
        if str(error) == "another EMP service owns this configuration":
            return str(error)
        return "EMP configuration is invalid"
    if isinstance(error, OSError):
        return "EMP integration state is not readable or writable"
    return "EMP integration operation failed"


def main(argv: Optional[Sequence[str]] = None) -> int:
    raw_args = list(sys.argv[1:] if argv is None else argv)
    if not raw_args and bool(getattr(sys, "frozen", False)):
        raw_args = [
            "serve",
            "--config",
            str(resolve_desktop_config_path()),
            "--open-browser",
        ]
    parser = build_parser()
    if not raw_args:
        parser.print_usage(sys.stderr)
        return 2
    args = parser.parse_args(raw_args)
    if args.command == "serve":
        try:
            return _run_serve(args)
        except (ConfigError, OSError) as error:
            print(_safe_error_message(error), file=sys.stderr)
            return 1
    if args.command == "doctor":
        try:
            return _run_doctor(args)
        except (IntegrationError, RuntimeSyncError, OSError) as error:
            print(_safe_error_message(error), file=sys.stderr)
            return 1
    if args.command == "restore":
        try:
            return _run_restore(args)
        except (IntegrationError, RuntimeSyncError, OSError) as error:
            print(_safe_error_message(error), file=sys.stderr)
            return 1
    parser.print_help()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
