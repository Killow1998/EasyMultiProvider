"""User-facing aggregates derived from privacy-safe route observations."""

from __future__ import annotations

from collections import Counter
from datetime import datetime, timedelta, timezone
from statistics import median
from typing import Any, Dict, Iterable, Mapping, Optional

from .performance import PERFORMANCE_SCHEMA


_PERFORMANCE_WINDOW_CALLS = 20
_PERFORMANCE_WINDOW_DAYS = 7


_CLIENT_ENDINGS = frozenset({
    "client_disconnect",
    "client_cancelled",
    "client_websocket_close",
})


def _number(value: Any):
    if isinstance(value, bool):
        return None
    try:
        result = float(value)
    except (TypeError, ValueError):
        return None
    if result != result or result < 0 or result in (float("inf"), float("-inf")):
        return None
    return result


def _rate(count: int, total: int) -> float:
    return round(count * 100.0 / total, 1) if total else 0.0


def _metric(values: Iterable[Any]):
    measured = [
        value
        for value in (_number(item) for item in values)
        if value is not None and value > 0
    ]
    return (
        (round(float(median(measured)), 1), len(measured))
        if measured
        else (None, 0)
    )


def _observed_at(value: Any) -> Optional[datetime]:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _successful_speed_sample(item: Mapping[str, Any]) -> bool:
    status = item.get("status")
    return (
        item.get("performance_schema") == PERFORMANCE_SCHEMA
        and item.get("error_class") == "none"
        and isinstance(status, int)
        and not isinstance(status, bool)
        and 200 <= status < 300
        and (
            (_number(item.get("ttft_ms")) or 0) > 0
            or (_number(item.get("tokens_per_second")) or 0) > 0
        )
    )


def _change_percent(current: Optional[float], previous: Optional[float], lower_is_better: bool):
    if current is None or previous is None or previous <= 0:
        return None
    raw = (previous - current) if lower_is_better else (current - previous)
    return round(raw * 100.0 / previous, 1)


def summarize_route_observations(
    records,
    max_models: int = 5,
    *,
    now: Optional[datetime] = None,
) -> Dict[str, Any]:
    """Summarize recent health and median model speed without inventing data."""

    material = [dict(item) for item in records if isinstance(item, Mapping)]
    relevant = [item for item in material if item.get("route") == "responses"]
    cancellations = [
        item for item in relevant if item.get("error_class") in _CLIENT_ENDINGS
    ]
    health_records = [
        item for item in relevant if item.get("error_class") not in _CLIENT_ENDINGS
    ]
    successes = [
        item
        for item in health_records
        if item.get("error_class") == "none"
        and isinstance(item.get("status"), int)
        and 200 <= item["status"] < 300
    ]
    status_429 = sum(item.get("status") == 429 for item in health_records)
    status_502 = sum(item.get("status") == 502 for item in health_records)
    status_503 = sum(item.get("status") == 503 for item in health_records)
    status_504 = sum(item.get("status") == 504 for item in health_records)
    local_capacity = sum(
        item.get("error_class") == "upstream_capacity" for item in health_records
    )
    total = len(health_records)
    failures = max(0, total - len(successes))
    failure_classes = Counter(
        str(item.get("error_class") or "unknown")
        for item in health_records
        if not (
            item.get("error_class") == "none"
            and isinstance(item.get("status"), int)
            and 200 <= item["status"] < 300
        )
    )

    reference = now or datetime.now(timezone.utc)
    if reference.tzinfo is None:
        reference = reference.replace(tzinfo=timezone.utc)
    else:
        reference = reference.astimezone(timezone.utc)
    cutoff = reference - timedelta(days=_PERFORMANCE_WINDOW_DAYS)
    performance_records = [
        item
        for item in relevant
        if _successful_speed_sample(item)
        and (_observed_at(item.get("observed_at")) or reference) >= cutoff
    ]

    recent_keys = []
    for item in reversed(performance_records):
        model_id = item.get("model_id")
        speed_mode = item.get("speed_mode") or "unknown"
        if (
            not isinstance(model_id, str)
            or not model_id
            or model_id.startswith("codex-auto-")
            or speed_mode not in {"standard", "fast", "unknown"}
        ):
            continue
        key = (model_id, speed_mode)
        if key not in recent_keys:
            recent_keys.append(key)
        if len(recent_keys) >= max(1, int(max_models)):
            break

    models = []
    for model_id, speed_mode in recent_keys:
        history = [
            item
            for item in performance_records
            if item.get("model_id") == model_id
            and (item.get("speed_mode") or "unknown") == speed_mode
        ]
        calls = history[-_PERFORMANCE_WINDOW_CALLS:]
        previous_calls = history[-2 * _PERFORMANCE_WINDOW_CALLS:-_PERFORMANCE_WINDOW_CALLS]
        ttft_ms, ttft_samples = _metric(item.get("ttft_ms") for item in calls)
        tokens_per_second, tps_samples = _metric(
            item.get("tokens_per_second") for item in calls
        )
        previous_ttft_ms, previous_ttft_samples = _metric(
            item.get("ttft_ms") for item in previous_calls
        )
        previous_tokens_per_second, previous_tps_samples = _metric(
            item.get("tokens_per_second") for item in previous_calls
        )
        if ttft_samples == 0 and tps_samples == 0:
            continue
        models.append(
            {
                "model_id": model_id,
                "speed_mode": speed_mode,
                "call_count": len(calls),
                "retained_call_count": len(history),
                "ttft_ms": ttft_ms,
                "ttft_samples": ttft_samples,
                "previous_ttft_ms": previous_ttft_ms,
                "previous_ttft_samples": previous_ttft_samples,
                "ttft_change_percent": (
                    _change_percent(ttft_ms, previous_ttft_ms, True)
                    if min(ttft_samples, previous_ttft_samples) >= 3
                    else None
                ),
                "tokens_per_second": tokens_per_second,
                "tps_samples": tps_samples,
                "previous_tokens_per_second": previous_tokens_per_second,
                "previous_tps_samples": previous_tps_samples,
                "tps_change_percent": (
                    _change_percent(
                        tokens_per_second, previous_tokens_per_second, False
                    )
                    if min(tps_samples, previous_tps_samples) >= 3
                    else None
                ),
                "last_seen": calls[-1].get("observed_at") if calls else None,
            }
        )

    return {
        "health": {
            "sample_count": total,
            "success_count": len(successes),
            "failure_count": failures,
            "success_rate": _rate(len(successes), total),
            "status_429_count": status_429,
            "status_429_rate": _rate(status_429, total),
            "status_502_count": status_502,
            "status_502_rate": _rate(status_502, total),
            "status_503_count": status_503,
            "status_503_rate": _rate(status_503, total),
            "status_504_count": status_504,
            "status_504_rate": _rate(status_504, total),
            "local_capacity_count": local_capacity,
            "local_capacity_rate": _rate(local_capacity, total),
            "cancelled_count": len(cancellations),
            "failure_classes": [
                {
                    "error_class": error_class,
                    "count": count,
                    "rate": _rate(count, total),
                }
                for error_class, count in failure_classes.most_common(6)
            ],
        },
        "performance_window": {
            "calls": _PERFORMANCE_WINDOW_CALLS,
            "days": _PERFORMANCE_WINDOW_DAYS,
        },
        "models": models,
    }
