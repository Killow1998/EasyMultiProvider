"""Plaintext V2 delegation using Codex's explicit plaintext-call marker.

Only tool transport metadata is adapted. Existing encrypted tasks are never
decoded, discarded, or relabelled as plaintext.
"""

import copy
import json
import uuid


NAMESPACE = "emp_collaboration"
MESSAGE_TOOLS = {"spawn_agent", "send_message", "followup_task"}


def prepare_collaboration(body):
    result = copy.deepcopy(body)
    containers = [result]
    if isinstance(result.get("input"), list):
        containers.extend(item for item in result["input"]
                          if isinstance(item, dict) and item.get("type") == "additional_tools")
    tools = [tool for container in containers
             for tool in (container.get("tools") or [])]
    if any(tool.get("name") == NAMESPACE for tool in tools if isinstance(tool, dict)):
        raise ValueError("reserved EMP collaboration namespace collision")
    changed = False
    for tool in tools:
        if not isinstance(tool, dict) or tool.get("type") != "namespace" or tool.get("name") != "collaboration":
            continue
        tool["name"] = NAMESPACE
        changed = True
        for child in tool.get("tools", []):
            if child.get("name") in MESSAGE_TOOLS:
                message = child.get("parameters", {}).get("properties", {}).get("message")
                if isinstance(message, dict):
                    message.pop("encrypted", None)
    if changed:
        for container in containers[1:]:
            if any(isinstance(tool, dict) and tool.get("name") == NAMESPACE
                   for tool in (container.get("tools") or [])) and "id" in container:
                # Codex derives this ID from the tool schema. Changed schemas
                # must not reuse the original encrypted definition's identity.
                container["id"] = "at_" + uuid.uuid5(
                    uuid.NAMESPACE_URL,
                    str(container["id"]) + json.dumps(container["tools"], sort_keys=True),
                ).hex
        for item in result.get("input", []) if isinstance(result.get("input"), list) else []:
            if (isinstance(item, dict) and item.get("type") == "function_call"
                    and item.get("namespace") == "collaboration"
                    and item.get("encrypted_function_args") == []):
                item["namespace"] = NAMESPACE
    return result, changed


def restore_collaboration(value):
    """Restore only tool items emitted under our plaintext namespace."""
    if not isinstance(value, dict):
        return value
    # Text deltas dominate a stream. Leave them untouched rather than copying
    # every response event when only collaboration calls need translation.
    if value.get("type") not in {
        "function_call", "response.output_item.added", "response.output_item.done",
        "response.completed", "response.failed", "response.incomplete",
    } and "output" not in value:
        return value
    result = copy.deepcopy(value)
    def restore(item):
        if not isinstance(item, dict):
            return
        if item.get("type") == "function_call" and item.get("namespace") == NAMESPACE:
            item["namespace"] = "collaboration"
            if item.get("name") in MESSAGE_TOOLS:
                if item.get("encrypted_function_args"):
                    raise ValueError("unexpected encrypted collaboration arguments")
                item["encrypted_function_args"] = []
    restore(result)
    restore(result.get("item"))
    for container in (result, result.get("response", {})):
        if isinstance(container, dict):
            for item in container.get("output", []):
                restore(item)
    return result
