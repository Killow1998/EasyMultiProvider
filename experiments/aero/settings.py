import json
import os
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parent
DEFAULT_RUNTIME = (
    Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state")) / "aero"
)


def load_config(path):
    path = path.expanduser().resolve()
    config = json.loads(path.read_text())
    if not isinstance(config, dict):
        raise ValueError("configuration must be an object")
    endpoint = config.get("upstream", "")
    if not isinstance(endpoint, str) or urlsplit(endpoint).scheme not in (
        "http",
        "https",
    ):
        raise ValueError("upstream must be an HTTP or HTTPS EMP API base URL")
    if not urlsplit(endpoint).hostname:
        raise ValueError("upstream must include a host")
    interval = config.get("review_interval", 30)
    if type(interval) is not int or interval < 1:
        raise ValueError("review_interval must be a positive integer")
    config["review_interval"] = interval
    models = config.get("models")
    if not isinstance(models, dict) or not models:
        raise ValueError("models must contain at least one aero profile")
    for name, profile in models.items():
        if not name.startswith("aero/") or not isinstance(profile, dict):
            raise ValueError("each profile must be an object named aero/...")
        for role in ("planner", "executor"):
            entry = profile.get(role)
            if not isinstance(entry, dict) or not isinstance(entry.get("model"), str):
                raise ValueError(f"{name}.{role}.model must be a model name")
            if not entry["model"].strip() or entry["model"].startswith("aero/"):
                raise ValueError(f"{name}.{role}.model must name a non-aero EMP model")
            if "effort" in entry and (
                not isinstance(entry["effort"], str) or not entry["effort"].strip()
            ):
                raise ValueError(f"{name}.{role}.effort must be a nonempty string")
    if "key_file" in config:
        if not isinstance(config["key_file"], str) or not config["key_file"].strip():
            raise ValueError("key_file must be a file path")
        key_file = Path(config["key_file"]).expanduser()
        config["key_file"] = path.parent / key_file
    return config


def gateway_key(config):
    token = os.environ.get("EMP_GATEWAY_KEY")
    if token is None and "key_file" in config:
        token = config["key_file"].read_text()
    if not token or not token.strip():
        raise ValueError("Set EMP_GATEWAY_KEY or key_file to an EMP Gateway Key")
    token = token.strip()
    if any(character.isspace() or ord(character) < 32 for character in token):
        raise ValueError("The EMP Gateway Key must be a single token")
    return token
