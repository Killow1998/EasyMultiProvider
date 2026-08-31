"""Codex integration status and target projections for the HTTP control plane."""

from __future__ import annotations

from typing import Any, Dict, Optional, Tuple

from .codex_runtime import (
    NOT_CHECKED,
    RELOAD_REQUIRED,
    STOP_FAILED,
    STOPPED_WAITING_FOR_START,
    UNSUPPORTED,
    VERIFICATION_FAILED,
)
from .integration import IntegrationError, IntegrationResult, ServiceNotReady


def _next_action(state: str, service_health: str) -> str:
    if state in ("prepared", "restoring"):
        return "restore"
    if state == "active":
        return "none" if service_health == "ready" else "confirm service health or restore"
    if state == "conflict":
        return "restore"
    if state == "native":
        return "enable default Codex"
    return "none"


def integration_summary(
    state: Any,
    result: Optional[IntegrationResult] = None,
) -> Dict[str, Any]:
    observed = state.integration_status()
    service_health = "ready" if state.service_ready() else "not_ready"
    summary_state = observed.state
    relation = observed.relation
    conflicts = list(observed.conflicts)
    startup_conflicts = state.startup_conflicts()
    if startup_conflicts:
        summary_state = "conflict"
        conflicts = list(startup_conflicts)
    if result is not None and not result.ok:
        summary_state = "conflict"
        relation = result.relation
        conflicts = list(result.conflicts)
    lease_status = observed.lease.status if observed.lease is not None else "none"
    runtime = state.runtime_sync_snapshot()
    runtime_state = runtime.get("state", NOT_CHECKED)
    runtime_action_required = runtime_state in {
        RELOAD_REQUIRED,
        STOP_FAILED,
        VERIFICATION_FAILED,
        UNSUPPORTED,
    }
    if runtime_state == RELOAD_REQUIRED:
        next_action = "wait for shared backend owner restart"
    elif runtime_state == STOPPED_WAITING_FOR_START:
        next_action = "wait for shared backend owner start"
    elif runtime_action_required:
        next_action = "check shared Codex backend"
    else:
        next_action = _next_action(summary_state, service_health)
    if summary_state == "active":
        configuration_state = "emp_applied"
    elif summary_state in ("native", "restored"):
        configuration_state = "native"
    else:
        configuration_state = summary_state
    return {
        "codex_compatibility": state.codex_compatibility_snapshot(),
        "configuration": {
            "state": configuration_state,
            "relation": relation,
            "config_exists": observed.config_exists,
            "lease_status": lease_status,
            "conflicts": conflicts,
        },
        "runtime": {
            "state": runtime_state,
            "target": runtime.get("target", "native"),
            "verified": bool(runtime.get("verified", False)),
            "confidence": runtime.get("confidence", "not_checked"),
            "action_required": runtime_action_required,
            "detail": runtime.get("detail", ""),
            "last_known": runtime.get("last_known"),
        },
        "service_health": service_health,
        "next_action": next_action,
    }


def integration_error_message(error: BaseException) -> str:
    if isinstance(error, ServiceNotReady):
        return "EMP service is not ready"
    if isinstance(error, IntegrationError):
        return "integration state is unavailable"
    return "integration operation failed"


def bound_base_url(server_address: Tuple[Any, ...]) -> str:
    host = str(server_address[0])
    if host in ("0.0.0.0", "::"):
        host = "127.0.0.1"
    elif ":" in host and not host.startswith("["):
        host = "[%s]" % host
    return "http://%s:%d/v1" % (host, int(server_address[1]))


def integration_target(
    state: Any,
    server_address: Tuple[Any, ...],
) -> Tuple[str, str]:
    return (
        bound_base_url(server_address),
        str(state.integration_catalog_path.resolve()),
    )


def startup_target_conflict(
    state: Any,
    base_url: str,
    catalog_path: str,
) -> Optional[IntegrationResult]:
    status = state.integration_status()
    lease = status.lease
    if lease is None or lease.status == "restored" or status.relation != "applied":
        return None
    conflicts = []
    if lease.fields["openai_base_url"].applied.value != base_url:
        conflicts.append("listener_mismatch")
    if lease.fields["model_catalog_json"].applied.value != catalog_path:
        conflicts.append("catalog_mismatch")
    if not conflicts:
        return None
    names = tuple(conflicts)
    state.set_startup_conflicts(names)
    return IntegrationResult(
        "conflict",
        "conflict",
        status.relation,
        status.fields,
        lease,
        names,
    )
