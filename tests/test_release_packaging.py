import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


PROJECT_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = PROJECT_ROOT / "packaging" / "validate_release.py"
SPEC = importlib.util.spec_from_file_location("emp_validate_release", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
release_validation = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release_validation
SPEC.loader.exec_module(release_validation)


class ReleasePackagingTests(unittest.TestCase):
    def _complete_assets(self, root: Path, version: str) -> None:
        for name in release_validation.primary_artifact_names(version):
            payload = (name + "\n").encode("utf-8")
            (root / name).write_bytes(payload)
            digest = hashlib.sha256(payload).hexdigest()
            (root / (name + ".sha256")).write_text(
                "%s  %s\n" % (digest, name), encoding="ascii"
            )

    def test_complete_manifest_matches_current_source_version(self):
        version = release_validation.source_version()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self._complete_assets(root, version)
            self.assertEqual(
                release_validation.validate_release("v" + version, root), version
            )

    def test_release_tag_must_match_source_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaisesRegex(
                release_validation.ReleaseValidationError,
                "does not match source version",
            ):
                release_validation.validate_release("v999.0.0", Path(temporary))

    def test_missing_or_unexpected_asset_fails_closed(self):
        version = release_validation.source_version()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self._complete_assets(root, version)
            missing = sorted(release_validation.primary_artifact_names(version))[0]
            (root / missing).unlink()
            (root / "unexpected.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaisesRegex(
                release_validation.ReleaseValidationError,
                "release manifest mismatch",
            ):
                release_validation.validate_release("v" + version, root)

    def test_checksum_mismatch_fails_closed(self):
        version = release_validation.source_version()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self._complete_assets(root, version)
            artifact = sorted(release_validation.primary_artifact_names(version))[0]
            (root / artifact).write_bytes(b"changed")
            with self.assertRaisesRegex(
                release_validation.ReleaseValidationError,
                "checksum mismatch",
            ):
                release_validation.validate_release("v" + version, root)


if __name__ == "__main__":
    unittest.main()
