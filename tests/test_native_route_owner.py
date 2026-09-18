"""Native socket reuse follows Codex 0.155 auth ownership, not token equality."""
import base64
import json
import unittest

from easy_multi_provider.native_identity import derive_native_route_identity


def _identity(user, workspace, nonce, *, valid_token=True):
    # Synthetic claims match official login/auth/change_state_tests.rs.
    claims = {"jti": nonce, "https://api.openai.com/auth": {"chatgpt_user_id": user}}
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    token = "fixture." + payload + ".signature" if valid_token else "opaque-" + nonce
    headers = {"chatgpt-account-id": workspace, "Authorization": "Bearer " + token}
    return derive_native_route_identity(
        headers, "https://native.example/backend-api/codex", "deployment", "model", headers,
    ).connection_key


class NativeRouteOwnerTests(unittest.TestCase):
    def test_known_owner_token_refresh_preserves_connection_identity(self):
        self.assertEqual(_identity("user-a", "workspace-a", "token-1"),
                         _identity("user-a", "workspace-a", "token-2"))

    def test_different_users_in_same_workspace_have_separate_connections(self):
        self.assertNotEqual(_identity("user-a", "workspace-a", "token-1"),
                            _identity("user-b", "workspace-a", "token-2"))

    def test_workspace_switch_has_separate_connection_identity(self):
        self.assertNotEqual(_identity("user-a", "workspace-a", "token-1"),
                            _identity("user-a", "workspace-b", "token-2"))

    def test_incomplete_or_opaque_owner_refresh_cannot_reuse_previous_credentials(self):
        for user, valid in (("", True), (" ", True), (None, True), ("user-a", False)):
            with self.subTest(user=user, valid=valid):
                self.assertNotEqual(_identity(user, "workspace-a", "token-1", valid_token=valid),
                                    _identity(user, "workspace-a", "token-2", valid_token=valid))

    def test_non_object_jwt_claims_use_conservative_credential_identity(self):
        for claims in (None, [], "owner", 42):
            with self.subTest(claims=claims):
                payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
                keys = []
                for signature in ("signature-1", "signature-2"):
                    headers = {"chatgpt-account-id": "workspace-a",
                               "Authorization": "Bearer fixture." + payload + "." + signature}
                    keys.append(derive_native_route_identity(
                        headers, "https://native.example/backend-api/codex", "deployment", "model", headers,
                    ).connection_key)
                self.assertNotEqual(*keys)

    def test_non_bearer_credentials_cannot_reuse_bearer_owner_connection(self):
        claims = {"https://api.openai.com/auth": {"chatgpt_user_id": "user-a"}}
        payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        token = "fixture." + payload + ".signature"
        keys = []
        for scheme in ("Bearer ", "Basic ", ""):
            headers = {"chatgpt-account-id": "workspace-a", "Authorization": scheme + token}
            keys.append(derive_native_route_identity(
                headers, "https://native.example/backend-api/codex", "deployment", "model", headers,
            ).connection_key)
        self.assertEqual(len(set(keys)), 3)
