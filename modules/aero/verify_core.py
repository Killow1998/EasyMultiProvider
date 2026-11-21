import argparse
import base64
import contextlib
import hashlib
import http.client
import json
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlsplit

from engine import digest
from verify import call, reply, text
from verify_features import LiveText
from verify_lifecycle import (
    ControlledUpstream,
    JsonResponse,
    Stall,
    StreamResponse,
    completed_response,
)
from worker import VERSION

sys.path.insert(
    0, str(Path(__file__).resolve().parents[2] / "scripts" / "shadow_frontend")
)

from e2e import Browser
from e2e_official_modules import LOCAL_KEY, AppServer, Emp, fixture
from local_backend import request, wait_for
from module_release_fixture import Release, http_server


def inference(emp, body, session="core-fixture", path="/responses"):
    address = urlsplit(emp.base)
    connection = http.client.HTTPConnection(address.hostname, address.port, timeout=20)
    connection.request(
        "POST",
        "/v1" + path,
        json.dumps(body),
        {
            "Authorization": "Bearer " + LOCAL_KEY,
            "Content-Type": "application/json",
            "session-id": session,
            "version": "0.169.0",
            "originator": "codex_cli_rs",
        },
    )
    return connection, connection.getresponse()


def complete(emp, body, session="core-fixture", path="/responses", status=200):
    connection, response = inference(emp, body, session, path)
    try:
        payload = response.read()
        assert response.status == status, (response.status, payload.decode())
        return json.loads(payload)
    finally:
        connection.close()


def catalog(emp):
    return emp.api("/v1/models?client_version=0.169.0")["models"]


@contextlib.contextmanager
def websocket(emp, session):
    address = urlsplit(emp.base)
    with socket.create_connection((address.hostname, address.port), timeout=15) as sock:
        sock.sendall(
            (
                "GET /v1/responses HTTP/1.1\r\n"
                f"Host: {address.netloc}\r\nAuthorization: Bearer {LOCAL_KEY}\r\n"
                f"session-id: {session}\r\noriginator: codex_cli_rs\r\n"
                "Connection: upgrade\r\nUpgrade: websocket\r\n"
                "Sec-WebSocket-Version: 13\r\n"
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            ).encode()
        )
        with sock.makefile("rb") as source:
            status = source.readline()
            assert b" 101 " in status, status
            assert any(line == b"\r\n" for line in iter(source.readline, b""))

            def send(frame):
                payload = json.dumps(frame).encode()
                mask = b"test"
                size = len(payload)
                length = bytes([128 + size]) if size <= 125 else bytes([254]) + size.to_bytes(2, "big")
                sock.sendall(
                    bytes([129]) + length + mask
                    + bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
                )

            def receive():
                frame = source.read(2)
                assert len(frame) == 2, frame
                size = frame[1]
                if size in (126, 127):
                    size = int.from_bytes(source.read(2 if size == 126 else 8), "big")
                payload = source.read(size)
                assert frame[0] == 129, (frame, payload)
                return json.loads(payload)

            yield send, receive


