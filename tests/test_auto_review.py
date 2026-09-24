import json
import tempfile
import threading
import unittest
from pathlib import Path

from easy_multi_provider.accounts import NATIVE_ACCOUNT_ID
from easy_multi_provider.auto_review import (
    automatic_review_candidates,
    quota_headroom,
)
from easy_multi_provider.catalog import build_catalog
from easy_multi_provider.config import normalize
from easy_multi_provider.route_plan import SUBSCRIPTION_ACCOUNT, resolve_route
from easy_multi_provider.server import AppState


def quota(used):
    return {"rate_limits": {"primary": {"usedPercent": used}}}


class AutoReviewTests(unittest.TestCase):
    def test_quota_headroom_uses_the_tightest_reported_limit(self):
        self.assertEqual(
            quota_headroom(
                {
                    "rate_limits": {
                        "primary": {"usedPercent": 25},
                        "secondary": {"usedPercent": 90},
                    }
                }
            ),
            10,
        )
        self.assertEqual(
            quota_headroom({"credits": {"spend_control_reached": True}}), 0
        )
        self.assertIsNone(quota_headroom(None))

    def test_native_is_preferred_until_exhausted_then_best_account_is_used(self):
        config = normalize(
            {
                "accounts": [
                    {"id": "low", "prefix": "low", "auth_file": "low.enc", "quota": quota(80)},
                    {"id": "high", "prefix": "high", "auth_file": "high.enc", "quota": quota(10)},
                    {"id": "empty", "prefix": "empty", "auth_file": "empty.enc", "quota": quota(100)},
                ]
            }
        )
        self.assertEqual(
            automatic_review_candidates(config, quota(40), True),
            [NATIVE_ACCOUNT_ID, "high", "low"],
        )
        self.assertEqual(
            automatic_review_candidates(config, quota(100), True),
            ["high", "low"],
        )
        self.assertEqual(
            automatic_review_candidates(
                config, quota(40), True, {NATIVE_ACCOUNT_ID: 200}, now=100
            ),
            ["high", "low"],
        )

    def test_review_route_accepts_default_and_stale_catalog_alias(self):
        with tempfile.TemporaryDirectory() as directory:
            catalog = Path(directory) / "models.json"
            catalog.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "codex-auto-review",
                                "visibility": "hide",
                                "supported_in_api": True,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(catalog),
                    "accounts": [
                        {
                            "id": "backup",
                            "prefix": "reserve",
                            "auth_file": str(Path(directory) / "backup.enc"),
                        }
                    ],
                }
            )
            config["_auto_review_candidates"] = ["backup"]
            models = {item["slug"]: item for item in build_catalog(config)["models"]}
            self.assertEqual(models["codex-auto-review"]["visibility"], "hide")
            self.assertNotIn("auto_review_model_override", models["codex-auto-review"])
            for requested in ("codex-auto-review", "old/codex-auto-review"):
                route = resolve_route(config, requested)
                self.assertEqual(route.source, SUBSCRIPTION_ACCOUNT)
                self.assertEqual(route.provider_id, "backup")
                self.assertEqual(route.requested_model, requested)
                self.assertEqual(route.upstream_model, "codex-auto-review")

    def test_rate_limit_temporarily_removes_the_failed_review_account(self):
        state = AppState.__new__(AppState)
        state.lock = threading.RLock()
        state._auto_review_cooldowns = {}
        state._observe_auto_review_route(
            {
                "client_model": "codex-auto-review",
                "provider_id": "codex-native",
                "success": False,
                "error_class": "rate_limit",
                "failure_reason": "rate_limited",
            }
        )
        self.assertIn(NATIVE_ACCOUNT_ID, state._auto_review_cooldowns)
        state._observe_auto_review_route(
            {
                "client_model": "codex-auto-review",
                "provider_id": "codex-native",
                "success": True,
            }
        )
        self.assertNotIn(NATIVE_ACCOUNT_ID, state._auto_review_cooldowns)


if __name__ == "__main__":
    unittest.main()
