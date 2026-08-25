"""Locate a Codex rollout from the read-only ``threads`` projection."""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Optional
from urllib.parse import quote

from .models import (
    HistoryAnchor,
    HistoryError,
    HistoryInputError,
    HistoryMismatchError,
    HistorySnapshot,
    HistoryUnavailableError,
    HistoryUnsupportedError,
)


def _model(value: Any) -> Optional[str]:
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value if value and len(value) <= 512 else None


@dataclass(frozen=True)
class SQLiteLocation:
    """Content-free rollout location for one Codex thread."""

    database_path: Path
    thread_id: str
    history_mode: str
    rollout_path: Path
    source_model: Optional[str] = None
    # The current state_5 schema does not expose these values. Rollout
    # ordinals remain authoritative until Codex adds a projection index.
    projection_ordinal: Optional[int] = None
    projection_offset: Optional[int] = None

    def __post_init__(self) -> None:
        if self.history_mode not in ("legacy", "paginated"):
            raise HistoryUnsupportedError("invalid_history_mode", source="sqlite")
        if not isinstance(self.thread_id, str) or not self.thread_id:
            raise HistoryInputError("invalid_thread_id")
        object.__setattr__(self, "database_path", Path(self.database_path))
        object.__setattr__(self, "rollout_path", Path(self.rollout_path))
        object.__setattr__(self, "source_model", _model(self.source_model))

    def __repr__(self) -> str:
        return "SQLiteLocation(thread_id=%r, history_mode=%r, source_model=%r)" % (
            self.thread_id,
            self.history_mode,
            self.source_model,
        )


class SQLiteReader:
    """Read only the Codex-owned thread locator; never write Codex state."""

    def __init__(self, database_path: Path, *, rollout_reader: Any = None) -> None:
        if database_path is None:
            raise HistoryInputError("database_source_missing")
        self.database_path = Path(database_path).expanduser()
        self.rollout_reader = rollout_reader

    def _connect(self) -> sqlite3.Connection:
        database_path = self.database_path.resolve()
        if not database_path.is_file():
            raise HistoryUnavailableError("database_missing", source="sqlite")
        uri = "file:%s?mode=ro" % quote(str(database_path), safe="/")
        try:
            connection = sqlite3.connect(uri, uri=True)
        except sqlite3.Error:
            raise HistoryUnavailableError("database_unavailable", source="sqlite") from None
        connection.row_factory = sqlite3.Row
        return connection

    def locate(self, anchor: HistoryAnchor) -> SQLiteLocation:
        if not isinstance(anchor, HistoryAnchor):
            raise HistoryInputError("invalid_anchor")
        if not anchor.thread_id:
            raise HistoryUnavailableError("thread_identity_missing", source="sqlite")
        connection = self._connect()
        try:
            columns = {
                row[1]
                for row in connection.execute('PRAGMA table_info("threads")').fetchall()
                if len(row) > 1 and isinstance(row[1], str)
            }
            if not {"id", "rollout_path"}.issubset(columns):
                raise HistoryUnsupportedError("threads_schema_unsupported", source="sqlite")
            mode_expression = '"history_mode"' if "history_mode" in columns else "'legacy'"
            model_expression = '"model"' if "model" in columns else "NULL"
            rows = connection.execute(
                'SELECT "id", "rollout_path", %s AS history_mode, %s AS model '
                'FROM "threads" WHERE "id" = ? LIMIT 2'
                % (mode_expression, model_expression),
                (anchor.thread_id,),
            ).fetchall()
        except HistoryError:
            raise
        except sqlite3.Error:
            raise HistoryUnavailableError("database_unavailable", source="sqlite") from None
        finally:
            connection.close()

        if not rows:
            raise HistoryUnavailableError("thread_missing", source="sqlite")
        if len(rows) != 1:
            raise HistoryMismatchError("thread_identity_conflict", source="sqlite")
        row = rows[0]
        history_mode = row["history_mode"] or "legacy"
        if history_mode not in ("legacy", "paginated"):
            raise HistoryUnsupportedError("invalid_history_mode", source="sqlite")
        raw_path = row["rollout_path"]
        if not isinstance(raw_path, str) or not raw_path.strip():
            raise HistoryUnavailableError("rollout_path_missing", source="sqlite")
        rollout_path = Path(raw_path.strip()).expanduser()
        if not rollout_path.is_absolute():
            rollout_path = self.database_path.resolve().parent / rollout_path
        return SQLiteLocation(
            database_path=self.database_path.resolve(),
            thread_id=anchor.thread_id,
            history_mode=history_mode,
            rollout_path=rollout_path.resolve(),
            source_model=_model(row["model"]),
        )

    def read_visible_history(self, anchor: HistoryAnchor) -> HistorySnapshot:
        location = self.locate(anchor)
        reader = self.rollout_reader
        if reader is None:
            from .rollout import RolloutReader

            reader = RolloutReader(
                location.rollout_path,
                history_mode=location.history_mode,
                source_model=location.source_model,
            )
        target = getattr(reader, "read_visible_history", None)
        if not callable(target):
            raise HistoryUnsupportedError("invalid_rollout_reader", source="sqlite")
        try:
            snapshot = target(anchor, location=location)
        except HistoryError:
            raise
        except Exception:
            raise HistoryUnavailableError("rollout_unavailable", source="sqlite") from None
        if not isinstance(snapshot, HistorySnapshot):
            raise HistoryUnsupportedError("invalid_rollout_snapshot", source="sqlite")
        if snapshot.thread_id != anchor.thread_id:
            raise HistoryMismatchError("thread_mismatch", source="sqlite")
        if (
            snapshot.source_model is None
            and location.source_model is not None
            and anchor.turn_id is None
        ):
            snapshot = replace(snapshot, source_model=location.source_model)
        return snapshot


__all__ = ["SQLiteLocation", "SQLiteReader"]
