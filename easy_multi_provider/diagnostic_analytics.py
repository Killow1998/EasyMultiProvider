"""User-facing aggregates derived from privacy-safe route observations."""

from __future__ import annotations

from collections import Counter
from statistics import median
from typing import Any, Dict, Iterable, Mapping


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


def summarize_route_observations(records, max_models: int = 5) -> Dict[str, Any]:
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

    recent_keys = []
    for item in reversed(relevant):
        model_id = item.get("model_id")
        speed_mode = item.get("speed_mode") or "unknown"
        ttft = _number(item.get("ttft_ms"))
        tps = _number(item.get("tokens_per_second"))
        if (
            not isinstance(model_id, str)
            or not model_id
            or model_id.startswith("codex-auto-")
            or speed_mode not in {"standard", "fast", "unknown"}
            or not ((ttft is not None and ttft > 0) or (tps is not None and tps > 0))
        ):
            continue
        key = (model_id, speed_mode)
        if key not in recent_keys:
            recent_keys.append(key)
        if len(recent_keys) >= max(1, int(max_models)):
            break

    models = []
    for model_id, speed_mode in recent_keys:
        calls = [
            item
            for item in relevant
            if item.get("model_id") == model_id
            and (item.get("speed_mode") or "unknown") == speed_mode
        ]
        ttft_ms, ttft_samples = _metric(item.get("ttft_ms") for item in calls)
        tokens_per_second, tps_samples = _metric(
            item.get("tokens_per_second") for item in calls
        )
        if ttft_samples == 0 and tps_samples == 0:
            continue
        models.append(
            {
                "model_id": model_id,
                "speed_mode": speed_mode,
                "call_count": len(calls),
                "ttft_ms": ttft_ms,
                "ttft_samples": ttft_samples,
                "tokens_per_second": tokens_per_second,
                "tps_samples": tps_samples,
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
        "models": models,
    }
