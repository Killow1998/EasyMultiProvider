"""Run with python3 -B -m scripts.native_restore_acceptance."""

import sys

from .scenario import main

if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"harness setup failed: {error}", file=sys.stderr)
        raise SystemExit(2) from error