def websocket_protocol(emp, upstream):
    body = {
        "type": "response.create",
        "model": "aero/Pair",
        "input": [{"role": "user", "content": "Compile the WebSocket fixture"}],
        "tools": [{"type": "function", "name": "shell_command", "parameters": {"type": "object"}}],
    }
    session = "aero-websocket"
    before = len(upstream.received)
    terminal = {"response.completed", "response.incomplete", "response.failed", "error"}

    def expect(receive, kind):
        while True:
            event = receive()
            if event["type"] == kind:
                return event
            assert event["type"] not in terminal, event

    with websocket(emp, session) as (send, receive):
        send({**body, "generate": False})
        assert expect(receive, "response.completed")["response"]["id"] == ""
        assert len(upstream.received) == before
        upstream.actions.put(StreamResponse(reply([call("exit_plan_mode", {"plan": "Compile"}, 1)])))
        upstream.actions.put(StreamResponse(reply([call("shell_command", {"command": "fixture"}, 2)])))
        send(body)
        first = expect(receive, "response.completed")["response"]
        assert first["output"][0]["call_id"] == "call_2"
        delta = {**body,
                 "previous_response_id": first["id"],
                 "input": [{"type": "function_call_output", "call_id": "call_2", "output": "Fixture built"}]}
        stalled = Stall("sse", upstream.cleanup)
        upstream.actions.put(stalled)
        send(delta)
        interrupted = expect(receive, "response.created")["response"]["id"]
        assert stalled.received.wait(4)
        send({"type": "response.interrupt", "response_id": interrupted, "mode": "discard_partial_items"})
        result = expect(receive, "response.incomplete")["response"]
        assert result["id"] == interrupted and result["incomplete_details"]["reason"] == "interrupted"
        assert stalled.closed.wait(6)
        send({**delta, "previous_response_id": interrupted})
        assert expect(receive, "error")["error"]["code"] == "previous_response_not_found"
        assert len(upstream.received) == before + 3

    with websocket(emp, "unrelated-aero-session") as (send, receive):
        send(delta)
        assert expect(receive, "error")["error"]["code"] == "previous_response_not_found"
        assert len(upstream.received) == before + 3

    with websocket(emp, session) as (send, receive):
        live = LiveText()
        upstream.actions.put(live)
        upstream.actions.put(StreamResponse(reply([text("REVIEWED_AFTER_WEBSOCKET")])))
        send(delta)
        try:
            assert expect(receive, "response.output_text.delta")["delta"] == "Hello "
        finally:
            live.release.set()
        second = expect(receive, "response.completed")["response"]
        assert "Hello world" in json.dumps(second)
        assert "REVIEWED_AFTER_WEBSOCKET" in json.dumps(second)
        assert "Fixture built" in json.dumps(upstream.received[-1])
        assert first["id"] not in json.dumps(upstream.received[-1])
        send(delta)
        assert expect(receive, "error")["error"]["code"] == "previous_response_not_found"
        assert len(upstream.received) == before + 5

        compact_body = {
            **body,
            "input": [
                *body["input"], *first["output"], *delta["input"], *second["output"],
            ],
        }
        compact_body.pop("type")
        upstream.actions.put(JsonResponse(reply([text("CHECKPOINT: fixture successfully built")])))
        compacted = complete(emp, compact_body, session, path="/responses/compact")
        assert compacted["output"][0]["type"] == "compaction"
        upstream.actions.put(StreamResponse(reply([text("RESUMED_AFTER_COMPACTION")])))
        send({**body, "input": [*compacted["output"], {"role":"user","content":"Report the result"}]})
        completed = expect(receive, "response.completed")["response"]
        assert "RESUMED_AFTER_COMPACTION" in json.dumps(completed)
        assert "CHECKPOINT" in json.dumps(upstream.received[-1])
        assert len(upstream.received) == before + 7


def codex_websocket(stack, emp, upstream, codex, output):
    model = "aero/Pair"
    version = subprocess.check_output(
        [str(codex), "--version"], env=emp.env, text=True
    ).split()[-1]
    models = emp.home / "models.json"
    models.write_text(json.dumps(emp.api("/v1/models?client_version=" + version)))
    (emp.home / "config.toml").write_text(
        f'model={json.dumps(model)}\nmodel_provider="aero_fixture"\n'
        f'model_catalog_json={json.dumps(str(models))}\n'
        'approval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
        '[features]\nplugins=false\napps=false\nmemories=false\n'
        '[model_providers.aero_fixture]\nname="Aero fixture"\nwire_api="responses"\n'
        f'base_url={json.dumps(emp.base + "/v1")}\nenv_key="EMP_FIXTURE_AUTH"\n'
        'supports_websockets=true\nrequest_max_retries=0\nstream_max_retries=0\n'
    )
    client = AppServer(codex, {**emp.env, "EMP_FIXTURE_AUTH": LOCAL_KEY}, output / "codex.log")
    stack.callback(client.close)
    thread = client.rpc("thread/start", {"model": model, "cwd": str(emp.root)})
    def websocket_completions():
        ids = set()
        for log in (emp.root / "state" / "logs").glob("*.jsonl"):
            for line in log.read_text().splitlines():
                record = json.loads(line)
                fields = record.get("fields", {})
                if (
                    record["event"] == "request_finished"
                    and fields.get("route") == "aero"
                    and fields.get("transport") == "websocket"
                    and fields.get("downstream_terminal") == "completed"
                    and fields.get("last_phase") == "execute_and_relay"
                ):
                    ids.add(fields["request_id"])
        return ids

    prior_completions = websocket_completions()
    before = len(upstream.received)
    (emp.root / "SPEC.txt").write_text("CODEX_AERO_WEBSOCKET_TOOL_RESULT\n")
    upstream.actions.put(StreamResponse(reply([
        call("aero_read_file", {"path": "SPEC.txt", "offset": 1, "limit": 10}, 1),
    ])))
    upstream.actions.put(StreamResponse(reply([text("CODEX_AERO_WEBSOCKET_COMPLETE")])))
    fixture.turn(client, thread["thread"]["id"], "Verify the Aero WebSocket model")
    assert len(upstream.received) == before + 2
    assert any(
        item.get("call_id") == "call_1" and "CODEX_AERO_WEBSOCKET_TOOL_RESULT" in str(item.get("output"))
        for item in upstream.received[-1]["input"]
    )
    assert "CODEX_AERO_WEBSOCKET_COMPLETE" in json.dumps(client.events)
    wait_for(lambda: len(websocket_completions() - prior_completions) == 2)
    client.close()
    return version


