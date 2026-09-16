"""Regression cases from the September 2026 external protocol review."""
import copy
import json
import unittest
from dataclasses import replace

from easy_multi_provider.catalog import _account_entry, _external_entry
from easy_multi_provider.codex_history.models import HistoryAnchor, HistoryMismatchError
from easy_multi_provider.history_continuity import request_history_anchor
from easy_multi_provider.dialects import ProjectionError, project_request
from easy_multi_provider.protocol_projection import responses_to_chat, responses_to_anthropic
from easy_multi_provider.router_errors import RouterError
from easy_multi_provider.transport_continuity import (
    TransportContinuityAdapter, TransportContinuityDecision, TransportContinuityState,
)


class ReviewBoundaryTests(unittest.TestCase):
    def test_external_capabilities_do_not_depend_on_native_capabilities(self):
        model = {"id": "external/model"}
        native = {
            "slug": "native", "base_instructions": "Coding guidance",
            "supports_experimental_context": True,
            "future_capability": True, "comp_hash": "native-only",
            "tool_mode": "code_mode_only", "available_access_programs": ["private"],
            "model_messages": {"instructions_template": "Coding template",
                               "token_budget": {"enabled": True}, "future_mode": True},
        }
        original = copy.deepcopy(native)
        external = _external_entry(model, native, {})
        self.assertFalse(external["supports_experimental_context"])
        for field in ("future_capability", "comp_hash", "available_access_programs", "tool_mode"):
            self.assertNotIn(field, external)
        self.assertEqual(external["model_messages"], {"instructions_template": "Coding template"})
        alternative = dict(native, supports_experimental_context=False, future_capability=False)
        self.assertEqual(external, _external_entry(model, alternative, {}))
        self.assertEqual(native, original)
        account = _account_entry({"prefix": "other"}, native)
        self.assertNotIn("available_access_programs", account)
        self.assertTrue(account["supports_experimental_context"])

    def test_forks_share_cache_key_without_sharing_history_identity(self):
        for thread in ("parent", "child-a", "child-b"):
            headers = {"thread-id": thread, "session-id": "parent-cache"}
            body = {"client_metadata": {"x-codex-turn-metadata": json.dumps({
                "thread_id": thread, "session_id": "execution-" + thread,
                "window_id": "current", "turn_id": "turn",
            })}}
            original = copy.deepcopy(headers)
            self.assertEqual(request_history_anchor(body, headers).thread_id, thread)
            self.assertEqual(headers, original)
        with self.assertRaises(HistoryMismatchError):
            request_history_anchor(body, {"thread-id": "another", "session-id": "parent-cache"})

    def test_cache_only_identity_cannot_open_a_parent_history(self):
        for version in (None, "0.155.0", "0.154.0-alpha.1", "unknown"):
            headers = {"session-id": "parent-cache"}
            if version:
                headers["version"] = version
            self.assertIsNone(HistoryAnchor.from_headers(headers).thread_id)
        self.assertEqual(HistoryAnchor.from_headers({
            "session-id": "legacy-thread", "version": "0.153.0",
        }).thread_id, "legacy-thread")
        self.assertIsNone(HistoryAnchor.from_headers({
            "thread-id": "warmup-thread", "x-codex-turn-metadata": '{"turn_id":""}',
        }).turn_id)

    def test_different_tool_identities_never_silently_collapse(self):
        tool = {"type": "function", "name": "search", "parameters": {"type": "object"}}
        body = {"input": "hello", "tools": [
            {"type": "namespace", "name": name, "tools": [tool]}
            for name in ("repository-a", "repository-b")
        ]}
        original = copy.deepcopy(body)
        with self.assertRaises(ProjectionError) as raised:
            project_request({"protocol": "responses"}, body)
        self.assertEqual(raised.exception.failure_class, "tool_name_collision")
        self.assertNotIn("repository-a", str(raised.exception))
        for convert in (responses_to_chat, responses_to_anthropic):
            with self.assertRaises(RouterError) as raised:
                convert(body, "test-model")
            self.assertEqual(raised.exception.status, 422)
        self.assertEqual(body, original)
        self.assertEqual(project_request({"protocol": "responses", "auth_mode": "forward"}, body), body)

    def test_repeated_identical_tool_is_allowed_but_changed_definition_is_not(self):
        tool = {"type": "function", "name": "search", "parameters": {"type": "object"}}
        body = {"input": [], "tools": [tool, copy.deepcopy(tool)]}
        self.assertEqual(len(project_request({"protocol": "responses"}, body)["tools"]), 1)
        body["tools"][1]["parameters"] = {"type": "object", "required": ["query"]}
        with self.assertRaises(ProjectionError):
            project_request({"protocol": "responses"}, body)

    def test_explicit_scope_changes_require_full_request_but_turn_alone_does_not(self):
        state = TransportContinuityState("route", "route", "resp", "resp", True, True,
                                         ("thread", "window"), ("thread", "window"))
        adapter = TransportContinuityAdapter()
        request = {"previous_response_id": "resp", "client_metadata": {"turn_id": "new"}}
        self.assertEqual(adapter.decide(request, state), TransportContinuityDecision.CONTINUE_INCREMENTAL)
        for scope in (("fork", "window"), ("thread", "compacted"), (None, None)):
            changed = replace(state, current_scope=scope)
            self.assertEqual(adapter.decide(request, changed), TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND)
            self.assertEqual(adapter.decide({}, changed), TransportContinuityDecision.FULL_REQUEST)
