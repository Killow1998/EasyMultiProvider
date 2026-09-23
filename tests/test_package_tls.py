"""Packaging defines a native Cargo executable rather than a Python freezer."""
import importlib.util
import sys
import tomllib
import unittest
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "emp_native_package_builder", PROJECT_ROOT / "packaging" / "build.py"
)
assert SPEC is not None and SPEC.loader is not None
builder = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = builder
SPEC.loader.exec_module(builder)


class NativePackageDefinitionTests(unittest.TestCase):
    def test_binary_version_comes_from_the_cargo_workspace(self):
        workspace = tomllib.loads(
            (PROJECT_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(
            builder.project_version(), workspace["workspace"]["package"]["version"]
        )

    def test_packaging_dependencies_do_not_include_pyinstaller(self):
        project = tomllib.loads(
            (PROJECT_ROOT / "pyproject.toml").read_text(encoding="utf-8")
        )
        package_group = project.get("dependency-groups", {}).get("package", [])
        self.assertFalse(
            any(str(dependency).lower().startswith("pyinstaller") for dependency in package_group)
        )
        source = (PROJECT_ROOT / "packaging" / "build.py").read_text(encoding="utf-8")
        self.assertIn('"--package",\n            "emp-app"', source)
        self.assertIn('"--bin",\n            EXECUTABLE_NAME', source)
        self.assertNotIn('"-m",\n            "PyInstaller"', source)


if __name__ == "__main__":
    unittest.main()
