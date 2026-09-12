import copy
import unittest

from easy_multi_provider.collaboration_transport import (
    collaboration_summary, prepare_collaboration, restore_collaboration,
)
from easy_multi_provider.dialects import project_request


class CollaborationTransportTests(unittest.TestCase):
    def test_collision_has_a_content_free_error_code_and_namespace_counts(self):
        from easy_multi_provider.protocol_adapters import CodexNativeAdapter
        from easy_multi_provider.router_errors import RouterError
        from easy_multi_provider.server import _router_error_body
        from easy_multi_provider.transport_failures import failure_from_exception

        body = {"tools": None, "input": [{"type": "additional_tools", "tools": [
            {"type": "namespace", "name": "collaboration", "tools": [
                {"type": "function", "name": "spawn_agent", "parameters": {
                    "properties": {"message": {"type": "string", "encrypted": True,
                                              "description": "PRIVATE_DESCRIPTION"}}}}]}]}],
            "instructions": "PRIVATE_INSTRUCTIONS"}
        projected, _ = prepare_collaboration(body)
        self.assertEqual(collaboration_summary(body), {"native": 1, "emp": 0, "emp_in_history": 0})
        self.assertEqual(collaboration_summary(projected), {"native": 0, "emp": 1, "emp_in_history": 1})
        with self.assertRaises(RouterError) as raised:
            CodexNativeAdapter().project_request(
                {"auth_mode": "forward", "protocol": "responses"}, projected,
                {"_emp_plaintext_collaboration": True}, "test-model",
            )
        self.assertEqual(raised.exception.status, 422)
        self.assertEqual(failure_from_exception(raised.exception).failure_reason,
                         "collaboration_namespace_collision")
        response = _router_error_body(raised.exception)
        self.assertEqual(response["error"]["code"], "collaboration_namespace_collision")
        self.assertNotIn("PRIVATE", repr((response, collaboration_summary(projected))))

    def test_responses_lite_additional_tools_are_projected(self):
        body = {"tools": None, "input": [{"type": "additional_tools", "id": "at_original",
            "role": "developer", "tools": [{"type": "namespace", "name": "collaboration",
            "tools": [{"type": "function", "name": "spawn_agent", "parameters": {
                "properties": {"message": {"type": "string", "encrypted": True}}}}]}]}]}
        original = copy.deepcopy(body)
        result, changed = prepare_collaboration(body)
        self.assertTrue(changed)
        self.assertEqual(body, original)
        item = result["input"][0]
        self.assertEqual(item["tools"][0]["name"], "emp_collaboration")
        self.assertNotIn("encrypted", item["tools"][0]["tools"][0]["parameters"]["properties"]["message"])
        self.assertNotEqual(item["id"], "at_original")
        self.assertEqual(prepare_collaboration(body)[0], result)

    def test_encrypted_task_error_explains_recovery_without_leaking_task(self):
        from easy_multi_provider.protocol_adapters import PortableResponsesAdapter
        from easy_multi_provider.router_errors import RouterError

        with self.assertRaises(RouterError) as raised:
            PortableResponsesAdapter().project_request(
                {"protocol": "responses", "auth_mode": "api_key"},
                {"input": [{"type": "agent_message", "content": [
                    {"type": "encrypted_content", "encrypted_content": "secret-cipher"}]}]},
                {}, "test-model",
            )
        self.assertEqual(raised.exception.status, 422)
        message = str(raised.exception)
        self.assertIn("encrypted_agent_task_requires_plaintext", message)
        self.assertIn("request was not forwarded", message)
        self.assertIn("plaintext-capable", message)
        self.assertIn("retrying", message)
        self.assertNotIn("secret-cipher", message)

    def test_plaintext_round_trip_and_history(self):
        body = {"tools": [{"type": "namespace", "name": "collaboration", "tools": [
            {"type": "function", "name": "spawn_agent", "parameters": {"properties": {
                "message": {"type": "string", "encrypted": True},
                "model": {"type": "string"}}}}]}], "input": []}
        original = copy.deepcopy(body)
        request, changed = prepare_collaboration(body)
        self.assertTrue(changed)
        self.assertEqual(body, original)
        self.assertNotIn("encrypted", request["tools"][0]["tools"][0]["parameters"]["properties"]["message"])
        output = {"type": "function_call", "namespace": "emp_collaboration",
                  "name": "spawn_agent", "arguments": '{"message":"read file"}'}
        for event in ({"type": "response.output_item.done", "item": output},
                      {"type": "response.completed", "response": {"output": [output]}}):
            result = restore_collaboration(event)
            item = result.get("item") or result["response"]["output"][0]
            self.assertEqual(item["namespace"], "collaboration")
            self.assertEqual(item["encrypted_function_args"], [])
            body["input"] = [item]
            self.assertEqual(prepare_collaboration(body)[0]["input"][0]["namespace"], "emp_collaboration")
        child = project_request({"protocol": "responses", "auth_mode": "api_key"},
                                {"input": [{"type": "agent_message", "content": [
                                    {"type": "input_text", "text": "read file"}]}]})
        self.assertEqual(child["input"][0]["content"][0]["text"], "read file")

    def test_ciphertext_is_never_relabelled(self):
        native = {"type": "function_call", "namespace": "collaboration", "name": "spawn_agent",
                  "encrypted_function_args": ["message"], "arguments": "ciphertext"}
        self.assertEqual(restore_collaboration(native), native)
        bad = dict(native, namespace="emp_collaboration")
        with self.assertRaises(ValueError):
            restore_collaboration(bad)

    def test_no_collaboration_tools_is_unchanged(self):
        body = {"input": "hello", "tools": []}
        self.assertEqual(prepare_collaboration(body), (body, False))
