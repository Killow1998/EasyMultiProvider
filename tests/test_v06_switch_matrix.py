import unittest

from easy_multi_provider.dialects import (
    CODEX_NATIVE,
    PORTABLE_RESPONSES,
    classify_dialect,
    project_request,
)


CURRENT_LOGIN = {
    "protocol": "responses",
    "auth_mode": "forward",
}
IMPORTED_SUBSCRIPTION = {
    "protocol": "responses",
    "auth_mode": "account",
    "account": {"id": "imported-subscription"},
}
EXTERNAL_PROVIDER_ONE = {"protocol": "responses", "auth_mode": "api_key"}
EXTERNAL_PROVIDER_TWO = {
    "protocol": "responses",
    "auth_mode": "api_key",
    "base_url": "https://external-two.example/v1",
}


def _text_parts(item):
    return [
        part["text"]
        for part in item.get("content", [])
        if isinstance(part, dict) and isinstance(part.get("text"), str)
    ]


class V06SwitchMatrixTests(unittest.TestCase):
    """Destination-projection baseline for the seven required route transitions.

    This is a dialect-boundary tracer, not proof of route/auth selection:
    source fixtures are classified but project_request consumes only the
    destination provider shape.
    """

    def test_switch_matrix_preserves_visible_history_at_dialect_boundary(self):
        cases = [
            ("N->S", CURRENT_LOGIN, IMPORTED_SUBSCRIPTION, CODEX_NATIVE, CODEX_NATIVE),
            ("S->N", IMPORTED_SUBSCRIPTION, CURRENT_LOGIN, CODEX_NATIVE, CODEX_NATIVE),
            ("N->E", CURRENT_LOGIN, EXTERNAL_PROVIDER_ONE, CODEX_NATIVE, PORTABLE_RESPONSES),
            ("E->N", EXTERNAL_PROVIDER_ONE, CURRENT_LOGIN, PORTABLE_RESPONSES, CODEX_NATIVE),
            ("S->E", IMPORTED_SUBSCRIPTION, EXTERNAL_PROVIDER_ONE, CODEX_NATIVE, PORTABLE_RESPONSES),
            ("E->S", EXTERNAL_PROVIDER_ONE, IMPORTED_SUBSCRIPTION, PORTABLE_RESPONSES, CODEX_NATIVE),
            (
                "E1->E2",
                EXTERNAL_PROVIDER_ONE,
                EXTERNAL_PROVIDER_TWO,
                PORTABLE_RESPONSES,
                PORTABLE_RESPONSES,
            ),
        ]
        private_reasoning = "route-private-reasoning"

        self.assertEqual(
            {CURRENT_LOGIN["auth_mode"], IMPORTED_SUBSCRIPTION["auth_mode"]},
            {"forward", "account"},
        )
        self.assertNotEqual(CURRENT_LOGIN, IMPORTED_SUBSCRIPTION)
        self.assertNotIn(
            "account",
            CURRENT_LOGIN,
        )
        self.assertIn("account", IMPORTED_SUBSCRIPTION)

        plain_fixture = [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "route-visible-user"}],
            },
            {
                "type": "reasoning",
                "content": [{"type": "reasoning_text", "text": private_reasoning}],
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "route-visible-final"}],
            },
        ]
        tool_fixture = [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "route-visible-user"}],
            },
            {
                "type": "function_call",
                "call_id": "call_route_fixture",
                "name": "route_fixture_tool",
                "arguments": "{}",
            },
            {
                "type": "function_call_output",
                "call_id": "call_route_fixture",
                "output": "route-visible-tool-result",
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "route-visible-final"}],
            },
        ]

        for name, source, destination, source_dialect, destination_dialect in cases:
            with self.subTest(transition=name):
                self.assertEqual(classify_dialect(source), source_dialect)

                for fixture_name, history in (
                    ("plain-visible", plain_fixture),
                    ("tool-pair", tool_fixture),
                ):
                    body = {"model": "source/model", "input": history}
                    projected = project_request(destination, body)

                    self.assertEqual(classify_dialect(destination), destination_dialect)
                    self.assertNotIn(private_reasoning, str(projected))
                    self.assertNotIn(
                        "reasoning",
                        [item.get("type") for item in projected["input"]],
                    )

                    self.assertEqual(
                        [item["type"] for item in projected["input"]],
                        [item["type"] for item in history if item["type"] != "reasoning"],
                    )

                    messages = [
                        item for item in projected["input"] if item["type"] == "message"
                    ]
                    self.assertEqual(
                        [text for item in messages for text in _text_parts(item)],
                        ["route-visible-user", "route-visible-final"],
                    )

                    if fixture_name == "tool-pair":
                        calls = [
                            item
                            for item in projected["input"]
                            if item["type"] == "function_call"
                        ]
                        outputs = [
                            item
                            for item in projected["input"]
                            if item["type"] == "function_call_output"
                        ]
                        self.assertEqual(len(calls), 1)
                        self.assertEqual(len(outputs), 1)
                        self.assertEqual(calls[0]["call_id"], "call_route_fixture")
                        self.assertEqual(outputs[0]["call_id"], "call_route_fixture")
                        self.assertEqual(
                            projected["input"].index(calls[0]) + 1,
                            projected["input"].index(outputs[0]),
                        )


if __name__ == "__main__":
    unittest.main()
