"""Request-local tool identities for protocols without Codex namespaces/search."""

from __future__ import annotations

import copy
import hashlib
import json
import re
from collections.abc import Mapping

from .router_errors import ExternalProtocolError, RouterError


class ExternalTools:
    """Translate at the external boundary; Codex owns discovery and execution."""

    def __init__(self):
        self.identities = {}
        self.search_name = None
        self.search_indices = set()
        self.search_ids = set()

    def _name(self, name, namespace=None, *, search=False):
        if not isinstance(name, str) or not name:
            raise RouterError("request projection failed: invalid tool name", 422)
        if namespace is not None and not isinstance(namespace, str):
            raise RouterError("request projection failed: invalid tool namespace", 422)
        identity = (namespace or None, name, search)
        if namespace or search:
            digest = hashlib.sha256(json.dumps(identity).encode()).hexdigest()[:32]
            label = re.sub(r"[^a-zA-Z0-9_-]", "_", (namespace + "_" if namespace else "") + name)
            alias = "emp_tool_" + label[:22] + "_" + digest
        else:
            alias = name
        previous = self.identities.setdefault(alias, identity)
        if previous != identity:
            raise RouterError("request projection failed: tool name collision", 422)
        if search:
            self.search_name = alias
        return alias

    def _definitions(self, tools, namespace=None, namespace_description=""):
        if not isinstance(tools, list):
            raise RouterError("request projection failed: invalid tools", 422)
        result = []
        for raw in tools:
            if not isinstance(raw, Mapping):
                raise RouterError("request projection failed: invalid tool definition", 422)
            kind = raw.get("type")
            if kind == "namespace":
                name = raw.get("name")
                if namespace or not isinstance(name, str) or not name:
                    raise RouterError("request projection failed: invalid tool namespace", 422)
                result.extend(self._definitions(
                    raw.get("tools"), name, str(raw.get("description") or ""),
                ))
                continue
            tool = copy.deepcopy(dict(raw))
            if kind == "tool_search":
                if tool.get("execution") != "client":
                    raise RouterError("external tool search requires client execution", 422)
                tool = {"type": "function", "name": self._name("tool_search", search=True),
                        "description": tool.get("description", ""),
                        "parameters": tool.get("parameters", {})}
            elif kind in {"function", "custom"}:
                definition = tool.get("function") if isinstance(tool.get("function"), dict) else tool
                if namespace:
                    # Keep the namespace's meaning visible to the external
                    # model as well as keeping its executable identity unique.
                    description = str(definition.get("description") or "")
                    definition["description"] = "\n".join(filter(None, (
                        namespace + "." + str(definition.get("name") or ""),
                        namespace_description, description,
                    )))
                definition["name"] = self._name(definition.get("name"), namespace)
                definition.pop("defer_loading", None)
                tool.pop("defer_loading", None)
            result.append(tool)
        return result

    def prepare(self, body):
        result = copy.deepcopy(body)
        definitions = self._definitions(result.get("tools", []))
        source = result.get("input")
        items = [source] if isinstance(source, Mapping) else source
        if isinstance(items, list):
            converted = []
            for item in items:
                if not isinstance(item, dict):
                    converted.append(item)
                    continue
                kind = item.get("type")
                if kind in {"additional_tools", "tool_search_output"}:
                    loaded = self._definitions(item.get("tools", []))
                    definitions.extend(loaded)
                    if kind == "additional_tools":
                        continue
                    if item.get("execution") != "client" or not item.get("call_id"):
                        raise RouterError("request projection failed: invalid tool search output", 422)
                    item = {"type": "function_call_output", "call_id": item["call_id"],
                            "output": json.dumps({"status": item.get("status"), "tools": loaded})}
                elif kind == "tool_search_call":
                    if (item.get("execution") != "client" or not item.get("call_id")
                            or not isinstance(item.get("arguments"), dict)):
                        raise RouterError("request projection failed: invalid tool search call", 422)
                    item = {"type": "function_call", "call_id": item["call_id"],
                            "name": self._name("tool_search", search=True),
                            "arguments": json.dumps(item["arguments"])}
                elif kind in {"function_call", "custom_tool_call", "function_call_output"} and item.get("name"):
                    item["name"] = self._name(item["name"], item.pop("namespace", None))
                converted.append(item)
            result["input"] = converted
        if "tools" in result or definitions:
            # Exact repetitions (including discovery history) are harmless. A
            # schema/type conflict for one identity remains an explicit error.
            unique = {}
            for tool in definitions:
                definition = tool.get("function", tool)
                key = definition.get("name") if isinstance(definition, dict) else None
                if key is None:
                    key = json.dumps(tool, sort_keys=True)
                if key in unique and unique[key] != tool:
                    raise RouterError("request projection failed: conflicting tool definitions", 422)
                unique[key] = tool
            result["tools"] = list(unique.values())
        choice = result.get("tool_choice")
        if isinstance(choice, dict):
            choices = choice.get("tools", []) if choice.get("type") == "allowed_tools" else [choice]
            for selected in choices:
                if selected.get("type") == "tool_search":
                    selected.update(type="function", name=self._name("tool_search", search=True))
                elif selected.get("name"):
                    selected["name"] = self._name(selected["name"], selected.pop("namespace", None))
            if choice.get("type") == "allowed_tools":
                allowed = {selected.get("name") for selected in choices}
                result["tools"] = [tool for tool in result.get("tools", [])
                                   if tool.get("function", tool).get("name") in allowed]
                result["tool_choice"] = choice.get("mode", "auto")
        return result

    def _restore_item(self, raw, *, partial=False):
        if not isinstance(raw, dict) or raw.get("type") not in {"function_call", "custom_tool_call"}:
            return raw
        identity = self.identities.get(raw.get("name"))
        if identity is None:
            return raw
        namespace, name, search = identity
        item = dict(raw)
        if not search:
            item["name"] = name
            if namespace:
                item["namespace"] = namespace
            return item
        try:
            arguments = {} if partial else json.loads(item.get("arguments", ""))
        except (TypeError, ValueError):
            raise ExternalProtocolError("external tool search returned invalid arguments") from None
        if not isinstance(arguments, dict):
            raise ExternalProtocolError("external tool search returned invalid arguments")
        item.update(type="tool_search_call", execution="client", arguments=arguments)
        item.pop("name", None)
        if isinstance(item.get("id"), str):
            item["id"] = "tsc_" + hashlib.sha256(item["id"].encode()).hexdigest()[:32]
        return item

    def restore_response(self, response):
        if not isinstance(response, dict) or not isinstance(response.get("output"), list):
            return response
        return dict(response, output=[self._restore_item(item) for item in response["output"]])

    def restore_event(self, event):
        event = dict(event)
        kind = event.get("type", "")
        item = event.get("item")
        if isinstance(item, dict):
            if self.search_name and item.get("name") == self.search_name:
                if event.get("output_index") is not None:
                    self.search_indices.add(event["output_index"])
                if item.get("id"):
                    self.search_ids.add(item["id"])
            event["item"] = self._restore_item(item, partial=kind == "response.output_item.added")
        if kind.startswith("response.function_call_arguments.") and (
            event.get("output_index") in self.search_indices or event.get("item_id") in self.search_ids
        ):
            # ToolSearchCall has object arguments and no function-argument delta
            # protocol. Codex dispatches its completed output item once.
            return None
        if isinstance(event.get("response"), dict):
            event["response"] = self.restore_response(event["response"])
        return event
