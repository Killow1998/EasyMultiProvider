"""Incremental, read-only Codex rollout accounting. Never retain message bodies."""

from __future__ import annotations

import hashlib
import json
import threading
import time
from datetime import datetime
from pathlib import Path

from .performance import token_count
from .usage_ledger import TOKEN_FIELDS, usage_identity

_FIELDS = dict(zip(TOKEN_FIELDS, ("input_tokens", "output_tokens", "cached_input_tokens",
                               "cache_write_input_tokens", "cache_write_1h_tokens", "reasoning_output_tokens")))


def _timestamp(value):
    try:
        parsed = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
        return parsed.timestamp() if parsed.tzinfo is not None else None
    except (ValueError, OverflowError, OSError):
        return None


def _counts(value):
    if not isinstance(value, dict):
        return None
    result = {field: value.get(field) for field in _FIELDS.values()}
    # Cumulative counters can exceed the per-request size limit.
    return {key: number for key, number in result.items()
            if type(number) is int and 0 <= number < 2**63}


def history_routes(config):
    """Current routes suggest a category, not the identity of an old login."""
    providers = {item["id"]: item for item in config.get("providers", [])}
    routes = {}
    for account in config.get("accounts", []):
        if account.get("prefix"):
            routes[str(account["prefix"]) + "/"] = ("subscription", "")
    for model in config.get("models", []):
        provider = providers.get(model.get("provider"))
        if provider:
            identity = usage_identity(provider, model)
            routes[model["id"]] = (identity["usage_category"], identity["upstream_model"])
    return routes


def parse_record(record, state, routes):
    """Advance a small numeric cursor and return at most one usage observation."""
    payload = record.get("payload")
    if not isinstance(payload, dict):
        return None
    kind = record.get("type")
    if kind == "session_meta":
        state.update(thread=str(payload.get("id") or payload.get("session_id") or ""),
                     provider=str(payload.get("model_provider") or ""),
                     fork=str(payload.get("forked_from_id") or ""))
        return None
    if kind == "turn_context":
        state["model"] = str(payload.get("model") or "")[:256]
        state["turn"] = str(payload.get("turn_id") or state.get("turn") or "")
        state["tier"] = str(payload.get("service_tier") or "default")
        return None
    if kind != "event_msg":
        return None
    if payload.get("type") == "task_started":
        state["turn"] = str(payload.get("turn_id") or "")
        return None
    if payload.get("type") != "token_count" or not isinstance(payload.get("info"), dict):
        return None
    info = payload["info"]
    totals, last = _counts(info.get("total_token_usage")), _counts(info.get("last_token_usage"))
    previous = state.get("totals")
    if totals and totals == previous:
        return None  # rate-limit-only notifications repeat the last usage
    state["totals"] = totals
    usage = last
    issue = None
    if not usage and totals:
        if previous and all(totals.get(key, 0) >= previous.get(key, 0) for key in totals):
            usage = {key: count - previous.get(key, 0) for key, count in totals.items()}
        elif not state.get("fork"):
            usage = totals
        # A cumulative-only observation may contain several model calls; long
        # context pricing cannot safely be applied to it as a single request.
        issue = "aggregate_usage"
    if not usage:
        return None
    model = state.get("model", "")
    observed_at = _timestamp(record.get("timestamp"))
    if not model or observed_at is None:
        state["unattributed"] = state.get("unattributed", 0) + 1
        return None
    category, upstream = "unknown", model
    if "/" not in model and state.get("provider") == "openai" and model.startswith(("gpt-", "codex-")):
        category = "native"
    elif model in routes:
        category, upstream = routes[model]
    elif "/" in model and model.split("/", 1)[0] + "/" in routes:
        category = routes[model.split("/", 1)[0] + "/"][0]
        upstream = model.split("/", 1)[1]
    owner = "history:" + (model.split("/", 1)[0] if "/" in model else state.get("provider", "unknown"))
    event = {field: token_count(usage[source]) for field, source in _FIELDS.items() if source in usage}
    raw_last = info.get("last_token_usage") or {}
    total = raw_last.get("total_tokens")
    if (type(total) is int and event.get("input_tokens") is not None and event.get("output_tokens") is not None
            and total != event["input_tokens"] + event["output_tokens"]):
        issue = "inconsistent_usage"
    if (event.get("reasoning_tokens") or 0) > (event.get("output_tokens") or 0):
        issue = "inconsistent_usage"
    identity = [state.get("turn") or state.get("thread"), record.get("timestamp"), model, usage]
    row_id = "history:" + hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
    event.update(observation_id=row_id, origin="history", usage_turn=state.get("turn", ""),
                 route_model=model, route="responses", usage_category=category, usage_owner=owner,
                 upstream_model=upstream, service_tier=state.get("tier", "default"), usage_issue=issue)
    return event, observed_at


