"""Lease the Codex fields required by standalone Subscription web search."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Tuple

from tomlkit import document, dumps, parse, table
from tomlkit.exceptions import ParseError

from .integration import IntegrationError, atomic_write_text, _reject_symlink


LEASE_SCHEMA = "easy-multi-provider.search-lease"
LEASE_VERSION = 1
ROOT_FIELD = "web_search"
FEATURE_FIELD = "standalone_web_search"


class SearchIntegrationError(IntegrationError):
    """Raised when search fields cannot be changed without overwriting user state."""


def _state(present: bool, value: Any = None) -> Dict[str, Any]:
    return {"present": bool(present), "value": value if present else None}


def _validate_state(value: Any, label: str, expected: type) -> Dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {"present", "value"}:
        raise SearchIntegrationError("invalid search lease field: %s" % label)
    if not isinstance(value.get("present"), bool):
        raise SearchIntegrationError("invalid search lease presence: %s" % label)
    raw = value.get("value")
    if value["present"] and not isinstance(raw, expected):
        raise SearchIntegrationError("invalid search lease value: %s" % label)
    if not value["present"] and raw is not None:
        raise SearchIntegrationError("invalid search lease value: %s" % label)
    return {"present": value["present"], "value": raw}


def _unwrap(value: Any) -> Any:
    unwrap = getattr(value, "unwrap", None)
    return unwrap() if callable(unwrap) else value


class SearchFeatureManager:
    """Compare-and-restore two Codex TOML fields under the integration lock."""

    def __init__(self, config_path: Path, lease_path: Path) -> None:
        self.config_path = Path(config_path)
        self.lease_path = Path(lease_path)

    def _read_config(self) -> Tuple[Any, bool]:
        _reject_symlink(self.config_path, "Codex config")
        if not self.config_path.exists():
            return document(), False
        try:
            return parse(self.config_path.read_text(encoding="utf-8")), True
        except (OSError, ParseError, TypeError, ValueError) as exc:
            raise SearchIntegrationError("unable to parse Codex TOML config") from exc

    @staticmethod
    def _states(config: Any) -> Dict[str, Any]:
        if ROOT_FIELD in config:
            root_value = _unwrap(config[ROOT_FIELD])
            if not isinstance(root_value, str):
                raise SearchIntegrationError("Codex web_search must be a string")
            root = _state(True, root_value)
        else:
            root = _state(False)

        features_present = "features" in config
        features = config.get("features")
        if features_present and not hasattr(features, "get"):
            raise SearchIntegrationError("Codex features must be a table")
        if features is not None and FEATURE_FIELD in features:
            feature_value = _unwrap(features[FEATURE_FIELD])
            if not isinstance(feature_value, bool):
                raise SearchIntegrationError(
                    "Codex standalone_web_search must be boolean"
                )
            feature = _state(True, feature_value)
        else:
            feature = _state(False)
        return {
            ROOT_FIELD: root,
            "features.%s" % FEATURE_FIELD: feature,
            "features_table_present": features_present,
        }

    @staticmethod
    def _apply_states(config: Any, states: Mapping[str, Any]) -> None:
        root = states[ROOT_FIELD]
        if root["present"]:
            config[ROOT_FIELD] = root["value"]
        elif ROOT_FIELD in config:
            del config[ROOT_FIELD]

        dotted = "features.%s" % FEATURE_FIELD
        feature = states[dotted]
        features = config.get("features")
        if feature["present"]:
            if features is None:
                features = table()
                config["features"] = features
            features[FEATURE_FIELD] = feature["value"]
        elif features is not None and FEATURE_FIELD in features:
            del features[FEATURE_FIELD]
        if (
            not states.get("features_table_present", False)
            and "features" in config
            and not list(config["features"].keys())
        ):
            del config["features"]

    def _read_lease(self) -> Optional[Dict[str, Any]]:
        _reject_symlink(self.lease_path, "search integration lease")
        if not self.lease_path.exists():
            return None
        try:
            raw = json.loads(self.lease_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise SearchIntegrationError("unable to read search integration lease") from exc
        if not isinstance(raw, dict) or raw.get("schema") != LEASE_SCHEMA or raw.get("version") != LEASE_VERSION:
            raise SearchIntegrationError("unsupported search integration lease")
        if raw.get("config_path") != str(self.config_path.resolve()):
            raise SearchIntegrationError("search integration lease targets another config")
        if raw.get("status") not in {"active", "restored"}:
            raise SearchIntegrationError("invalid search integration lease status")
        for side in ("original", "applied"):
            states = raw.get(side)
            if not isinstance(states, dict) or not isinstance(
                states.get("features_table_present"), bool
            ):
                raise SearchIntegrationError("invalid search integration lease")
            _validate_state(states.get(ROOT_FIELD), ROOT_FIELD, str)
            _validate_state(
                states.get("features.%s" % FEATURE_FIELD), FEATURE_FIELD, bool
            )
        return raw

    def _write_lease(self, value: Mapping[str, Any]) -> None:
        self.lease_path.parent.mkdir(parents=True, exist_ok=True)
        atomic_write_text(
            self.lease_path,
            json.dumps(value, indent=2, sort_keys=True) + "\n",
            mode=0o600,
        )

    def _write_config(self, config: Any) -> None:
        atomic_write_text(self.config_path, dumps(config), mode=0o600)

    def apply(self, enabled: bool) -> None:
        """Apply or restore search fields. Caller must hold the operation lock."""

        if not enabled:
            self.restore()
            return
        config, _ = self._read_config()
        current = self._states(config)
        desired = {
            ROOT_FIELD: _state(True, "live"),
            "features.%s" % FEATURE_FIELD: _state(True, True),
            "features_table_present": current["features_table_present"],
        }
        lease = self._read_lease()
        if lease is not None and lease["status"] == "active":
            if current == lease["applied"]:
                return
            if current != lease["original"]:
                raise SearchIntegrationError(
                    "Codex search fields changed outside EMP"
                )
        original = current
        self._apply_states(config, desired)
        self._write_config(config)
        verified, _ = self._read_config()
        applied = self._states(verified)
        if applied[ROOT_FIELD] != desired[ROOT_FIELD] or applied[
            "features.%s" % FEATURE_FIELD
        ] != desired["features.%s" % FEATURE_FIELD]:
            raise SearchIntegrationError("unable to apply Codex standalone search")
        self._write_lease(
            {
                "schema": LEASE_SCHEMA,
                "version": LEASE_VERSION,
                "config_path": str(self.config_path.resolve()),
                "status": "active",
                "original": original,
                "applied": applied,
            }
        )

    def restore(self) -> None:
        """Restore the pre-EMP values if an active search lease exists."""

        lease = self._read_lease()
        if lease is None or lease["status"] == "restored":
            return
        config, _ = self._read_config()
        current = self._states(config)
        if current == lease["original"]:
            lease["status"] = "restored"
            self._write_lease(lease)
            return
        if current != lease["applied"]:
            raise SearchIntegrationError("Codex search fields changed outside EMP")
        self._apply_states(config, lease["original"])
        self._write_config(config)
        verified, _ = self._read_config()
        if self._states(verified) != lease["original"]:
            raise SearchIntegrationError("unable to restore Codex search fields")
        lease["status"] = "restored"
        self._write_lease(lease)