def active(emp):
    return next(
        row for row in emp.api("/api/modules")["modules"] if row["id"] == "aero"
    )


def browser_pairing(stack, root, emp, output):
    browser = Browser(stack, root)
    request(browser.base + "/url", {"url": emp.url})
    wait_for(
        lambda: browser.execute(
            "return document.querySelector('#aero_open')?.hidden === false"
        )
    )
    browser.execute("document.querySelector('#aero_open').click()")
    wait_for(
        lambda: browser.execute("return !!document.querySelector('[data-aero-add]')")
    )
    browser.execute("document.querySelector('[data-aero-add]').click()")
    browser.execute(
        """
        const name=document.querySelector('#aero_name');
        name.value='中文配对'; name.dispatchEvent(new Event('input'));
        for(const role of ['planner','executor']){
          const select=document.querySelector('#aero_'+role);
          const option=[...select.options].find(option=>option.value.toLowerCase().endsWith('/'+role));
          if(!option)throw Error('Missing fixture role '+role);
          select.value=option.value; select.dispatchEvent(new Event('change'));
        }
        document.querySelector('#modal_submit').click();
        """
    )
    wait_for(
        lambda: any(
            profile["name"] == "中文配对"
            for profile in emp.api("/api/config")["aero_profiles"]
        )
    )
    wait_for(
        lambda: browser.execute("return !!document.querySelector('[data-aero-edit]')")
    )
    browser.execute("document.querySelector('[data-aero-edit]').click()")
    assert (
        browser.execute("return document.querySelector('#aero_name').value") == "Pair"
    )
    browser.execute("document.querySelector('#modal_back').click()")
    assert browser.execute("return !!document.querySelector('[data-aero-add]')")
    screenshot = request(browser.base + "/screenshot")[1]["value"]
    (output / "aero-panel.png").write_bytes(base64.b64decode(screenshot))
    assert not browser.diagnostics(), browser.diagnostics()
    browser.execute("document.querySelector('#modal_close').click()")
    source_label = browser.execute(
        """
        const row = document.querySelector('#catalog_display_models [data-id="aero/fixture"]')
          .closest('.display-row');
        row.scrollIntoView({block: 'center'});
        return row.querySelector('code').textContent;
        """
    )
    assert source_label == "Fixture", source_label
    screenshot = request(browser.base + "/screenshot")[1]["value"]
    (output / "aero-catalog.png").write_bytes(base64.b64decode(screenshot))
    return source_label


