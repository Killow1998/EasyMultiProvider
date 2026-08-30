"""Generate native package icons from EMP's committed RGBA master image."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Optional, Sequence

from PIL import Image


PROJECT_ROOT = Path(__file__).resolve().parents[1]
MASTER_ICON = (
    PROJECT_ROOT / "assets" / "branding" / "easy-multi-provider-icon-1024.png"
)
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)


def generate_icons(output: Path, master_path: Path = MASTER_ICON) -> None:
    output.mkdir(parents=True, exist_ok=True)
    with Image.open(master_path) as source:
        master = source.convert("RGBA")

    if master.size != (1024, 1024):
        raise RuntimeError("icon master must be exactly 1024 x 1024")
    corners = ((0, 0), (1023, 0), (0, 1023), (1023, 1023))
    if any(master.getpixel(point)[3] != 0 for point in corners):
        raise RuntimeError("icon master must have transparent outer corners")

    master.save(
        output / "easy-multi-provider.ico",
        format="ICO",
        sizes=[(size, size) for size in ICO_SIZES],
    )
    master.save(output / "easy-multi-provider.icns", format="ICNS")
    master.resize((256, 256), Image.Resampling.LANCZOS).save(
        output / "easy-multi-provider-256.png",
        format="PNG",
        optimize=True,
    )


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="Generate EMP native icon assets")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--master", type=Path, default=MASTER_ICON)
    args = parser.parse_args(argv)
    generate_icons(args.output, args.master)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
