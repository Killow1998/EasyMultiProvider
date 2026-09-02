import importlib.util
import os
import plistlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

try:
    from PIL import Image
except ImportError:  # Packaging-only dependency; normal runtime tests may omit it.
    Image = None


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def _load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


package_builder = _load_module(
    "emp_package_builder", PROJECT_ROOT / "packaging" / "build.py"
)
icon_assets = (
    _load_module("emp_icon_assets", PROJECT_ROOT / "packaging" / "icon_assets.py")
    if Image is not None
    else None
)


@unittest.skipIf(Image is None, "Pillow is only installed in the packaging group")
class IconPackagingTests(unittest.TestCase):
    def test_master_has_no_opaque_application_tile(self):
        with Image.open(icon_assets.MASTER_ICON) as image:
            master = image.convert("RGBA")

        self.assertEqual(master.getpixel((0, 0))[3], 0)
        self.assertEqual(master.getpixel((100, 512))[3], 0)

    def test_master_generates_windows_macos_and_linux_icons(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            icon_assets.generate_icons(output)

            expected = {
                "easy-multi-provider.ico": ("ICO", (256, 256)),
                "easy-multi-provider.icns": ("ICNS", (1024, 1024)),
                "easy-multi-provider-256.png": ("PNG", (256, 256)),
            }
            for name, (image_format, size) in expected.items():
                with Image.open(output / name) as image:
                    self.assertEqual(image.format, image_format)
                    self.assertEqual(image.size, size)
                    if image_format == "ICO":
                        self.assertEqual(
                            image.ico.sizes(),
                            {(value, value) for value in icon_assets.ICO_SIZES},
                        )

    def test_macos_app_has_icon_and_visible_terminal_launcher(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            icon_assets.generate_icons(root / "icons")
            executable = root / "EMP"
            executable.write_bytes(b"frozen-binary")
            icons = package_builder.PackageIcons(
                windows=root / "icons" / "easy-multi-provider.ico",
                macos=root / "icons" / "easy-multi-provider.icns",
                linux=root / "icons" / "easy-multi-provider-256.png",
            )
            app = root / "EMP.app"

            package_builder._write_macos_app(app, executable, "0.9.0", icons)

            with (app / "Contents" / "Info.plist").open("rb") as handle:
                info = plistlib.load(handle)
            self.assertEqual(info["CFBundleExecutable"], "EMP")
            self.assertEqual(
                info["CFBundleIconFile"], "easy-multi-provider.icns"
            )
            launcher = (app / "Contents" / "MacOS" / "EMP").read_text(
                encoding="utf-8"
            )
            command = (app / "Contents" / "Resources" / "launch.command").read_text(
                encoding="utf-8"
            )
            self.assertIn("open -a Terminal", launcher)
            self.assertIn("--open-browser", command)
            self.assertIn("Application Support/EasyMultiProvider", command)
            if os.name != "nt":
                subprocess.run(
                    (
                        "/bin/sh",
                        "-n",
                        str(app / "Contents" / "MacOS" / "EMP"),
                    ),
                    check=True,
                )
                subprocess.run(
                    (
                        "/bin/sh",
                        "-n",
                        str(
                            app
                            / "Contents"
                            / "Resources"
                            / "launch.command"
                        ),
                    ),
                    check=True,
                )


if __name__ == "__main__":
    unittest.main()
