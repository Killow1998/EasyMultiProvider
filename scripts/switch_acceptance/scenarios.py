"""Observable same-thread continuity and real Codex subagent round trips."""
import hashlib
import json
import time
import uuid

from .environment import NATIVE


def completed_text(turn):
    return "\n".join(item.get("text", "") for item in turn.get("items", [])
                     if item.get("type") == "agentMessage")


def run_turn(client, thread, model, prompt, expected, report, output, child_model=None, child_result=None):
    before = len(client.events)
    started = time.monotonic()
    response = client.rpc("turn/start", {"threadId": thread, "model": model,
                          "input": [{"type": "text", "text": prompt}],
                          "approvalPolicy": "never"})
    turn_id = response["turn"]["id"]
    status = client.wait_turn(thread, timeout=240)
    final = client.rpc("thread/read", {"threadId": thread, "includeTurns": True})
    turn = next(row for row in final["thread"]["turns"] if row["id"] == turn_id)
    text = completed_text(turn)
    events = client.events[before:]
    methods = [event.get("method") for event in events]
    row = {"model": model, "status": status, "seconds": round(time.monotonic() - started, 3),
           "expected_retained": all(value in text for value in expected),
           "output_sha256": hashlib.sha256(text.encode()).hexdigest(),
           "tool_calls": [item.get("type") for item in turn.get("items", [])
                          if item.get("type") in ("collabAgentToolCall", "subAgentActivity")],
           "error_events": methods.count("error")}
    report.setdefault("turns", []).append(row)
    output.write_text(json.dumps(report, indent=2))
    print(json.dumps(row), flush=True)
    if status != "completed" or not row["expected_retained"] or row["error_events"]:
        # Full synthetic-only details remain private, never stdout.
        output.with_name("failed-turn.json").write_text(json.dumps(turn, indent=2))
        raise RuntimeError("Switching turn failed observable continuity checks")
    if child_model:
        starts = [item for item in turn.get("items", [])
                  if item.get("type") == "subAgentActivity" and item.get("kind") == "started"]
        if len(starts) != 1 or starts[0].get("model") != child_model:
            raise RuntimeError("Expected exactly one child using the requested model")
        child = client.rpc("thread/read", {"threadId": starts[0]["agentThreadId"], "includeTurns": True})["thread"]
        if not child["turns"] or child["turns"][-1]["status"] != "completed":
            raise RuntimeError("The actual spawned child did not complete")
        if child_result not in completed_text(child["turns"][-1]):
            raise RuntimeError("The actual child result does not match the parent's claim")
        row["child_model"] = child_model
        row["child_completed"] = True
        output.write_text(json.dumps(report, indent=2))
    return row


def scenario(client, project, external, report, output, subagents=True):
    result = client.rpc("thread/start", {"cwd": str(project), "model": external,
                        "approvalPolicy": "never", "sandbox": "read-only", "ephemeral": False})
    thread = result["thread"]["id"]
    code = "PARENT_" + uuid.uuid4().hex[:12]
    run_turn(client, thread, external, f"Remember the exact code {code}. Reply only with that code. Do not use tools.",
             [code], report, output)
    for model in (NATIVE, external):
        run_turn(client, thread, model, "Reply only with the exact code I asked you to remember earlier. Do not use tools.",
                 [code], report, output)
    if not subagents:
        return
    child = "CHILD_" + uuid.uuid4().hex[:12]
    prompt = (f"Use collaboration.spawn_agent exactly once with model {NATIVE!r}, fork_turns='none', "
              "task_name='native_probe'. Its task is: "
              f"Reply exactly {child!r}. "
              "Use no tools. As parent wait for completion, then reply with its actual result and the original remembered code. "
              "Do not repeat the child instructions as if they were a received result.")
    spawned = run_turn(client, thread, external, prompt, [code, child], report, output, NATIVE, child)
    if not spawned["tool_calls"]:
        raise RuntimeError("External parent claimed a child result without a collaboration tool call")
    for model in (NATIVE, external):
        run_turn(client, thread, model, "Reply with the original remembered code and the message received from native_probe. No tools.",
                 [code, child], report, output)
    other = "EXTERNAL_CHILD_" + uuid.uuid4().hex[:12]
    prompt = (f"Use collaboration.spawn_agent exactly once with model {external!r}, fork_turns='none', "
              f"task_name='external_probe', and task 'Reply exactly {other}; no tools'. "
              "Wait for its completed result, then reply with that result and the original remembered code.")
    spawned = run_turn(client, thread, NATIVE, prompt, [code, other], report, output, external, other)
    if not spawned["tool_calls"]:
        raise RuntimeError("Native parent claimed a child result without a collaboration tool call")
    run_turn(client, thread, external, "Reply with the original remembered code and external_probe's actual result. No tools.",
             [code, other], report, output)
