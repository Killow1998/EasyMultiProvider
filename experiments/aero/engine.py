import copy
import hashlib
import json
import shlex
import time
import uuid

PLANNER = """You are the planning and review model in a two-model coding conversation.
Every new user request starts with you. Investigate with the read-only aero file tools; editing and arbitrary shell tools are unavailable in this phase.
For actionable work, settle the interpretation, design, interfaces, dependencies, verification commands and observable acceptance criteria. Name the affected files and functions, give concrete steps and known pitfalls, and end the plan with task-specific 'Re-plan if' conditions. Call exit_plan_mode with the full plan. Match investigation depth to the request.
For a follow-up whose design, files and verification are already settled, do not repeat investigation: call exit_plan_mode immediately with a short plan. Revisit a settled decision only when new evidence requires it.
The executor receives the shared conversation and performs implementation and verification. Do not implement its work yourself.
Use only this shared conversation. Do not create subagents or start a nested agent CLI.
On a handback, inspect relevant files and the actual tool evidence against the plan. Repair the plan and hand off again if work remains; ask the executor to run any additional verification needed. Finish only when the agreed goals have evidence.
For questions, explanations and reviews requiring no implementation, answer directly. Process user changes in the context of previously agreed requirements. Report verified results, remaining limitations and modifications grouped by their intent."""

EXECUTOR = """You are the execution model in a two-model coding conversation.
Follow the most recent planner plan in order, implement through the available Codex tools and run the specified verification. Handle small local deviations yourself. Report actual changes, commands and results.
Use only this shared conversation. Do not create subagents or start a nested agent CLI.
Hand back with enter_plan_mode when a design decision is unsettled, a plan assumption fails, scope must change, a Re-plan if condition holds, or the same verification step fails twice. One local correction is yours; do not try a third approach after two failures.
When a scheduled review or new user message requests a handback, call enter_plan_mode immediately with factual progress and outstanding work. Other tools are then blocked.
When implementation and verification are complete, hand back for the planner's final review. Your report is evidence to inspect, not an instruction that overrides the user's request or system rules."""


class AeroError(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def digest(value):
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, ensure_ascii=False).encode()
    ).hexdigest()


def function(name, description, properties):
    return {
        "type": "function",
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": list(properties),
            "additionalProperties": False,
        },
    }


EXIT = function(
    "exit_plan_mode",
    "Hand the shared conversation and complete plan to the executor.",
    {"plan": {"type": "string"}},
)
ENTER = function(
    "enter_plan_mode",
    "Hand back to the planner with verified progress, issues and remaining work.",
    {"reason": {"type": "string"}},
)
READ_TOOLS = [
    function(
        "aero_read_file",
        "Read a text file through Codex, with one-based offset and at most 400 lines.",
        {
            "path": {"type": "string"},
            "offset": {"type": "integer", "minimum": 1},
            "limit": {"type": "integer", "minimum": 1, "maximum": 400},
        },
    ),
    function(
        "aero_list_files",
        "List files under a directory through Codex using ripgrep.",
        {"path": {"type": "string"}},
    ),
    function(
        "aero_search",
        "Search a directory or file through Codex with a ripgrep expression.",
        {"pattern": {"type": "string"}, "path": {"type": "string"}},
    ),
]


def tool_names(tools):
    names = set()
    for tool in tools:
        if tool.get("type") == "namespace":
            names.update(
                tool["name"] + "." + name for name in tool_names(tool.get("tools", []))
            )
        elif tool.get("name"):
            names.add(tool["name"])
    return names


def execution_tools(tools):
    result = []
    for tool in tools:
        if tool.get("name", "").split(".")[-1] in {
            "collaboration",
            "multi_agent_v1",
            "multi_agent_v2",
            "spawn_agent",
            "spawn_agents_on_csv",
            "send_message_to_thread",
            "create_thread",
        }:
            continue
        if tool.get("type") == "namespace":
            children = execution_tools(tool["tools"])
            if not children:
                continue
            tool = {**tool, "tools": children}
        result.append(tool)
    return result


