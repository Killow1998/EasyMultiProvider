import copy
import json
import logging
import os
import signal
import subprocess
import sys
import tempfile
import threading
from pathlib import Path
from unittest.mock import patch

from codex import codex_command, write_catalog
from engine import (
    AeroError,
    Engine,
    call_output,
    normalize,
    output_text,
    tool_names,
    translate_read,
)
from gateway import ROOT, Store, create_server
from settings import gateway_key, load_config
from verify_lifecycle import ControlledUpstream, JsonResponse


def call(name, values, number):
    return {
        "type": "function_call",
        "call_id": "call_" + str(number),
        "name": name,
        "arguments": json.dumps(values),
    }


def reply(items, model="fake", usage=None):
    return {
        "model": model,
        "status": "completed",
        "output": items,
        "usage": usage or {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12},
    }


def text(value):
    return {
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": value}],
    }


def expect_error(action, fragment):
    try:
        action()
    except AeroError as error:
        assert fragment in str(error), str(error)
        return
    raise AssertionError("expected error: " + fragment)


def test_engine():
    config = json.loads((ROOT / "profiles.json").read_text())
    tools = [
        {"type": "function", "name": "shell_command", "parameters": {"type": "object"}},
        {
            "type": "namespace",
            "name": "multi_agent_v1",
            "tools": [{"type": "function", "name": "spawn_agent"}],
        },
        {
            "type": "namespace",
            "name": "functions",
            "tools": [
                {"type": "function", "name": "spawn_agent"},
                {"type": "function", "name": "apply_patch"},
            ],
        },
    ]
    body = {
        "model": "aero/sol-dsk",
        "input": [{"role": "user", "content": "Implement a project feature"}],
        "tools": tools,
        "reasoning": {"effort": "max"},
    }
    replies = [
        reply([call("exit_plan_mode", {"plan": "Implement and verify"}, 1)]),
        reply([call("shell_command", {"command": "build-project"}, 2)]),
        reply([call("enter_plan_mode", {"reason": "Build passed"}, 3)]),
        reply([text("Verified and complete")]),
    ]
    seen, saved, events = [], [], []

    def backend(request, cancelled):
        seen.append(copy.deepcopy(request))
        return replies.pop(0)

    engine = Engine(config, backend, saved.append)
    response, state = engine.run(body, observe=events.append)
    assert [x["model"] for x in seen] == ["gpt-6.1-sol", "deepseek/deepseek-v4-pro"]
    assert seen[0]["reasoning"] == {"effort": "high"}
    assert "reasoning" not in seen[1]
    assert tool_names(seen[1]["tools"]) == {
        "shell_command",
        "functions.apply_patch",
        "enter_plan_mode",
    }
    assert response["model"] == "aero/sol-dsk"
    assert response["usage"]["input_tokens"] == 20
    assert (
        len(response["output"]) == 1
        and response["output"][0]["name"] == "shell_command"
    )
    assert all(x.get("reasoning", {}).get("effort") != "max" for x in seen)
    repeated, _ = engine.run(body, state)
    assert repeated == response and len(seen) == 2
    continuation = {
        **body,
        "input": body["input"]
        + response["output"]
        + [call_output("call_2", "Build passed")],
    }
    final, final_state = engine.run(continuation, state)
    assert output_text(final["output"]) == "Verified and complete"
    assert final["output"][0]["phase"] == "final_answer"
    assert final_state["finished"]
    assert [x["model"] for x in seen] == [
        "gpt-6.1-sol",
        "deepseek/deepseek-v4-pro",
        "deepseek/deepseek-v4-pro",
        "gpt-6.1-sol",
    ]
    assert any(x.get("name") == "exit_plan_mode" for x in seen[-1]["input"])
    assert any(
        x.get("call_id") == "call_2" and x["type"] == "function_call_output"
        for x in seen[-1]["input"]
    )
    expect_error(
        lambda: engine.run({**body, "input": []}, final_state), "prefix changed"
    )
    expect_error(
        lambda: engine.run({**body, "previous_response_id": "x"}), "full visible input"
    )
    expect_error(
        lambda: normalize([{"type": "compaction", "encrypted_content": "x"}]),
        "encrypted compaction",
    )
    assert normalize([{"type": "reasoning", "encrypted_content": "private"}]) == []

    config["review_interval"] = 1
    seen.clear()
    replies[:] = [
        reply([call("exit_plan_mode", {"plan": "Implement"}, 1)]),
        reply([call("shell_command", {"command": "check"}, 2)]),
        reply([call("shell_command", {"command": "must-not-run"}, 3)]),
        reply([call("enter_plan_mode", {"reason": "Review checkpoint"}, 4)]),
        reply([text("Reviewed")]),
    ]
    first, checkpoint = engine.run(body)
    final, _ = engine.run(
        {
            **body,
            "input": body["input"] + first["output"] + [call_output("call_2", "ok")],
        },
        checkpoint,
        observe=events.append,
    )
    assert [x["name"] for x in seen[2]["tools"]] == ["enter_plan_mode"]
    assert any(x["kind"] == "tool_gate" for x in events)
    assert output_text(final["output"]) == "Reviewed"

    config["review_interval"] = 30
    seen.clear()
    replies[:] = [
        reply([call("exit_plan_mode", {"plan": "First plan"}, 1)]),
        reply([call("shell_command", {"command": "check"}, 2)]),
        reply(
            [
                call(
                    "enter_plan_mode",
                    {"reason": "Status before changed requirement"},
                    3,
                )
            ]
        ),
        reply([text("New requirement understood")]),
    ]
    first, state = engine.run(body)
    update = {"role": "user", "content": "Change the public API"}
    engine.run(
        {
            **body,
            "input": body["input"]
            + first["output"]
            + [call_output("call_2", "ok"), update],
        },
        state,
    )
    assert [x["name"] for x in seen[2]["tools"]] == ["enter_plan_mode"]
    assert "new user message" in seen[2]["input"][-1]["content"][0]["text"]

    read = call(
        "aero_read_file",
        {"path": "a; touch /tmp/point-aero-must-not-exist", "offset": 1, "limit": 10},
        7,
    )
    translated = translate_read(read, tools)
    command = json.loads(translated["arguments"])["command"]
    assert command == "sed -n '1,10p' -- 'a; touch /tmp/point-aero-must-not-exist'"
    seen.clear()
    replies[:] = [reply([read])]
    read_response, state = engine.run(body)
    assert read_response["output"][0]["name"] == "shell_command"
    assert state["history"][-1]["name"] == "aero_read_file"
    assert state["public"][-1]["name"] == "shell_command"

    seen.clear()
    replies[:] = [
        reply([call("exit_plan_mode", {"plan": "Implement and verify"}, 1)]),
        reply([call("shell_command", {"command": "check"}, 2)]),
    ]
    engine.run({**body, "model": "aero/astra-sol"})
    assert [x["model"] for x in seen] == ["gpt-6-astra", "gpt-6.1-sol"]

    seen.clear()
    replies[:] = [
        reply(
            [
                {
                    "type": "custom_tool_call",
                    "call_id": "blocked",
                    "name": "apply_patch",
                    "input": "must not execute",
                }
            ]
        ),
        reply([text("Read-only response")]),
    ]
    blocked, _ = engine.run({**body, "input": "Explain the planning phase"})
    assert all(x["type"] == "message" for x in blocked["output"])
    assert any(
        x["type"] == "custom_tool_call_output" and x.get("call_id") == "blocked"
        for x in seen[-1]["input"]
    )
    return {
        "engine": "passed",
        "covered": [
            "role routing",
            "both model pairings",
            "planner edit rejection",
            "effort isolation",
            "handoff schemas",
            "hidden history",
            "usage",
            "retry replay",
            "periodic tool gate",
            "user-message handback",
            "read translation",
            "unsupported history rejection",
        ],
    }


