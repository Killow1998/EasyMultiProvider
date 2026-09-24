"""Request-local account selection for Codex automatic approval reviews."""

from __future__ import annotations

import math
import time
from typing import Any, List, Mapping, Optional, Tuple

from .accounts import NATIVE_ACCOUNT_ID


AUTO_REVIEW_MODEL_ID = "codex-auto-review"


def is_auto_review_model(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    return value == AUTO_REVIEW_MODEL_ID or value.endswith(
        "/" + AUTO_REVIEW_MODEL_ID
    )


def quota_headroom(quota: Any) -> Optional[float]:
    """Return the tightest reported remaining percentage, or unknown."""

    if not isinstance(quota, Mapping):
        return None
    credits = quota.get("credits")
    if isinstance(credits, Mapping):
        if credits.get("spend_control_reached") is True:
            return 0.0
        individual = credits.get("individual_limit")
        if isinstance(individual, Mapping):
            remaining = individual.get("remaining_percent")
            if (
                isinstance(remaining, (int, float))
                and not isinstance(remaining, bool)
                and math.isfinite(float(remaining))
            ):
                if float(remaining) <= 0:
                    return 0.0

    limits = quota.get("rate_limits")
    if not isinstance(limits, Mapping):
        return None
    remaining_values = []
    for key in ("primary", "secondary"):
        window = limits.get(key)
        if not isinstance(window, Mapping):
            continue
        used = window.get("usedPercent", window.get("used_percent"))
        if (
            isinstance(used, (int, float))
            and not isinstance(used, bool)
            and math.isfinite(float(used))
        ):
            remaining_values.append(max(0.0, min(100.0, 100.0 - float(used))))
    return min(remaining_values) if remaining_values else None


def automatic_review_candidates(
    config: Mapping[str, Any],
    native_quota: Any,
    native_available: bool,
    cooldowns: Optional[Mapping[str, float]] = None,
    *,
    now: Optional[float] = None,
) -> List[str]:
    """Order usable review accounts without persisting a user choice."""

    current = time.monotonic() if now is None else float(now)
    blocked_until = cooldowns or {}
    candidates = []
    if native_available:
        candidates.append(
            (NATIVE_ACCOUNT_ID, True, quota_headroom(native_quota), 0)
        )
    for index, account in enumerate(config.get("accounts", [])):
        if (
            not isinstance(account, Mapping)
            or not account.get("id")
            or account.get("enabled", True) is False
            or not account.get("auth_file")
            or account.get("credential_status") == "invalid"
        ):
            continue
        candidates.append(
            (
                str(account["id"]),
                False,
                quota_headroom(account.get("quota")),
                index,
            )
        )
    if not candidates:
        return []

    def cooling(item: Tuple[str, bool, Optional[float], int]) -> bool:
        return float(blocked_until.get(item[0], 0.0) or 0.0) > current

    healthy = [
        item
        for item in candidates
        if not cooling(item) and (item[2] is None or item[2] > 0)
    ]
    if not healthy:
        healthy = [
            item for item in candidates if item[2] is None or item[2] > 0
        ]
    if not healthy:
        healthy = candidates

    native = [item for item in healthy if item[1]]
    imported = [item for item in healthy if not item[1]]
    imported.sort(
        key=lambda item: (
            item[2] is None,
            -(item[2] if item[2] is not None else 0.0),
            item[3],
        )
    )
    return [item[0] for item in native + imported]