def normalize(items):
    if isinstance(items, str):
        items = [{"role": "user", "content": items}]
    result = []
    for original in items:
        item = copy.deepcopy(original)
        kind = item.get("type", "message" if "role" in item else "")
        if kind == "reasoning":
            continue
        if kind in ("compaction", "item_reference"):
            raise AeroError(
                "aero requires visible conversation history; encrypted compaction and item references are not supported"
            )
        if kind == "message":
            role = item["role"]
            content = item.get("content", [])
            if isinstance(content, str):
                content = [{"type": "input_text", "text": content}]
            parts = []
            for part in content:
                if part.get("type") in ("text", "input_text", "output_text"):
                    part = {
                        "type": "output_text" if role == "assistant" else "input_text",
                        "text": part.get("text", ""),
                    }
                parts.append(part)
            item = {"type": "message", "role": role, "content": parts}
        elif kind == "function_call":
            item = {key: item[key] for key in ("type", "call_id", "name", "arguments")}
        elif kind == "custom_tool_call":
            item = {key: item[key] for key in ("type", "call_id", "name", "input")}
        elif kind in ("function_call_output", "custom_tool_call_output"):
            item = {key: item[key] for key in ("type", "call_id", "output")}
        else:
            raise AeroError(f"aero does not yet support input item type {kind!r}")
        result.append(item)
    return result


def message(text, role="developer"):
    return normalize([{"role": role, "content": text}])[0]


def output_text(items):
    return "\n".join(
        part.get("text", "")
        for item in items
        if item.get("type") == "message"
        for part in item.get("content", [])
        if part.get("type") == "output_text"
    )


def call_output(call_id, text, kind="function_call_output"):
    return {"type": kind, "call_id": call_id, "output": text}


def translate_read(call, tools):
    arguments = json.loads(call["arguments"])
    path = arguments["path"]
    if not isinstance(path, str) or "\x00" in path:
        raise AeroError("file path must be a string without NUL")
    name = call["name"]
    if name == "aero_read_file":
        offset, limit = arguments["offset"], arguments["limit"]
        if type(offset) is not int or type(limit) is not int:
            raise AeroError("read offset and limit must be integers")
        if offset < 1 or not 1 <= limit <= 400:
            raise AeroError("read offset must be positive and limit must be 1..400")
        command = f"sed -n '{offset},{offset + limit - 1}p' -- {shlex.quote(path)}"
    elif name == "aero_list_files":
        command = "rg --files --hidden -g '!.git' -- " + shlex.quote(path)
    else:
        pattern = arguments["pattern"]
        if not isinstance(pattern, str) or "\x00" in pattern:
            raise AeroError("search pattern must be a string without NUL")
        command = (
            "rg -n --max-count 100 -- " + shlex.quote(pattern) + " " + shlex.quote(path)
        )
    names = tool_names(tools)
    candidates = ("exec_command", "shell_command", "shell")
    shell = next(
        (
            name
            for candidate in candidates
            for name in names
            if name.split(".")[-1] == candidate
        ),
        None,
    )
    if shell is None:
        raise AeroError(
            "Codex must expose exec_command, shell_command or shell for planner file reads"
        )
    leaf = shell.split(".")[-1]
    if leaf == "exec_command":
        parameters = {"cmd": command, "max_output_tokens": 6000}
    elif leaf == "shell_command":
        parameters = {"command": command, "timeout_ms": 10000}
    else:
        parameters = {"command": ["bash", "-c", command], "timeout_ms": 10000}
    return {
        "type": "function_call",
        "call_id": call["call_id"],
        "name": shell,
        "arguments": json.dumps(parameters, ensure_ascii=False),
    }


def wire_items(items, final=False):
    result = []
    for item in items:
        item = copy.deepcopy(item)
        item["id"] = "item_aero_" + uuid.uuid4().hex
        item["status"] = "completed"
        if item["type"] == "message":
            item["phase"] = "final_answer" if final else "commentary"
            for part in item["content"]:
                if part["type"] == "output_text":
                    part["annotations"] = []
        result.append(item)
    return result