def test_no_task_cutoff():
    config = json.loads((ROOT / "profiles.json").read_text())
    calls, saved = [], []
    body = {
        "model": "aero/sol-dsk",
        "input": "Continue the project until reviewed completion",
    }

    def backend(request, cancelled):
        calls.append(request["model"])
        number = len(calls)
        if number == 33:
            return reply([text("Reviewed completion after continued work")])
        if request["model"] == config["models"][body["model"]]["planner"]["model"]:
            return reply(
                [
                    call(
                        "exit_plan_mode",
                        {"plan": "Continue the remaining work"},
                        number,
                    )
                ]
            )
        return reply(
            [
                call(
                    "enter_plan_mode",
                    {"reason": "Review new progress and continue"},
                    number,
                )
            ]
        )

    response, state = Engine(config, backend, saved.append).run(body)
    assert len(calls) == 33
    assert state["finished"] and len(saved) == 1
    assert output_text(response["output"]) == "Reviewed completion after continued work"
    assert all(item["type"] == "message" for item in response["output"])

    cancelled = threading.Event()
    calls.clear()
    saved.clear()

    def cancelling_backend(request, is_cancelled):
        result = backend(request, is_cancelled)
        if len(calls) == 17:
            cancelled.set()
        return result

    expect_error(
        lambda: Engine(config, cancelling_backend, saved.append).run(
            body, cancelled=cancelled.is_set
        ),
        "request cancelled",
    )
    assert len(calls) == 17 and not saved
    return {
        "continued_handoffs_without_cutoff": "passed",
        "user_cancellation_during_continued_work": "passed",
    }


