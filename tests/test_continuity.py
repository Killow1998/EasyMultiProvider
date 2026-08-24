import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.continuity import (
    BindingError,
    BindingMissing,
    BindingStale,
    CompactionBinding,
    CompactionBindingStore,
    ContinuityError,
    NativeCompactionObserver,
    NativeRouteIdentity,
    NativeTrustDomain,
    binding_for_opaque,
    derive_native_route_identity,
    fingerprint_opaque_content,
    register_native_json,
)
from easy_multi_provider.vault import read_encrypted_json, write_encrypted_json
from easy_multi_provider.capabilities import endpoint_fingerprint

from tests.support import ensure_test_master_key


ensure_test_master_key()

_OPAQUE = "opaque-native-encrypted-content"
_SOURCE_MODEL = "source-model-fixture"
_DOMAIN_A = NativeTrustDomain("auth-a", "endpoint-shape-a")
_DOMAIN_B = NativeTrustDomain("auth-b", "endpoint-shape-b")
_EP_A = "route-endpoint-shape-a"
_DEP_A = "deployment-shape-a"


_ENDPOINT_A = "https://api.example.com/v1/responses"
_ENDPOINT_B = "https://api.example.org/v1/responses"
_ACCOUNT_A = "acct-0000000000000000001"
_ACCOUNT_B = "acct-0000000000000000002"
_SOURCE_SLUG_A = "login/gpt-5.6-sol"
_UPSTREAM_A = "gpt-5.6-sol"
_DEPLOY_A = "sol-deploy-east"
_OPAQUE_JSON_A = "native-opaque-envelope-AAA"


def _binding(**overrides):
    opaque = overrides.pop("opaque", _OPAQUE)
    defaults = {
        "fingerprint": fingerprint_opaque_content(opaque),
        "trust_domain": _DOMAIN_A,
        "source_model_slug": _SOURCE_MODEL,
        "endpoint_fingerprint": _EP_A,
        "deployment_fingerprint": _DEP_A,
    }
    defaults.update(overrides)
    return CompactionBinding(**defaults)


