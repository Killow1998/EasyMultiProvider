"""CLI entry point."""

from __future__ import annotations

import argparse
from pathlib import Path

from .server import serve


def main() -> None:
    parser = argparse.ArgumentParser(description="Web-configured local router for Codex models")
    parser.add_argument("--config", type=Path, help="configuration JSON path")
    parser.add_argument("--host", help="override listen host")
    parser.add_argument("--port", type=int, help="override listen port")
    args = parser.parse_args()
    serve(args.config, args.host, args.port)


if __name__ == "__main__":
    main()
