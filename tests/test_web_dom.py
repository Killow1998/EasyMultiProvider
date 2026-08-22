import subprocess
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


class WebDomBehaviorTests(unittest.TestCase):
    def test_integration_and_picker_dom_behavior(self):
        result = subprocess.run(
            ["node", str(ROOT / "tests" / "web_dom_harness.js"), str(ROOT / "easy_multi_provider" / "web" / "index.html")],
            text=True,
            capture_output=True,
            timeout=20,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("web DOM behavior: ok", result.stdout)


if __name__ == "__main__":
    unittest.main()
