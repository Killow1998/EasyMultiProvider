"""Windows resource contracts for the native Rust package builder."""

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


PROJECT_ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "emp_native_package_builder", PROJECT_ROOT / "packaging" / "build.py"
)
assert SPEC is not None and SPEC.loader is not None
builder = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = builder
SPEC.loader.exec_module(builder)


class WindowsResourceDefinitionTests(unittest.TestCase):
    def test_resource_script_embeds_icon_and_exact_source_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            icon = root / "source icon ✓.ico"
            icon.write_bytes(b"fixture icon")

            script = builder._write_windows_resource_script(
                root / "resources", icon, "0.11.10"
            )

            self.assertEqual(
                (script.parent / "easy-multi-provider.ico").read_bytes(),
                b"fixture icon",
            )
            source = script.read_text(encoding="ascii")
            self.assertIn('1 ICON "easy-multi-provider.ico"', source)
            self.assertNotIn(str(icon), source)
            self.assertIn("FILEVERSION 0,11,10,0", source)
            self.assertIn("PRODUCTVERSION 0,11,10,0", source)
            self.assertIn('VALUE "FileVersion", "0.11.10"', source)
            self.assertIn('VALUE "ProductVersion", "0.11.10"', source)
            self.assertIn('VALUE "OriginalFilename", "EMP.exe"', source)

    def test_resource_script_rejects_missing_icon_and_nonstable_versions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing_icon = root / "missing.ico"
            with self.assertRaisesRegex(RuntimeError, "icon is missing"):
                builder._write_windows_resource_script(root / "missing", missing_icon, "0.11.10")

            icon = root / "icon.ico"
            icon.write_bytes(b"fixture icon")
            for version in ("0.11.10-beta", "0.11", "0.11.65536"):
                with self.subTest(version=version):
                    with self.assertRaises(RuntimeError):
                        builder._write_windows_resource_script(root / "resources", icon, version)

    def test_resource_compilation_fails_closed_for_missing_icon_or_rc(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            script = root / "EMP.rc"
            script.write_text("resource fixture", encoding="ascii")
            with self.assertRaisesRegex(RuntimeError, "icon is missing"):
                builder._compile_windows_resource(script)

            (root / "easy-multi-provider.ico").write_bytes(b"fixture icon")
            with patch.dict(
                os.environ,
                {"WindowsSdkBinPath": "", "WindowsSdkDir": "", "WindowsSDKVersion": "", "ProgramFiles(x86)": ""},
                clear=True,
            ):
                with patch.object(builder.shutil, "which", return_value=None):
                    with self.assertRaisesRegex(RuntimeError, "rc.exe was not found"):
                        builder._compile_windows_resource(script)

    def test_resource_compilation_uses_windows_sdk_fallback_and_checks_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sdk_root = root / "Program Files (x86)"
            sdk_bin = sdk_root / "Windows Kits" / "10" / "bin"
            older = sdk_bin / "10.0.9.0" / "x64"
            newer = sdk_bin / "10.0.10.0" / "x64"
            older.mkdir(parents=True)
            newer.mkdir(parents=True)
            old_rc = older / "rc.exe"
            rc = newer / "rc.exe"
            old_rc.write_bytes(b"rc")
            rc.write_bytes(b"rc")
            script_dir = root / "resources"
            script_dir.mkdir()
            script = script_dir / "EMP.rc"
            script.write_text("resource fixture", encoding="ascii")
            (script_dir / "easy-multi-provider.ico").write_bytes(b"fixture icon")

            with patch.dict(os.environ, {"ProgramFiles(x86)": str(sdk_root)}, clear=True):
                with patch.object(builder.shutil, "which", return_value=None):
                    self.assertEqual(builder._find_windows_resource_compiler(), rc)

                    def compile_resource(command, **kwargs):
                        script.with_suffix(".res").write_bytes(b"compiled resource")
                        return SimpleNamespace(returncode=0, stdout="", stderr="")

                    with patch.object(
                        builder.subprocess,
                        "run",
                        side_effect=compile_resource,
                    ) as run:
                        result = builder._compile_windows_resource(script)

            self.assertEqual(result.read_bytes(), b"compiled resource")
            self.assertEqual(run.call_args.args[0][0], str(rc))
            self.assertEqual(run.call_args.kwargs["cwd"], str(script_dir))

    def test_windows_binary_build_requires_resource_script_and_passes_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target_root = root / "target"
            (target_root / "release").mkdir(parents=True)
            binary = target_root / "release" / "EMP.exe"
            binary.write_bytes(b"synthetic PE placeholder")
            resource_file = root / "EMP.res"
            resource_file.write_bytes(b"compiled resource")
            target = builder.Target("Windows", "windows", "x86_64", ".exe")

            with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target_root)}):
                with patch.object(builder.shutil, "which", return_value="cargo"):
                    with patch.object(builder, "_run") as run:
                        result = builder._build_binary(target, resource_file)

            self.assertEqual(result, binary)
            environment = run.call_args.args[1]
            self.assertEqual(environment["EMP_PACKAGE_RESOURCE"], str(resource_file.resolve()))
            self.assertEqual(environment["EMP_REQUIRE_WINDOWS_RESOURCES"], "1")

    def test_windows_binary_build_fails_before_cargo_without_resource_script(self):
        target = builder.Target("Windows", "windows", "x86_64", ".exe")
        with patch.object(builder, "_run") as run:
            with self.assertRaisesRegex(RuntimeError, "compiled icon/version resources"):
                builder._build_binary(target)
        run.assert_not_called()


@unittest.skipUnless(sys.platform == "win32", "requires a Windows package artifact")
class WindowsPackageArtifactTests(unittest.TestCase):
    def test_built_executable_has_icon_and_current_version_resources(self):
        executable = PROJECT_ROOT / "artifacts" / "EMP.exe"
        self.assertTrue(executable.is_file(), "build the Windows package first")
        builder._validate_windows_resources(executable, builder.project_version())


if __name__ == "__main__":
    unittest.main()