def protocol(emp, upstream):
    before = len(upstream.received)
    body = {
        "model": "aero/Pair",
        "input": [{"role": "user", "content": "Implement SPEC.txt and verify"}],
        "tools": [
            {
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object"},
            }
        ],
    }
    upstream.actions.put(
        StreamResponse(
            reply([call("exit_plan_mode", {"plan": "Implement and verify"}, 1)])
        )
    )
    upstream.actions.put(
        StreamResponse(
            reply([call("shell_command", {"command": "verify SPEC.txt"}, 2)])
        )
    )
    first = complete(emp, body)
    assert any(item.get("name") == "shell_command" for item in first["output"]), first
    assert [entry["model"] for entry in upstream.received[before:]] == [
        "gpt-6.1-sol",
        "gpt-6.1-terra",
    ], upstream.received
    compact_body = {
        **body,
        "input": body["input"]
        + first["output"]
        + [{"type": "function_call_output", "call_id": "call_2", "output": "Verified"}],
    }
    upstream.actions.put(
        JsonResponse(reply([text("CHECKPOINT: SPEC.txt implemented and verified")]))
    )
    compacted = complete(emp, compact_body, path="/responses/compact")
    assert compacted["output"][0]["type"] == "compaction", compacted
    assert complete(emp, compact_body, path="/responses/compact") == compacted
    assert len(upstream.received) == before + 3
    continued_body = {
        **body,
        "input": compacted["output"] + [{"role": "user", "content": "Finish"}],
    }
    upstream.actions.put(
        StreamResponse(reply([call("enter_plan_mode", {"reason": "Final review"}, 3)]))
    )
    upstream.actions.put(StreamResponse(reply([text("CORE_AERO_COMPLETE")])))
    continued = complete(emp, continued_body)
    assert "CORE_AERO_COMPLETE" in json.dumps(continued)
    assert "CHECKPOINT" in json.dumps(upstream.received[-1])
    assert complete(emp, continued_body) == continued
    assert len(upstream.received) == before + 5
    return continued_body, continued


def legacy_session(emp, upstream, fixture):
    directory = emp.root / "aero"
    directory.mkdir(exist_ok=True)
    with sqlite3.connect(directory / "sessions.sqlite3") as database:
        database.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, state TEXT NOT NULL)"
        )
        database.execute(
            "INSERT INTO sessions VALUES (?,?)",
            (fixture["key"], json.dumps(fixture["state"])),
        )
    before = len(upstream.received)
    upstream.actions.put(
        StreamResponse(
            reply([call("enter_plan_mode", {"reason": "Legacy final review"}, 3)])
        )
    )
    upstream.actions.put(StreamResponse(reply([text("LEGACY_CORE_COMPLETE")])))
    response = complete(emp, fixture["request"], fixture["session"])
    assert "LEGACY_CORE_COMPLETE" in json.dumps(response), response
    assert "LEGACY_CHECKPOINT" in json.dumps(upstream.received[before])
    assert fixture["state"]["plan"] in upstream.received[before]["instructions"]
    assert "native-fixture-opaque" not in json.dumps(upstream.received[before:])
    assert complete(emp, fixture["request"], fixture["session"]) == response
    assert len(upstream.received) == before + 2


def disable_during_stream(emp, upstream):
    live = LiveText()
    upstream.actions.put(live)
    connection, response = inference(
        emp, {"model": "aero/Pair", "input": "Show live text", "stream": True}, "drain"
    )
    try:
        assert response.status == 200, response.read()
        while line := response.readline():
            if (
                line.startswith(b"data:")
                and json.loads(line[5:]).get("delta") == "Hello "
            ):
                break
        else:
            raise AssertionError("Core withheld the first text delta")
        started = time.monotonic()
        emp.api("/api/modules", {"id": "aero", "action": "disable"})
        assert not active(emp)["active"]
        assert not any(row["slug"].startswith("aero/") for row in catalog(emp))
        denied = complete(
            emp, {"model": "aero/Pair", "input": "New request"}, "denied", status=404
        )
        assert denied["error"]["code"] == "module_disabled", denied
        live.release.set()
        terminal = completed_response(response.read().decode())
        assert "Hello world" in json.dumps(terminal), terminal
        return round(time.monotonic() - started, 3)
    finally:
        live.release.set()
        connection.close()