class ContinuityTests(unittest.TestCase):
    def test_encrypted_round_trip_and_raw_file_privacy(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path)
            store.register(_binding())

            raw = path.read_bytes()
            self.assertNotIn(_OPAQUE.encode(), raw)
            self.assertNotIn(_SOURCE_MODEL.encode(), raw)

            result = store.resolve(
                _OPAQUE, _DOMAIN_A, _EP_A, _DEP_A,
            )
            self.assertEqual(result.source_model_slug, _SOURCE_MODEL)
            self.assertEqual(result.trust_domain, _DOMAIN_A)
            self.assertGreaterEqual(result.created_at, 0)
            self.assertGreaterEqual(result.updated_at, result.created_at)

    def test_capacity_eviction_removes_oldest_first(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path, capacity=2)
            store.register(
                _binding(opaque="opaque-one", created_at=1.0, updated_at=1.0)
            )
            store.register(
                _binding(opaque="opaque-two", created_at=2.0, updated_at=2.0)
            )
            store.register(
                _binding(opaque="opaque-three", created_at=3.0, updated_at=3.0)
            )

            with self.assertRaises(BindingMissing):
                store.resolve("opaque-one", _DOMAIN_A, _EP_A, _DEP_A)
            store.resolve("opaque-two", _DOMAIN_A, _EP_A, _DEP_A)
            store.resolve("opaque-three", _DOMAIN_A, _EP_A, _DEP_A)

    def test_reregister_preserves_created_at_and_fifo_eviction_order(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path, capacity=2)
            store.register(
                _binding(opaque="opaque-oldest", created_at=1.0, updated_at=1.0)
            )
            store.register(
                _binding(opaque="opaque-second", created_at=2.0, updated_at=2.0)
            )
            store.register(
                _binding(
                    opaque="opaque-oldest",
                    created_at=99.0,
                    updated_at=100.0,
                )
            )

            updated = store.resolve(
                "opaque-oldest", _DOMAIN_A, _EP_A, _DEP_A
            )
            self.assertEqual(updated.created_at, 1.0)
            self.assertEqual(updated.updated_at, 100.0)
            self.assertEqual(updated.source_model_slug, _SOURCE_MODEL)

            store.register(
                _binding(
                    opaque="opaque-third", created_at=101.0, updated_at=101.0
                )
            )
            with self.assertRaises(BindingMissing):
                store.resolve("opaque-oldest", _DOMAIN_A, _EP_A, _DEP_A)
            store.resolve("opaque-second", _DOMAIN_A, _EP_A, _DEP_A)
            store.resolve("opaque-third", _DOMAIN_A, _EP_A, _DEP_A)

    def test_reregister_rejects_each_source_conflict_without_rewrite(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path)
            store.register(_binding(created_at=1.0, updated_at=1.0))
            original = path.read_bytes()

            format_conflict = _binding(created_at=2.0, updated_at=2.0)
            object.__setattr__(format_conflict, "format_version", "emp2")
            conflicts = {
                "auth-identity": _binding(
                    trust_domain=NativeTrustDomain(
                        "conflicting-auth", _DOMAIN_A.endpoint_fingerprint
                    ),
                    created_at=2.0,
                    updated_at=2.0,
                ),
                "trust-endpoint": _binding(
                    trust_domain=NativeTrustDomain(
                        _DOMAIN_A.auth_identity, "conflicting-trust-endpoint"
                    ),
                    created_at=2.0,
                    updated_at=2.0,
                ),
                "source-model": _binding(
                    source_model_slug="conflicting-source-model",
                    created_at=2.0,
                    updated_at=2.0,
                ),
                "endpoint-deployment": _binding(
                    endpoint_fingerprint="conflicting-endpoint-deployment",
                    created_at=2.0,
                    updated_at=2.0,
                ),
                "deployment": _binding(
                    deployment_fingerprint="conflicting-deployment",
                    created_at=2.0,
                    updated_at=2.0,
                ),
                "format-version": format_conflict,
            }

            for name, conflict in conflicts.items():
                with self.subTest(field=name):
                    with self.assertRaises(BindingStale) as raised:
                        store.register(conflict)
                    self.assertEqual(path.read_bytes(), original)
                    message = str(raised.exception)
                    for private_value in (
                        _OPAQUE,
                        _SOURCE_MODEL,
                        "conflicting-auth",
                        "conflicting-trust-endpoint",
                        "conflicting-source-model",
                        "conflicting-endpoint-deployment",
                        "conflicting-deployment",
                    ):
                        self.assertNotIn(private_value, message)

            result = store.resolve(_OPAQUE, _DOMAIN_A, _EP_A, _DEP_A)
            self.assertEqual(result.created_at, 1.0)
            self.assertEqual(result.updated_at, 1.0)

    def test_fresh_instance_reloads_bindings(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            CompactionBindingStore(path).register(_binding())
            reloaded = CompactionBindingStore(path)
            result = reloaded.resolve(_OPAQUE, _DOMAIN_A, _EP_A, _DEP_A)
            self.assertEqual(result.source_model_slug, _SOURCE_MODEL)

    def test_lookup_returns_reloaded_source_binding_without_opaque_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path)
            store.register(_binding())

            result = store.lookup(_OPAQUE)
            self.assertEqual(result.trust_domain, _DOMAIN_A)
            self.assertEqual(result.source_model_slug, _SOURCE_MODEL)
            self.assertEqual(result.endpoint_fingerprint, _EP_A)
            self.assertEqual(result.deployment_fingerprint, _DEP_A)
            self.assertNotIn(_OPAQUE, repr(result))
            with self.assertRaises(BindingMissing):
                store.lookup("synthetic-missing-opaque")

            reloaded = CompactionBindingStore(path).lookup(_OPAQUE)
            self.assertEqual(reloaded, result)
            self.assertNotIn(_OPAQUE, repr(reloaded))

    def test_same_domain_hit_and_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path)
            store.register(_binding())
            store.resolve(_OPAQUE, _DOMAIN_A, _EP_A, _DEP_A)
            with self.assertRaises(BindingMissing):
                store.resolve("opaque-absent", _DOMAIN_A, _EP_A, _DEP_A)

    def test_endpoint_deployment_or_domain_mismatch_is_stale(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            store = CompactionBindingStore(path)
            store.register(_binding())

            with self.assertRaises(BindingStale):
                store.resolve(_OPAQUE, _DOMAIN_A, "different-endpoint", _DEP_A)
            with self.assertRaises(BindingStale):
                store.resolve(_OPAQUE, _DOMAIN_A, _EP_A, "different-deployment")
            with self.assertRaises(BindingStale):
                store.resolve(_OPAQUE, _DOMAIN_B, _EP_A, _DEP_A)

    def test_opaque_content_larger_than_one_mib_round_trips(self):
        opaque = "synthetic-large-opaque:" + ("x" * (1024 * 1024 + 17))
        with tempfile.TemporaryDirectory() as directory:
            store = CompactionBindingStore(Path(directory) / "bindings.enc")
            store.register(_binding(opaque=opaque))

            result = store.resolve(opaque, _DOMAIN_A, _EP_A, _DEP_A)

        self.assertEqual(result.fingerprint, fingerprint_opaque_content(opaque))
        self.assertEqual(result.source_model_slug, _SOURCE_MODEL)

    def test_threaded_register_and_resolve_hold_one_instance_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            store = CompactionBindingStore(
                Path(directory) / "bindings.enc", capacity=8
            )
            original_load = store._load
            original_save = store._save

            def checked_load():
                self.assertTrue(store._lock._is_owned())
                return original_load()

            def checked_save(bindings):
                self.assertTrue(store._lock._is_owned())
                return original_save(bindings)

            store._load = checked_load
            store._save = checked_save
            opaque_values = [
                "synthetic-concurrent-opaque-%d" % index for index in range(8)
            ]
            start = threading.Barrier(len(opaque_values))

            def register_and_resolve(index):
                opaque = opaque_values[index]
                source_model = "synthetic-source-%d" % index
                start.wait()
                store.register(
                    _binding(
                        opaque=opaque,
                        source_model_slug=source_model,
                        created_at=float(index + 1),
                        updated_at=float(index + 1),
                    )
                )
                return store.resolve(
                    opaque, _DOMAIN_A, _EP_A, _DEP_A
                ).source_model_slug

            with ThreadPoolExecutor(max_workers=len(opaque_values)) as executor:
                results = list(
                    executor.map(
                        register_and_resolve, range(len(opaque_values))
                    )
                )

            self.assertEqual(
                results,
                ["synthetic-source-%d" % index for index in range(8)],
            )
            for index, opaque in enumerate(opaque_values):
                result = store.resolve(opaque, _DOMAIN_A, _EP_A, _DEP_A)
                self.assertEqual(
                    result.source_model_slug, "synthetic-source-%d" % index
                )

    def test_unknown_top_level_format_fails_closed_without_rewrite(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bindings.enc"
            write_encrypted_json(path, {"format": "emp2", "bindings": []})
            original = path.read_bytes()
            store = CompactionBindingStore(path)

            with self.assertRaises(BindingError):
                store.resolve(_OPAQUE, _DOMAIN_A, _EP_A, _DEP_A)
            with self.assertRaises(BindingError):
                store.register(_binding())
            self.assertEqual(path.read_bytes(), original)

    def test_unknown_or_malformed_entry_fails_closed(self):
        valid_entry = {
            "fingerprint": fingerprint_opaque_content(_OPAQUE),
            "auth_identity": _DOMAIN_A.auth_identity,
            "endpoint_fingerprint": _DOMAIN_A.endpoint_fingerprint,
            "source_model_slug": _SOURCE_MODEL,
            "endpoint_deployment_fingerprint": _EP_A,
            "deployment_fingerprint": _DEP_A,
            "format_version": "emp1",
            "created_at": 1.0,
            "updated_at": 2.0,
        }
        malformed_entries = {
            "unknown-version": dict(valid_entry, format_version="emp2"),
            "invalid-fingerprint": dict(
                valid_entry, fingerprint="sha256:" + ("a" * 63)
            ),
            "missing-required-string": {
                key: value
                for key, value in valid_entry.items()
                if key != "source_model_slug"
            },
            "non-string": dict(valid_entry, auth_identity=7),
            "negative-created": dict(valid_entry, created_at=-1.0),
            "negative-updated": dict(valid_entry, updated_at=-1.0),
            "non-finite-created": dict(valid_entry, created_at=float("nan")),
            "non-finite-updated": dict(valid_entry, updated_at=float("inf")),
            "updated-before-created": dict(
                valid_entry, created_at=2.0, updated_at=1.0
            ),
            "extra-content-field": dict(
                valid_entry, prompt="synthetic-forbidden-content"
            ),
        }

        for name, entry in malformed_entries.items():
            with self.subTest(case=name):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "bindings.enc"
                    write_encrypted_json(
                        path, {"format": "emp1", "bindings": [entry]}
                    )
                    original = path.read_bytes()
                    store = CompactionBindingStore(path)
                    with self.assertRaises(BindingError) as raised:
                        store.resolve(
                            _OPAQUE, _DOMAIN_A, _EP_A, _DEP_A
                        )
                    with self.assertRaises(BindingError):
                        store.register(_binding())
                    self.assertEqual(path.read_bytes(), original)
                    message = str(raised.exception)
                    for private_value in (
                        _OPAQUE,
                        _SOURCE_MODEL,
                        _DOMAIN_A.auth_identity,
                        _DOMAIN_A.endpoint_fingerprint,
                        _EP_A,
                        _DEP_A,
                    ):
                        self.assertNotIn(private_value, message)

    def test_invalid_inputs_fail_without_content(self):
        with self.assertRaises(BindingError):
            fingerprint_opaque_content("")
        with self.assertRaises(BindingError):
            fingerprint_opaque_content(42)
        with self.assertRaises(BindingError):
            CompactionBindingStore("/tmp/x", capacity=0)
        with self.assertRaises(BindingError):
            CompactionBindingStore("/tmp/x", capacity=True)

        for fingerprint in (
            "sha256:" + ("a" * 63),
            "sha256:" + ("A" * 64),
            "sha256:" + ("a" * 65),
            "not-a-fingerprint",
            7,
        ):
            with self.subTest(fingerprint_type=type(fingerprint).__name__):
                with self.assertRaises(BindingError) as raised:
                    _binding(fingerprint=fingerprint)
                self.assertNotIn(_OPAQUE, str(raised.exception))

        invalid_timestamps = (
            {"created_at": "not-a-timestamp"},
            {"created_at": float("nan")},
            {"created_at": -1.0},
            {"updated_at": float("inf")},
            {"updated_at": -1.0},
            {"updated_at": True},
            {"created_at": 2.0, "updated_at": 1.0},
        )
        for timestamps in invalid_timestamps:
            with self.subTest(timestamp_fields=tuple(timestamps)):
                with self.assertRaises(BindingError) as raised:
                    _binding(**timestamps)
                self.assertNotIn(_OPAQUE, str(raised.exception))

def _identity(**overrides):
    defaults = {
        "account_id": _ACCOUNT_A,
        "endpoint": _ENDPOINT_A,
        "deployment_identity": _DEPLOY_A,
        "upstream_model": _UPSTREAM_A,
        "source_model_slug": _SOURCE_SLUG_A,
    }
    defaults.update(overrides)
    return derive_native_route_identity(
        headers={"chatgpt-account-id": defaults["account_id"]},
        endpoint=defaults["endpoint"],
        deployment_identity=defaults["deployment_identity"],
        upstream_model=defaults["upstream_model"],
        source_model_slug=defaults["source_model_slug"],
    )


def _compaction_output(opaque, **extra):
    item = {"type": "compaction", "encrypted_content": opaque}
    item.update(extra)
    return item


def _native_json_response(items, **extra):
    payload = {"output": items}
    payload.update(extra)
    return payload


class NativeRouteIdentityTests(unittest.TestCase):
    def test_header_case_insensitive_and_stable_identity(self):
        upper = derive_native_route_identity(
            headers={"ChatGPT-Account-ID": _ACCOUNT_A},
            endpoint=_ENDPOINT_A,
            deployment_identity=_DEPLOY_A,
            upstream_model=_UPSTREAM_A,
            source_model_slug=_SOURCE_SLUG_A,
        )
        mixed = derive_native_route_identity(
            headers={"chatgpt-Account-Id": _ACCOUNT_A},
            endpoint=_ENDPOINT_A,
            deployment_identity=_DEPLOY_A,
            upstream_model=_UPSTREAM_A,
            source_model_slug=_SOURCE_SLUG_A,
        )
        self.assertEqual(upper, mixed)
        self.assertIsInstance(upper, NativeRouteIdentity)
        self.assertEqual(upper.source_model_slug, _SOURCE_SLUG_A)
        self.assertEqual(upper.endpoint_fingerprint, endpoint_fingerprint(_ENDPOINT_A))

    def test_endpoint_is_fingerprinted_at_the_capabilities_boundary(self):
        endpoint = object()
        expected = "sha256:" + ("f" * 64)

        with patch(
            "easy_multi_provider.continuity.capabilities.endpoint_fingerprint",
            return_value=expected,
        ) as fingerprint:
            identity = derive_native_route_identity(
                headers={"chatgpt-account-id": _ACCOUNT_A},
                endpoint=endpoint,
                deployment_identity=_DEPLOY_A,
                upstream_model=_UPSTREAM_A,
                source_model_slug=_SOURCE_SLUG_A,
            )

        fingerprint.assert_called_once_with(endpoint)
        self.assertEqual(identity.endpoint_fingerprint, expected)
        self.assertEqual(identity.trust_domain.endpoint_fingerprint, expected)

    def test_invalid_capabilities_fingerprint_fails_without_returning_endpoint(self):
        private_endpoint = "https://private-endpoint.example/v1"

        with patch(
            "easy_multi_provider.continuity.capabilities.endpoint_fingerprint",
            return_value=private_endpoint,
        ):
            with self.assertRaises(ContinuityError) as raised:
                derive_native_route_identity(
                    headers={"chatgpt-account-id": _ACCOUNT_A},
                    endpoint=private_endpoint,
                    deployment_identity=_DEPLOY_A,
                    upstream_model=_UPSTREAM_A,
                    source_model_slug=_SOURCE_SLUG_A,
                )

        self.assertNotIn(private_endpoint, str(raised.exception))

    def test_missing_or_empty_account_id_fails_closed(self):
        for headers in (
            {},
            {"chatgpt-account-id": ""},
            {"chatgpt-account-id": "   "},
            {"other-header": "x"},
        ):
            with self.subTest(headers=headers):
                with self.assertRaises(ContinuityError):
                    derive_native_route_identity(
                        headers=headers,
                        endpoint=_ENDPOINT_A,
                        deployment_identity=_DEPLOY_A,
                        upstream_model=_UPSTREAM_A,
                        source_model_slug=_SOURCE_SLUG_A,
                    )

    def test_bearer_token_cannot_substitute_account_id(self):
        with self.assertRaises(ContinuityError):
            derive_native_route_identity(
                headers={
                    "Authorization": "Bearer secret-token",
                    "chatgpt-account-id": "",
                },
                endpoint=_ENDPOINT_A,
                deployment_identity=_DEPLOY_A,
                upstream_model=_UPSTREAM_A,
                source_model_slug=_SOURCE_SLUG_A,
            )

    def test_missing_or_empty_upstream_model_fails_closed(self):
        for upstream_model in (None, "", "   "):
            with self.subTest(upstream_model=upstream_model):
                with self.assertRaises(ContinuityError) as raised:
                    derive_native_route_identity(
                        headers={"chatgpt-account-id": _ACCOUNT_A},
                        endpoint=_ENDPOINT_A,
                        deployment_identity=_DEPLOY_A,
                        upstream_model=upstream_model,
                        source_model_slug=_SOURCE_SLUG_A,
                    )
                self.assertEqual(
                    str(raised.exception), "upstream model is unavailable"
                )

    def test_overlong_identity_inputs_fail_without_echoing_values(self):
        overlong = "private-value-" + ("x" * 512)
        cases = (
            (
                "account",
                {"chatgpt-account-id": overlong},
                _DEPLOY_A,
                _UPSTREAM_A,
                "account identity header is too long",
            ),
            (
                "deployment",
                {"chatgpt-account-id": _ACCOUNT_A},
                overlong,
                _UPSTREAM_A,
                "deployment identity is too long",
            ),
            (
                "upstream",
                {"chatgpt-account-id": _ACCOUNT_A},
                _DEPLOY_A,
                overlong,
                "upstream model is too long",
            ),
        )

        for name, headers, deployment, upstream, expected in cases:
            with self.subTest(field=name):
                with self.assertRaises(ContinuityError) as raised:
                    derive_native_route_identity(
                        headers=headers,
                        endpoint=_ENDPOINT_A,
                        deployment_identity=deployment,
                        upstream_model=upstream,
                        source_model_slug=_SOURCE_SLUG_A,
                    )
                self.assertEqual(str(raised.exception), expected)
                self.assertNotIn(overlong, str(raised.exception))

    def test_different_endpoint_isolates_trust_domain(self):
        a = _identity()
        b = _identity(endpoint=_ENDPOINT_B)
        self.assertNotEqual(a.trust_domain.auth_identity, b.trust_domain.auth_identity)
        self.assertNotEqual(
            a.trust_domain.endpoint_fingerprint, b.trust_domain.endpoint_fingerprint
        )
        self.assertNotEqual(a.deployment_fingerprint, b.deployment_fingerprint)

    def test_all_returned_fields_are_fingerprints_or_slugs_no_raw(self):
        identity = _identity()
        for raw in (
            _ACCOUNT_A,
            _ACCOUNT_B,
            _ENDPOINT_A,
            _ENDPOINT_B,
            _DEPLOY_A,
            _UPSTREAM_A,
            "secret-token",
            "Bearer",
        ):
            self.assertNotIn(raw, identity.trust_domain.auth_identity)
            self.assertNotIn(raw, identity.trust_domain.endpoint_fingerprint)
            self.assertNotIn(raw, identity.endpoint_fingerprint)
            self.assertNotIn(raw, identity.deployment_fingerprint)
        self.assertTrue(identity.trust_domain.auth_identity.startswith("sha256:"))
        self.assertTrue(identity.trust_domain.endpoint_fingerprint.startswith("sha256:"))
        self.assertTrue(identity.endpoint_fingerprint.startswith("sha256:"))
        self.assertTrue(identity.deployment_fingerprint.startswith("sha256:"))

    def test_identity_stable_across_repeated_calls(self):
        a = _identity()
        b = _identity()
        self.assertEqual(a, b)
        self.assertEqual(hash(a), hash(b))


class BindingForOpaqueTests(unittest.TestCase):
    def test_binding_for_opaque_has_correct_source_fields(self):
        identity = _identity()
        binding = binding_for_opaque(identity, _OPAQUE_JSON_A)
        self.assertEqual(binding.fingerprint, fingerprint_opaque_content(_OPAQUE_JSON_A))
        self.assertEqual(binding.trust_domain, identity.trust_domain)
        self.assertEqual(binding.source_model_slug, _SOURCE_SLUG_A)
        self.assertEqual(binding.endpoint_fingerprint, identity.endpoint_fingerprint)
        self.assertEqual(binding.deployment_fingerprint, identity.deployment_fingerprint)

    def test_binding_does_not_carry_opaque_value(self):
        identity = _identity()
        binding = binding_for_opaque(identity, _OPAQUE_JSON_A)
        self.assertNotIn(_OPAQUE_JSON_A, repr(binding))


class RegisterNativeJsonTests(unittest.TestCase):
    def _store(self, directory):
        return CompactionBindingStore(Path(directory) / "bindings.enc")

    def test_json_success_only_compaction_registered(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            response = _native_json_response([
                {"type": "message", "content": "visible-text-not-stored"},
                {"type": "reasoning", "summary": "reasoning-not-stored"},
                {"type": "tool_call", "arguments": "tool-not-stored"},
                {"type": "image", "url": "image-not-stored"},
                _compaction_output(_OPAQUE_JSON_A),
                _compaction_output("native-opaque-envelope-BBB"),
            ])
            count = register_native_json(store, identity, response, True)
            self.assertEqual(count, 2)
            store.lookup(_OPAQUE_JSON_A)
            store.lookup("native-opaque-envelope-BBB")
            raw = (Path(directory) / "bindings.enc").read_bytes()
            self.assertNotIn(b"visible-text-not-stored", raw)
            self.assertNotIn(b"reasoning-not-stored", raw)
            self.assertNotIn(b"tool-not-stored", raw)
            self.assertNotIn(b"image-not-stored", raw)

    def test_json_success_skips_non_compaction_and_empty_content(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            response = _native_json_response([
                _compaction_output(""),
                _compaction_output("   "),
                {"type": "compaction"},
                {"type": "compaction", "encrypted_content": 12345},
                _compaction_output(_OPAQUE_JSON_A),
            ])
            count = register_native_json(store, identity, response, True)
            self.assertEqual(count, 1)
            store.lookup(_OPAQUE_JSON_A)
            payload = read_encrypted_json(Path(directory) / "bindings.enc")
            self.assertEqual(len(payload["bindings"]), 1)

    def test_json_non_success_registers_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            response = _native_json_response(
                [_compaction_output(_OPAQUE_JSON_A)],
                status="failed",
                error={"message": "upstream-failed"},
            )
            count = register_native_json(store, identity, response, False)
            self.assertEqual(count, 0)
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_json_malformed_output_registers_nothing(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            for malformed in (
                {},
                {"output": "not-a-list"},
                {"output": [{"type": "compaction"}]},
                {"output": [{"type": "compaction", "encrypted_content": ""}]},
            ):
                with self.subTest(malformed=malformed):
                    count = register_native_json(store, identity, malformed, True)
                    self.assertEqual(count, 0)
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_json_error_is_content_free(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            write_encrypted_json(
                Path(directory) / "bindings.enc",
                {"format": "emp2", "bindings": []},
            )
            response = _native_json_response([_compaction_output(_OPAQUE_JSON_A)])
            with self.assertRaises(ContinuityError) as raised:
                register_native_json(store, identity, response, True)
            message = str(raised.exception)
            for private in (_OPAQUE_JSON_A, _ACCOUNT_A, _ENDPOINT_A, _UPSTREAM_A, "secret-token"):
                self.assertNotIn(private, message)


class NativeCompactionObserverTests(unittest.TestCase):
    def _store(self, directory):
        return CompactionBindingStore(Path(directory) / "bindings.enc")

    def test_stream_completed_registers_compaction(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            count = observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            self.assertEqual(count, 0)
            completed = observer.observe({
                "type": "response.completed",
                "response": _native_json_response([
                    _compaction_output("native-opaque-envelope-BBB"),
                ]),
            })
            self.assertEqual(completed, 2)
            store.lookup(_OPAQUE_JSON_A)
            store.lookup("native-opaque-envelope-BBB")

    def test_stream_completed_dedupes_same_opaque(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            completed = observer.observe({
                "type": "response.completed",
                "response": _native_json_response([
                    _compaction_output(_OPAQUE_JSON_A),
                    _compaction_output("native-opaque-envelope-BBB"),
                ]),
            })
            self.assertEqual(completed, 2)
            store.lookup(_OPAQUE_JSON_A)
            store.lookup("native-opaque-envelope-BBB")

    def test_stream_completed_releases_opaque_from_observer_state(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            observer = NativeCompactionObserver(store, _identity())
            observer.observe(
                {
                    "type": "response.output_item.done",
                    "item": _compaction_output(_OPAQUE_JSON_A),
                }
            )

            completed = observer.observe(
                {
                    "type": "response.completed",
                    "response": _native_json_response([]),
                }
            )

            self.assertEqual(completed, 1)
            store.lookup(_OPAQUE_JSON_A)
            retained_state = " ".join(
                repr(getattr(observer, slot)) for slot in observer.__slots__
            )
            self.assertNotIn(_OPAQUE_JSON_A, retained_state)

    def test_stream_failed_discards(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            count = observer.observe({
                "type": "response.failed",
                "error": {"message": "upstream-failed"},
            })
            self.assertEqual(count, 0)
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_stream_incomplete_discards(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            count = observer.observe({"type": "response.incomplete"})
            self.assertEqual(count, 0)
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_stream_malformed_completed_discards(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            observer = NativeCompactionObserver(store, _identity())
            observer.observe(
                {
                    "type": "response.output_item.done",
                    "item": _compaction_output(_OPAQUE_JSON_A),
                }
            )

            count = observer.observe(
                {
                    "type": "response.completed",
                    "response": {"output": "not-a-list"},
                }
            )

            self.assertEqual(count, 0)
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_stream_completed_with_nested_failure_discards(self):
        cases = (
            {"status": "failed", "output": []},
            {"status": "incomplete", "output": []},
            {"status": "completed", "error": {"message": "failed"}, "output": []},
        )
        for response in cases:
            with self.subTest(response=response), tempfile.TemporaryDirectory() as directory:
                store = self._store(directory)
                observer = NativeCompactionObserver(store, _identity())
                observer.observe(
                    {
                        "type": "response.output_item.done",
                        "item": _compaction_output(_OPAQUE_JSON_A),
                    }
                )

                self.assertEqual(
                    observer.observe(
                        {"type": "response.completed", "response": response}
                    ),
                    0,
                )
                with self.assertRaises(BindingMissing):
                    store.lookup(_OPAQUE_JSON_A)

    def test_stream_malformed_event_discards_and_closes_observer(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            observer = NativeCompactionObserver(store, _identity())
            observer.observe(
                {
                    "type": "response.output_item.done",
                    "item": _compaction_output(_OPAQUE_JSON_A),
                }
            )

            self.assertEqual(observer.observe("malformed-event"), 0)
            self.assertEqual(
                observer.observe(
                    {
                        "type": "response.completed",
                        "response": _native_json_response([]),
                    }
                ),
                0,
            )
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_stream_abandon_discards(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            observer.abandon()
            with self.assertRaises(BindingMissing):
                store.lookup(_OPAQUE_JSON_A)

    def test_stream_does_not_retain_ordinary_content(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": {"type": "message", "content": "secret-message"},
            })
            observer.observe({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "summary": "secret-reasoning"},
            })
            observer.observe({
                "type": "response.output_item.done",
                "item": {"type": "tool_call", "arguments": "secret-tool"},
            })
            observer.observe({
                "type": "response.output_item.done",
                "item": {"type": "image", "url": "secret-image-url"},
            })
            completed = observer.observe({
                "type": "response.completed",
                "response": _native_json_response([]),
            })
            self.assertEqual(completed, 0)
            for secret in ("secret-message", "secret-reasoning", "secret-tool", "secret-image-url"):
                self.assertNotIn(secret, repr(observer))
            raw = Path(directory) / "bindings.enc"
            if raw.exists():
                self.assertNotIn(b"secret-message", raw.read_bytes())

    def test_observer_errors_are_content_free(self):
        with tempfile.TemporaryDirectory() as directory:
            store = self._store(directory)
            identity = _identity()
            write_encrypted_json(
                Path(directory) / "bindings.enc",
                {"format": "emp2", "bindings": []},
            )
            observer = NativeCompactionObserver(store, identity)
            observer.observe({
                "type": "response.output_item.done",
                "item": _compaction_output(_OPAQUE_JSON_A),
            })
            with self.assertRaises(ContinuityError) as raised:
                observer.observe({
                    "type": "response.completed",
                    "response": _native_json_response([]),
                })
            message = str(raised.exception)
            for private in (_OPAQUE_JSON_A, _ACCOUNT_A, _ENDPOINT_A, "secret-token"):
                self.assertNotIn(private, message)




if __name__ == "__main__":
    unittest.main()
