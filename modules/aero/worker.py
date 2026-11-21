import json
import logging
import sqlite3
import sys
import threading
from pathlib import Path
from urllib.parse import quote, urlsplit

from gateway import create_server
from settings import ROOT, normalize_config

VERSION = "0.10.0-beta"


def panel():
    web = ROOT / "web"
    css = (web / "aero.css").read_text(encoding="utf-8")
    svg = quote((web / "aero.svg").read_text(encoding="utf-8"), safe="")
    css = css.replace("/assets/aero.svg", "data:image/svg+xml," + svg)
    return "\n".join(
        [
            "(()=>{",
            "const style=document.getElementById('emp_aero_style')||document.head.appendChild(Object.assign(document.createElement('style'),{id:'emp_aero_style'}));",
            "style.textContent=" + json.dumps(css) + ";",
            (web / "copy-aero.js").read_text(encoding="utf-8"),
            (web / "aero-settings.js").read_text(encoding="utf-8"),
            "window.createAeroSettings=createAeroSettings;",
            "})();",
        ]
    )


def required_text(command, key):
    value = command.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError("aero.invalid_" + key)
    return value


class Runtime:
    def __init__(self):
        self.server = None

    def command(self, command):
        if not isinstance(command, dict):
            raise ValueError("invalid_module_command")
        kind = command.get("command")
        if kind == "read_panel":
            return panel()
        if kind == "aero_start":
            if self.server is not None:
                raise ValueError("aero.already_started")
            directory = Path(required_text(command, "state_dir")).expanduser().resolve()
            base = required_text(command, "core_base_url").rstrip("/")
            target = urlsplit(base)
            if target.scheme != "http" or target.hostname not in ("127.0.0.1", "::1"):
                raise ValueError("aero.invalid_core_base_url")
            inbound = required_text(command, "inbound_key")
            upstream = required_text(command, "upstream_key")
            if any(
                character.isspace() for key in (inbound, upstream) for character in key
            ):
                raise ValueError("aero.invalid_key")
            config = normalize_config(
                {"upstream": base + "/v1", "models": command.get("profiles")}
            )
            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            server, store = create_server(
                config, directory, token=inbound, upstream_key=upstream
            )
            self.store = store
            self.directory = directory
            self.server = server
            self.thread = threading.Thread(target=server.serve_forever)
            self.thread.start()
            return {"endpoint": f"http://127.0.0.1:{server.server_port}/v1"}
        if kind == "aero_configure":
            if self.server is None:
                raise ValueError("aero.not_started")
            config = normalize_config(
                {**self.server.configuration, "models": command.get("profiles")}
            )
            self.server.configuration = config
            return {}
        raise ValueError("invalid_module_command")

    def close(self):
        if self.server is not None:
            self.server.drain()
            self.thread.join()
            self.store.close()
            logger = logging.getLogger("aero." + str(self.directory))
            for handler in logger.handlers[:]:
                logger.removeHandler(handler)
                handler.close()


def write(value):
    sys.stdout.write(json.dumps(value, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main():
    sys.stdin.reconfigure(encoding="utf-8")
    sys.stdout.reconfigure(encoding="utf-8")
    write(
        {
            "module": "aero",
            "version": VERSION,
            "module_version": VERSION,
            "protocol": 1,
            "host_api": 2,
        }
    )
    runtime = Runtime()
    try:
        for line in sys.stdin:
            try:
                value = runtime.command(json.loads(line))
            except ValueError as error:
                write(
                    {
                        "value": None,
                        "error": str(error)
                        if str(error).startswith("aero.")
                        else "invalid_module_command",
                    }
                )
            except (OSError, sqlite3.Error):
                write({"value": None, "error": "aero.runtime_unavailable"})
            else:
                write({"value": value, "error": None})
    finally:
        runtime.close()


if __name__ == "__main__":
    main()