def verify(core, module, output, browser_enabled, codex=None):
    output.mkdir(parents=True, exist_ok=True)
    with contextlib.ExitStack() as stack:
        root = Path(
            stack.enter_context(tempfile.TemporaryDirectory(prefix="aero-core-e2e-"))
        )
        release = http_server(stack, Release)
        release.version = subprocess.check_output(
            [str(core), "--version"], text=True
        ).split()[-1]
        release.assets = {}
        release.module_releases = {
            "emp-mod-aero-api2-v" + VERSION: {module.name: module.read_bytes()}
        }
        release.corrupt = None
        upstream = stack.enter_context(ControlledUpstream("aero-provider-fixture"))
        roles = {
            "planner": {"model": "fixture/planner", "effort": "low"},
            "executor": {"model": "fixture/executor", "effort": "high"},
        }
        legacy = json.loads(
            (
                Path(__file__).parent / "fixtures/legacy-compacted-session.json"
            ).read_text()
        )
        emp = Emp(
            stack,
            core,
            root / "emp",
            {
                "providers": [
                    {
                        "id": "fixture",
                        "name": "Fixture",
                        "base_url": upstream.url,
                        "api_key": "aero-provider-fixture",
                        "auth_mode": "api_key",
                        "protocol": "responses",
                    }
                ],
                "models": [
                    {
                        "id": "fixture/planner",
                        "provider": "fixture",
                        "upstream_id": "gpt-6.1-sol",
                    },
                    {
                        "id": "fixture/executor",
                        "provider": "fixture",
                        "upstream_id": "gpt-6.1-terra",
                    },
                ],
                "aero_profiles": [
                    {"id": "aero/fixture", "name": "Pair", **roles},
                    {"id": "aero/legacy", "name": "Legacy pair", **roles},
                ],
            },
            f"http://127.0.0.1:{release.server_port}",
        )
        assert not active(emp)["active"]
        assert not any(row["slug"].startswith("aero/") for row in catalog(emp))
        emp.api("/api/gateway", expected=404)
        installed = emp.install("aero")
        assert active(emp)["active"], installed
        assert next(row for row in catalog(emp) if row["slug"] == "aero/Pair")["prefer_websockets"]
        assert not any(
            row["active"]
            for row in emp.api("/api/modules")["modules"]
            if row["id"] != "aero"
        )
        source_label = (
            browser_pairing(stack, root, emp, output) if browser_enabled else None
        )
        legacy_session(emp, upstream, legacy)
        try:
            body, expected = protocol(emp, upstream)
            websocket_protocol(emp, upstream)
        finally:
            (output / "upstream.json").write_text(
                json.dumps(upstream.received, indent=2)
            )
            upstream.assert_clean()
        drain_seconds = disable_during_stream(emp, upstream)
        with sqlite3.connect(emp.root / "aero" / "sessions.sqlite3") as database:
            state = json.loads(
                database.execute(
                    "SELECT state FROM sessions WHERE id=?",
                    (digest(["drain", "aero/Pair"]),),
                ).fetchone()[0]
            )
            assert state["finished"] and "Hello world" in json.dumps(state["response"])
        emp.api("/api/modules", {"id": "aero", "action": "enable"})
        assert active(emp)["active"]
        count = len(upstream.received)
        assert complete(emp, body) == expected
        assert len(upstream.received) == count
        assert not any(
            row["id"] == "gateway" and row["installed_version"]
            for row in emp.api("/api/modules")["modules"]
        )
        upstream.assert_clean()
        codex_version = codex_websocket(stack, emp, upstream, codex, output) if codex else None
        upstream.assert_clean()
        (output / "upstream.json").write_text(json.dumps(upstream.received, indent=2))
        core_hash = hashlib.sha256()
        with core.open("rb") as executable:
            for block in iter(lambda: executable.read(1024 * 1024), b""):
                core_hash.update(block)
        return {
            "verified_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "module": "aero",
            "version": VERSION,
            "core_version": release.version,
            "core_sha256": core_hash.hexdigest(),
            "module_sha256": hashlib.sha256(module.read_bytes()).hexdigest(),
            "upstream": "synthetic",
            "binary_install_and_catalog": "passed",
            "gateway_not_installed": "passed",
            "planner_executor_tool_chain": "passed",
            "legacy_native_compacted_session": "passed",
            "compact_continue_replay": "passed",
            "websocket_warmup_tools_interrupt_reconnect_compact": "passed",
            "codex_websocket": codex_version,
            "live_text_disable_drain_and_reenable": "passed",
            "disable_and_drain_seconds": drain_seconds,
            "browser_create_edit_back": "passed" if browser_enabled else "not run",
            "browser_aero_source_label": source_label,
            "upstream_requests": len(upstream.received),
        }


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--core", type=Path, required=True)
    parser.add_argument("--module", type=Path, required=True)
    parser.add_argument(
        "--output", type=Path, default=Path(__file__).parent / "evidence/core-linux"
    )
    parser.add_argument("--no-browser", action="store_true")
    parser.add_argument("--codex", type=Path)
    arguments = parser.parse_args()
    result = verify(
        arguments.core.resolve(),
        arguments.module.resolve(),
        arguments.output,
        not arguments.no_browser,
        arguments.codex.resolve() if arguments.codex else None,
    )
    (arguments.output / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))