class UsageHistoryScanner:
    """One worker; file checkpoints commit with their observations in SQLite."""

    def __init__(self, ledger, codex_home, config):
        self.ledger, self.codex_home, self.config = ledger, codex_home, config
        self._stop = threading.Event()
        self._wake = threading.Event()
        self._scan_lock = threading.Lock()
        self._thread = None
        self.status = {"running": False, "files": 0, "updated": 0, "errors": 0, "last_scan_at": None}

    def scan(self):
        if not self._scan_lock.acquire(blocking=False):
            return
        self.status = {**self.status, "running": True, "queued": False, "files": 0, "updated": 0, "errors": 0}
        try:
            home = Path(self.codex_home()).resolve()
            routes = history_routes(self.config())
            for directory in (home / "sessions", home / "archived_sessions"):
                if not directory.is_dir() or directory.is_symlink():
                    continue
                for path in directory.rglob("*.jsonl"):
                    if self._stop.is_set():
                        return
                    self.status["files"] += 1
                    try:
                        if path.is_symlink():
                            continue
                        path.resolve().relative_to(home)
                        if self._scan_file(path, routes):
                            self.status["updated"] += 1
                    except (OSError, ValueError, TypeError):
                        self.status["errors"] += 1
            self.status["last_scan_at"] = time.time()
        except Exception as exc:
            self.status["errors"] += 1
            self.ledger.journal.event("warning", "usage_history_scan_failed", exception_type=type(exc).__name__)
        finally:
            self.status["running"] = False
            self._scan_lock.release()

    def _scan_file(self, path, routes):
        stat = path.stat()
        saved = self.ledger.history_checkpoint(str(path))
        # ctime changes on append on Unix. The inode plus a prefix digest
        # distinguishes replacements while allowing append-only progress.
        with path.open("rb") as stream:
            prefix = hashlib.sha256(stream.read(min(256, stat.st_size))).hexdigest()
            identity = [stat.st_dev, stat.st_ino, prefix]
            def tail_digest(offset):
                position = stream.tell()
                stream.seek(max(0, offset - 256))
                digest = hashlib.sha256(stream.read(min(256, offset))).hexdigest()
                stream.seek(position)
                return digest

            if (saved and saved["identity"] == identity and stat.st_size >= saved["offset"]
                    and saved.get("tail") == tail_digest(saved["offset"])):
                if stat.st_size == saved["size"] and stat.st_mtime_ns == saved["mtime"]:
                    return False
                state, offset = saved["state"], saved["offset"]
            else:
                state, offset = {}, 0
            stream.seek(offset)
            events = []
            while not self._stop.is_set():
                line = stream.readline()
                if not line or not line.endswith(b"\n"):
                    break  # leave an incomplete final JSON object for the next scan
                offset = stream.tell()
                if not any(marker in line for marker in (b'"session_meta"', b'"turn_context"',
                                                         b'"token_count"', b'"task_started"')):
                    continue
                try:
                    record = json.loads(line)
                    parsed = parse_record(record, state, routes) if isinstance(record, dict) else None
                    if parsed:
                        events.append(parsed)
                except (ValueError, TypeError, OverflowError):
                    state["malformed"] = state.get("malformed", 0) + 1
                if len(events) >= 200:
                    self.ledger.record_history(events, str(path), {"identity": identity, "offset": offset,
                        "size": offset, "mtime": stat.st_mtime_ns, "state": state, "tail": tail_digest(offset)})
                    events.clear()
            self.ledger.record_history(events, str(path), {"identity": identity, "offset": offset,
                "size": stat.st_size, "mtime": stat.st_mtime_ns, "state": state, "tail": tail_digest(offset)})
        return True

    def _run(self):
        while not self._stop.is_set():
            self._wake.clear()
            self.scan()
            self._wake.wait(60)

    def start(self):
        if self._thread is None:
            self._thread = threading.Thread(target=self._run, daemon=True, name="emp-usage-history")
            self._thread.start()

    def refresh(self):
        self.status["queued"] = True
        self.start()
        self._wake.set()

    def stop(self):
        self._stop.set()
        self._wake.set()
        if self._thread is not None:
            self._thread.join(timeout=2)