def test_native_codex(directory):
    config = json.loads((ROOT / "profiles.json").read_text())
    steps = []

    def backend(body, cancelled):
        index = len(steps)
        assert all("spawn_agent" not in name for name in tool_names(body["tools"]))
        steps.append(
            {
                "model": body["model"],
                "input_items": len(body["input"]),
                "tool_names": sorted(tool_names(body["tools"])),
            }
        )
        if index == 0:
            return reply(
                [
                    call(
                        "aero_read_file",
                        {"path": "SPEC.txt", "offset": 1, "limit": 40},
                        1,
                    )
                ]
            )
        if index == 1:
            assert any(
                x.get("type") == "function_call_output"
                and "NATIVE_AERO_SPEC" in str(x.get("output"))
                for x in body["input"]
            )
            return reply(
                [
                    call(
                        "exit_plan_mode",
                        {
                            "plan": "Write verified.txt with NATIVE_AERO_VERIFIED, then run a shell check and hand back"
                        },
                        2,
                    )
                ]
            )
        if index == 2:
            shell = next(
                name
                for name in tool_names(body["tools"])
                if name.split(".")[-1] in ("exec_command", "shell_command")
            )
            command = "printf '%s\\n' NATIVE_AERO_VERIFIED > verified.txt\ntest \"$(cat verified.txt)\" = NATIVE_AERO_VERIFIED"
            arguments = (
                {"cmd": command, "max_output_tokens": 1000}
                if shell.split(".")[-1] == "exec_command"
                else {"command": command, "timeout_ms": 10000}
            )
            return reply(
                [
                    call(
                        shell,
                        arguments,
                        3,
                    )
                ]
            )
        if index == 3:
            assert any(
                x.get("type") == "function_call_output" and x.get("call_id") == "call_3"
                for x in body["input"]
            )
            return reply(
                [
                    call(
                        "enter_plan_mode",
                        {"reason": "Created verified.txt and shell check succeeded"},
                        4,
                    )
                ]
            )
        if index == 4:
            return reply(
                [
                    call(
                        "aero_read_file",
                        {"path": "verified.txt", "offset": 1, "limit": 10},
                        5,
                    )
                ]
            )
        if index == 5:
            assert any(
                x.get("type") == "function_call_output"
                and x.get("call_id") == "call_5"
                and "NATIVE_AERO_VERIFIED" in str(x.get("output"))
                for x in body["input"]
            )
            return reply([text("NATIVE_AERO_REVIEWED_OK")])
        raise AssertionError("unexpected native Codex request")

    server, store = create_server(
        config,
        directory / "gateway",
        backend_factory=lambda session: backend,
        token="synthetic-aero-key",
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    cwd = directory / "project"
    cwd.mkdir()
    (cwd / "SPEC.txt").write_text("NATIVE_AERO_SPEC\n")
    catalog = directory / "models.json"
    write_catalog(config, ROOT / "fixtures" / "catalog.json", catalog)
    (directory / "codex-home").mkdir()
    command = codex_command(
        "aero/sol-dsk",
        server.server_port,
        catalog,
        cwd,
        [
            "-c",
            "model_providers.aero.requires_openai_auth=false",
            "-c",
            "model_providers.aero.request_max_retries=0",
            "-c",
            "model_providers.aero.stream_max_retries=0",
            "-c",
            "features.plugins=false",
            "-c",
            "features.apps=false",
            "-c",
            "features.personality=false",
            "-c",
            "sqlite_home=" + json.dumps(str(directory / "codex-db")),
            "-c",
            "log_dir=" + json.dumps(str(directory / "codex-logs")),
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--skip-git-repo-check",
            "--sandbox",
            "workspace-write",
            "--json",
            "Follow SPEC.txt and complete the task using the aero workflow",
        ],
    )
    try:
        with subprocess.Popen(
            command,
            text=True,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
            env={
                **os.environ,
                "CODEX_HOME": str(directory / "codex-home"),
                "AERO_API_KEY": "synthetic-aero-key",
            },
        ) as process:
            try:
                stdout, stderr = process.communicate(timeout=90)
            finally:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            result = subprocess.CompletedProcess(
                command, process.returncode, stdout, stderr
            )
        (directory / "codex.jsonl").write_text(result.stdout)
        (directory / "codex.stderr").write_text(result.stderr)
        evidence = ROOT / "evidence"
        for name in ("codex.jsonl", "codex.stderr"):
            (evidence / name).write_text((directory / name).read_text())
        (evidence / "gateway-events.jsonl").write_text(
            (directory / "gateway" / "events.jsonl").read_text()
        )
        (evidence / "backend-steps.json").write_text(
            json.dumps(steps, ensure_ascii=False, indent=2) + "\n"
        )
        assert result.returncode == 0, result.stderr[-6000:] + result.stdout[-6000:]
        assert "NATIVE_AERO_REVIEWED_OK" in result.stdout, result.stdout[-6000:]
        assert (cwd / "verified.txt").read_text() == "NATIVE_AERO_VERIFIED\n"
        assert [x["model"] for x in steps] == [
            "gpt-6.1-sol",
            "gpt-6.1-sol",
            "deepseek/deepseek-v4-pro",
            "deepseek/deepseek-v4-pro",
            "gpt-6.1-sol",
            "gpt-6.1-sol",
        ]
        return {
            "native_codex": "passed",
            "backend": "scripted protocol fixture; no live model calls",
            "steps": steps,
        }
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        store.close()


def test_persistence(directory):
    config = json.loads((ROOT / "profiles.json").read_text())
    state_dir = directory / "persistence"
    store = Store(state_dir)
    body = {"model": "aero/astra-sol", "input": "Explain the design"}
    first, state = Engine(
        config,
        lambda request, cancelled: reply([text("Read-only answer")]),
        lambda state: store.put("session", state),
    ).run(body)
    store.close()
    store = Store(state_dir)
    seen = []
    try:
        replay, _ = Engine(
            config, lambda request, cancelled: seen.append(request), lambda state: None
        ).run(body, store.get("session"))
        assert replay == first and not seen
        return {"persistence_and_restart_replay": "passed"}
    finally:
        store.close()


def test_portable_settings(directory):
    config_path = directory / "profiles.json"
    config = json.loads((ROOT / "profiles.json").read_text())
    config["key_file"] = "caller.key"
    config_path.write_text(json.dumps(config))
    (directory / "caller.key").write_text("synthetic-file-key\n")
    settings = load_config(config_path)
    with patch.dict(os.environ, {}, clear=True):
        assert gateway_key(settings) == "synthetic-file-key"
        with patch.dict(os.environ, {"EMP_GATEWAY_KEY": "synthetic-env-key"}):
            assert gateway_key(settings) == "synthetic-env-key"
    config["models"]["aero/astra-sol"]["executor"]["model"] = "aero/sol-dsk"
    config_path.write_text(json.dumps(config))
    try:
        load_config(config_path)
    except ValueError as error:
        assert "non-aero EMP model" in str(error)
    else:
        raise AssertionError("recursive Aero profile was accepted")
    catalog = directory / "portable-catalog.json"
    write_catalog(settings, ROOT / "fixtures" / "catalog.json", catalog)
    assert {
        model["slug"] for model in json.loads(catalog.read_text())["models"]
    } == set(settings["models"])
    return {"portable_configuration_and_gateway_key": "passed"}


def test_native_launcher(directory):
    config = json.loads((ROOT / "profiles.json").read_text())
    cwd = directory / "launcher-project"
    cwd.mkdir()
    home = directory / "launcher-codex-home"
    home.mkdir()
    with ControlledUpstream("synthetic-launcher-key") as upstream:
        config["upstream"] = upstream.url
        profile = directory / "launcher-profiles.json"
        profile.write_text(json.dumps(config))
        upstream.actions.put(JsonResponse(reply([text("NATIVE_LAUNCHER_OK")])))
        result = subprocess.run(
            [
                sys.executable,
                "-B",
                str(ROOT / "codex.py"),
                "--model",
                "aero/sol-dsk",
                "--config",
                str(profile),
                "--catalog-template",
                str(ROOT / "fixtures" / "catalog.json"),
                "--runtime",
                str(directory / "launcher-runtime"),
                "--cwd",
                str(cwd),
                "--",
                "-c",
                "features.plugins=false",
                "-c",
                "features.apps=false",
                "exec",
                "--ephemeral",
                "--ignore-user-config",
                "--skip-git-repo-check",
                "--json",
                "Explain this experiment without using tools",
            ],
            env={
                **os.environ,
                "CODEX_HOME": str(home),
                "EMP_GATEWAY_KEY": "synthetic-launcher-key",
            },
            capture_output=True,
            text=True,
            timeout=90,
        )
        assert result.returncode == 0, result.stderr[-6000:] + result.stdout[-6000:]
        assert "NATIVE_LAUNCHER_OK" in result.stdout, result.stdout[-6000:]
        assert len(upstream.received) == 1
        assert upstream.received[0]["model"] == "gpt-6.1-sol"
        upstream.assert_clean()
    return {"native_launcher_with_gateway_key": "passed"}


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--native-codex", action="store_true")
    args = parser.parse_args()
    evidence = ROOT / "evidence"
    evidence.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="point-aero-", dir="/tmp") as scratch:
        directory = Path(scratch)
        results = {
            **test_engine(),
            **test_no_task_cutoff(),
            **test_persistence(directory),
            **test_portable_settings(directory),
        }
        if args.native_codex:
            results.update(test_native_codex(directory))
            results.update(test_native_launcher(directory))
            for name in ("codex.jsonl", "codex.stderr"):
                (evidence / name).write_text((directory / name).read_text())
        (
            evidence
            / (
                "verification-native.json"
                if args.native_codex
                else "verification-engine.json"
            )
        ).write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(results, ensure_ascii=False, indent=2))
    logging.shutdown()
