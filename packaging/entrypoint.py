"""PyInstaller entry point kept outside the runtime package."""

from easy_multi_provider.main import main


if __name__ == "__main__":
    raise SystemExit(main())