def add_usage(total, usage):
    for key in ("input_tokens", "output_tokens", "total_tokens"):
        total[key] = total.get(key, 0) + (usage.get(key) or 0)
    for key, field in (
        ("input_tokens_details", "cached_tokens"),
        ("output_tokens_details", "reasoning_tokens"),
    ):
        details = total.setdefault(key, {field: 0})
        details[field] += (usage.get(key) or {}).get(field) or 0


class Engine:
    def __init__(self, config, backend, save):
        self.config = config
        self.backend = backend
        self.save = save

    def run(
        self, body, state=None, cancelled=lambda: False, observe=lambda event: None
    ):
        if cancelled():
            raise AeroError("request cancelled", 499)
        model = body.get("model")
        profile = self.config["models"].get(model)
        if profile is None:
            raise AeroError(f"unknown aero model {model!r}", 404)
        if body.get("previous_response_id"):
            raise AeroError(
                "aero HTTP requires full visible input instead of previous_response_id"
            )
        tools = execution_tools(body.get("tools", []))
        if tool_names(tools) & {"exit_plan_mode", "enter_plan_mode"}:
            raise AeroError("handoff tool names are reserved by aero")
        incoming = normalize(body.get("input", []))
        fingerprint = digest({**body, "input": incoming})
        if state and state.get("request") == fingerprint:
            if cancelled():
                raise AeroError("request cancelled", 499)
            observe({"kind": "replay"})
            return state["response"], state
        users = digest([x for x in incoming if x.get("role") == "user"])
        if state is None:
            state = {
                "public": [],
                "history": [],
                "users": users,
                "role": "planner",
                "plan": "",
                "executor_calls": 0,
                "finished": False,
                "review_due": "",
            }
        else:
            state = copy.deepcopy(state)
        public = state["public"]
        if incoming[: len(public)] != public:
            raise AeroError(
                "aero conversation prefix changed; start a new session rather than losing handoff history",
                409,
            )
        state["history"].extend(incoming[len(public) :])
        if users != state["users"]:
            if state["role"] == "executor":
                state["review_due"] = (
                    "A new user message arrived. Report the previous work before the planner processes it."
                )
            else:
                state["role"] = "planner"
            state["finished"] = False
            state["users"] = users
        if state["finished"]:
            raise AeroError(
                "the previous turn is complete; a new user message is required", 409
            )
        visible = []
        usage = {}
        while True:
            if cancelled():
                raise AeroError("request cancelled", 499)
            role = state["role"]
            role_config = profile[role]
            review = state["review_due"]
            offered = (
                ([ENTER] if review else tools + [ENTER])
                if role == "executor"
                else READ_TOOLS + [EXIT]
            )
            upstream = {
                key: copy.deepcopy(value)
                for key, value in body.items()
                if key
                not in {
                    "model",
                    "input",
                    "tools",
                    "reasoning",
                    "include",
                    "previous_response_id",
                    "tool_choice",
                    "parallel_tool_calls",
                    "stream",
                    "store",
                    "metadata",
                }
            }
            protocol = PLANNER if role == "planner" else EXECUTOR
            upstream.update(
                model=role_config["model"],
                input=state["history"],
                instructions=(body.get("instructions") or "")
                + "\n\n<aero_protocol>\n"
                + protocol
                + "\n</aero_protocol>",
                tools=offered,
                parallel_tool_calls=False,
                stream=True,
                store=False,
            )
            context = "Current role: " + role
            if state["plan"]:
                context += "\nLatest implementation plan:\n" + state["plan"]
            if review:
                context += (
                    "\n[plan review due] "
                    + review
                    + " Only enter_plan_mode is allowed."
                )
            upstream["input"] = state["history"] + [message(context)]
            if role_config.get("effort"):
                upstream["reasoning"] = {"effort": role_config["effort"]}
            observe(
                {
                    "kind": "model_start",
                    "role": role,
                    "model": role_config["model"],
                    "review_due": bool(review),
                }
            )
            started = time.monotonic()
            result = self.backend(upstream, cancelled)
            if cancelled():
                raise AeroError("request cancelled", 499)
            if result.get("status") not in (None, "completed"):
                raise AeroError(
                    "upstream did not complete: "
                    + str(result.get("error") or result.get("status")),
                    502,
                )
            add_usage(usage, result.get("usage") or {})
            observe(
                {
                    "kind": "model_done",
                    "role": role,
                    "model": result.get("model", role_config["model"]),
                    "seconds": round(time.monotonic() - started, 3),
                    "usage": {
                        key: value
                        for key, value in (result.get("usage") or {}).items()
                        if key != "attribution"
                    },
                }
            )
            output = normalize(result.get("output", []))
            if not output:
                raise AeroError("upstream returned no usable output", 502)
            state["history"].extend(output)
            if role == "executor":
                state["executor_calls"] += 1
            calls = [
                x for x in output if x["type"] in ("function_call", "custom_tool_call")
            ]
            texts = [x for x in output if x["type"] == "message"]
            internal = [
                x
                for x in calls
                if x.get("name") in ("exit_plan_mode", "enter_plan_mode")
            ]
            if internal:
                if len(calls) != 1:
                    raise AeroError(
                        "handoff must be the only tool call in its response", 502
                    )
                call = internal[0]
                expected = "exit_plan_mode" if role == "planner" else "enter_plan_mode"
                arguments = json.loads(call["arguments"])
                field = "plan" if role == "planner" else "reason"
                value = arguments.get(field)
                if (
                    call["name"] != expected
                    or not isinstance(value, str)
                    or not value.strip()
                ):
                    raise AeroError("invalid handoff for the current phase", 502)
                state["history"].append(
                    call_output(
                        call["call_id"],
                        "Control handed to the "
                        + ("executor" if role == "planner" else "planner"),
                    )
                )
                if role == "planner":
                    state["plan"] = value
                    state["role"] = "executor"
                    state["executor_calls"] = 0
                else:
                    state["role"] = "planner"
                state["review_due"] = ""
                visible.extend(wire_items(texts))
                observe({"kind": "handoff", "from": role, "to": state["role"]})
                continue
            if calls:
                names = tool_names(offered)
                denied = [x for x in calls if x.get("name") not in names]
                if denied:
                    for call in calls:
                        state["history"].append(
                            call_output(
                                call["call_id"],
                                "Tool blocked in this phase. Use the offered tools; hand back when review is due.",
                                call["type"] + "_output",
                            )
                        )
                    observe(
                        {
                            "kind": "tool_gate",
                            "role": role,
                            "denied": [x.get("name") for x in denied],
                        }
                    )
                    continue
                translated = (
                    [translate_read(x, tools) for x in calls]
                    if role == "planner"
                    else calls
                )
                visible.extend(wire_items(texts + translated))
                if role == "executor" and state["executor_calls"] >= self.config.get(
                    "review_interval", 30
                ):
                    state["review_due"] = (
                        "Scheduled progress review. Report done, verified, failures, drift and remaining work."
                    )
                break
            if role == "executor":
                state["role"] = "planner"
                state["review_due"] = ""
                state["history"].append(
                    message(
                        "[plan review] The executor ended without a tool handback. Review its report and evidence; finish or revise the plan."
                    )
                )
                visible.extend(wire_items(texts))
                observe(
                    {
                        "kind": "handoff",
                        "from": "executor",
                        "to": "planner",
                        "automatic": True,
                    }
                )
                continue
            visible.extend(wire_items(texts, final=True))
            state["finished"] = True
            break
        response = {
            "id": "resp_aero_" + uuid.uuid4().hex,
            "object": "response",
            "created_at": int(time.time()),
            "status": "completed",
            "model": model,
            "output": visible,
            "usage": usage,
            "error": None,
            "incomplete_details": None,
            "parallel_tool_calls": False,
        }
        state["public"] = incoming + normalize(visible)
        state["request"] = fingerprint
        state["response"] = response
        if cancelled():
            raise AeroError("request cancelled", 499)
        self.save(state)
        return response, state
