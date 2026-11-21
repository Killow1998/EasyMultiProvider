import argparse
import base64
import http.client
import json
import os
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from urllib.parse import urlsplit

from engine import digest
from settings import ROOT
from verify import call, reply, text
from verify_features import CompactReply, LiveText
from verify_lifecycle import ControlledUpstream, JsonResponse, completed_response

INBOUND = "aero-inbound-fixture"
DELEGATED = "aero-delegated-fixture"


class Worker:
    def __init__(self, command):
        environment = {**os.environ, "PYTHONIOENCODING": "ascii"}
        if len(command) == 1:
            environment["PATH"] = ""
        self.process = subprocess.Popen(
            command,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        self.messages = queue.Queue()
        self.reader = threading.Thread(target=self.read)
        self.reader.start()
        hello = self.messages.get(timeout=5)
        assert hello == {
            "module": "aero",
            "version": "0.10.0-beta",
            "module_version": "0.10.0-beta",
            "protocol": 1,
            "host_api": 2,
        }, hello

    def read(self):
        for line in self.process.stdout:
            self.messages.put(json.loads(line))

    def rpc(self, command, **fields):
        self.process.stdin.write(
            json.dumps({"command": command, **fields}, ensure_ascii=False) + "\n"
        )
        self.process.stdin.flush()
        return self.messages.get(timeout=5)

    def eof(self):
        self.process.stdin.close()

    def finish(self):
        code = self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        error = self.process.stderr.read()
        assert code == 0, error
        assert not self.reader.is_alive()
        assert INBOUND not in error and DELEGATED not in error
        self.process.stdout.close()
        self.process.stderr.close()

    def abort(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()
        self.reader.join(timeout=2)


def request(
    endpoint, body, session, delegated=DELEGATED, path="/responses", caller="local"
):
    address = urlsplit(endpoint)
    connection = http.client.HTTPConnection(address.hostname, address.port, timeout=5)
    connection.request(
        "POST",
        address.path + path,
        json.dumps(body),
        {
            "Authorization": "Bearer " + INBOUND,
            "X-Aero-Caller-Id": caller,
            "X-Aero-Caller-Key": delegated,
            "session-id": session,
            "Content-Type": "application/json",
            "X-Codex-Turn-Metadata": json.dumps({"turn_id": "worker-fixture-turn"}),
        },
    )
    return connection, connection.getresponse()


def complete(endpoint, body, session):
    connection, response = request(endpoint, body, session)
    try:
        payload = response.read()
        assert response.status == 200, payload
        return json.loads(payload)
    finally:
        connection.close()


def start(worker, directory, upstream, profiles, fallback="unused-upstream-fixture"):
    result = worker.rpc(
        "aero_start",
        state_dir=str(directory),
        core_base_url=upstream.url.removesuffix("/v1"),
        inbound_key=INBOUND,
        upstream_key=fallback,
        profiles=profiles,
    )
    assert result["error"] is None, result
    return result["value"]["endpoint"]


def legacy_and_restart(command, directory):
    fixture = json.loads((ROOT / "fixtures/legacy-compacted-session.json").read_bytes())
    directory.mkdir()
    with sqlite3.connect(directory / "sessions.sqlite3") as database:
        database.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, state TEXT NOT NULL)"
        )
        database.execute(
            "INSERT INTO sessions VALUES (?,?)",
            (fixture["key"], json.dumps(fixture["state"])),
        )
    assert digest([fixture["session"], fixture["model"]]) == fixture["key"]
    profiles = {
        fixture["model"]: fixture["profile"],
        "aero/Renamed pair": fixture["profile"],
    }
    with ControlledUpstream(DELEGATED) as upstream:
        worker = Worker(command)
        try:
            assert (
                worker.rpc("read_panel")["value"].find("window.createAeroSettings") >= 0
            )
            endpoint = start(worker, directory, upstream, profiles)
            upstream.actions.put(
                JsonResponse(
                    reply(
                        [
                            call(
                                "enter_plan_mode",
                                {"reason": "Legacy verification is complete"},
                                1,
                            )
                        ]
                    )
                )
            )
            upstream.actions.put(JsonResponse(reply([text("LEGACY_CONTINUED_OK")])))
            result = complete(endpoint, fixture["request"], fixture["session"])
            assert "LEGACY_CONTINUED_OK" in json.dumps(result)
            assert [body["model"] for body in upstream.received] == [
                "executor-fixture",
                "planner-fixture",
            ]
            assert "LEGACY_CHECKPOINT" in json.dumps(upstream.received[0]["input"])
            assert "Update SPEC.txt" in json.dumps(upstream.received[0]["input"])
            assert "native-fixture-opaque" not in json.dumps(upstream.received)
            assert complete(endpoint, fixture["request"], fixture["session"]) == result
            assert len(upstream.received) == 2
            assert worker.rpc("aero_configure", profiles=profiles)["error"] is None
            assert complete(endpoint, fixture["request"], fixture["session"]) == result
            worker.eof()
            worker.finish()
        finally:
            worker.abort()
        worker = Worker(command)
        try:
            endpoint = start(worker, directory, upstream, profiles)
            assert complete(endpoint, fixture["request"], fixture["session"]) == result
            assert len(upstream.received) == 2
            worker.eof()
            worker.finish()
        finally:
            worker.abort()
        upstream.assert_clean()


def configure_and_drain(command, directory):
    profile = {
        "planner": {"model": "planner-original"},
        "executor": {"model": "executor-original"},
    }
    with ControlledUpstream(DELEGATED) as upstream:
        worker = Worker(command)
        live = LiveText()
        try:
            panel = worker.rpc("read_panel")
            assert panel["error"] is None and "data:image/svg+xml" in panel["value"]
            endpoint = start(worker, directory, upstream, {"aero/Pair": profile})
            assert worker.rpc("aero_start")["error"] == "aero.already_started"
            assert worker.rpc(
                "aero_configure",
                profiles={"aero/Pair": {"planner": {}, "executor": {}}},
            )["error"]
            upstream.actions.put(JsonResponse(reply([text("ORIGINAL_PROFILE_OK")])))
            complete(
                endpoint, {"model": "aero/Pair", "input": "First request"}, "first"
            )
            assert upstream.received[-1]["model"] == "planner-original"
            changed = {
                "planner": {"model": "planner-new", "effort": "low"},
                "executor": {"model": "executor-new"},
            }
            assert (
                worker.rpc("aero_configure", profiles={"aero/Pair": changed})["error"]
                is None
            )
            upstream.actions.put(live)
            connection, response = request(
                endpoint,
                {
                    "model": "aero/Pair",
                    "input": "Drain this accepted request",
                    "stream": True,
                },
                "drain",
            )
            try:
                assert response.status == 200
                while True:
                    line = response.readline()
                    assert line, "The worker did not deliver live text"
                    if (
                        line.startswith(b"data:")
                        and json.loads(line[5:]).get("delta") == "Hello "
                    ):
                        break
                assert upstream.received[-1]["model"] == "planner-new"
                assert upstream.received[-1]["reasoning"] == {"effort": "low"}
                worker.eof()
                try:
                    worker.process.wait(timeout=0.2)
                except subprocess.TimeoutExpired:
                    pass
                else:
                    raise AssertionError("EOF cancelled an accepted inference")
                live.release.set()
                terminal = completed_response(response.read().decode())
                assert "Hello world" in json.dumps(terminal)
            finally:
                live.release.set()
                connection.close()
            worker.finish()
            with sqlite3.connect(directory / "sessions.sqlite3") as database:
                state = json.loads(
                    database.execute(
                        "SELECT state FROM sessions WHERE id=?",
                        (digest(["drain", "aero/Pair"]),),
                    ).fetchone()[0]
                )
            assert state["finished"] and "Hello world" in json.dumps(state["response"])
            for path in directory.iterdir():
                if path.is_file():
                    contents = path.read_bytes()
                    assert (
                        INBOUND.encode() not in contents
                        and DELEGATED.encode() not in contents
                    )
        finally:
            live.release.set()
            worker.abort()
        upstream.assert_clean()


def empty_profiles_and_fallback(command, directory):
    with ControlledUpstream(DELEGATED) as upstream:
        worker = Worker(command)
        try:
            endpoint = start(worker, directory, upstream, {}, fallback=DELEGATED)
            assert (
                worker.rpc(
                    "aero_configure",
                    profiles={
                        "aero/空配对": {
                            "planner": {"model": "planner"},
                            "executor": {"model": "executor"},
                        }
                    },
                )["error"]
                is None
            )
            upstream.actions.put(JsonResponse(reply([text("FALLBACK_KEY_OK")])))
            address = urlsplit(endpoint)
            connection = http.client.HTTPConnection(
                address.hostname, address.port, timeout=5
            )
            connection.request(
                "POST",
                "/v1/responses",
                json.dumps({"model": "aero/空配对", "input": "Fallback credential"}),
                {
                    "Authorization": "Bearer " + INBOUND,
                    "session-id": "fallback",
                    "Content-Type": "application/json",
                },
            )
            response = connection.getresponse()
            assert (
                response.status == 200 and "FALLBACK_KEY_OK" in response.read().decode()
            )
            connection.close()
            worker.eof()
            worker.finish()
        finally:
            worker.abort()
        upstream.assert_clean()


def incremental_turn(command, directory):
    profile = {"planner": {"model": "planner"}, "executor": {"model": "executor"}}
    model = "aero/Pair"
    body = {
        "model": model,
        "stream": True,
        "tools": [
            {
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object"},
            }
        ],
    }

    def send(body, *, session="incremental", caller="device-a", status=200):
        connection, response = request(endpoint, body, session, caller=caller)
        try:
            payload = response.read()
            assert response.status == status, (response.status, payload)
            if status == 200:
                return payload.decode()
            assert response.getheader("Content-Type") == "application/json"
            assert json.loads(payload)["error"]["code"] == "previous_response_not_found"
        finally:
            connection.close()

    with ControlledUpstream(DELEGATED) as upstream:
        worker = Worker(command)
        try:
            endpoint = start(
                worker, directory, upstream, {model: profile, "aero/Other": profile}
            )
            upstream.actions.put(
                JsonResponse(
                    reply([call("exit_plan_mode", {"plan": "Verify fixture"}, 1)])
                )
            )
            upstream.actions.put(
                JsonResponse(
                    reply([call("shell_command", {"command": "verify fixture"}, 2)])
                )
            )
            first = {**body, "input": "Verify the fixture"}
            parent = completed_response(send(first))
            assert parent["output"][0]["name"] == "shell_command"
            tool_output = {
                "type": "function_call_output",
                "call_id": "call_2",
                "output": "Verified",
            }
            delta = {
                **body,
                "previous_response_id": parent["id"],
                "input": [tool_output],
            }
            send(delta, caller="device-b", status=400)
            send(delta, session="other-thread", status=400)
            send({**delta, "model": "aero/Other"}, status=400)
            send({**delta, "previous_response_id": "unknown-response"}, status=400)
            assert len(upstream.received) == 2

            upstream.actions.put(JsonResponse({**reply([]), "status": "failed"}))
            failed = [
                json.loads(line[6:])
                for line in send(delta).splitlines()
                if line.startswith("data: ")
            ]
            assert failed[-1]["type"] == "response.failed"
            failed_id = failed[0]["response"]["id"]
            assert failed_id != parent["id"]
            send({**delta, "previous_response_id": failed_id}, status=400)
            assert len(upstream.received) == 3

            upstream.actions.put(
                JsonResponse(
                    reply([call("enter_plan_mode", {"reason": "Verified"}, 3)])
                )
            )
            upstream.actions.put(JsonResponse(reply([text("INCREMENTAL_COMPLETE")])))
            result = completed_response(send(delta))
            assert "INCREMENTAL_COMPLETE" in json.dumps(result)
            assert [entry["model"] for entry in upstream.received] == [
                "planner",
                "executor",
                "executor",
                "executor",
                "planner",
            ]
            assert all(
                "previous_response_id" not in entry for entry in upstream.received
            )
            assert "Verify the fixture" in json.dumps(upstream.received[-1]["input"])
            assert tool_output in upstream.received[-1]["input"]
            send(delta, status=400)
            full = {
                **body,
                "input": [
                    {"role": "user", "content": first["input"]},
                    *parent["output"],
                    tool_output,
                ],
            }
            assert completed_response(send(full)) == result
            assert len(upstream.received) == 5

            checkpoint_text = "INCREMENTAL_CHECKPOINT"
            checkpoint = {
                "type": "compaction",
                "encrypted_content": "emp1:"
                + base64.urlsafe_b64encode(checkpoint_text.encode()).decode(),
            }
            upstream.actions.put(
                CompactReply({"id": "compact-incremental", "output": [checkpoint]})
            )
            followup = {
                **body,
                "previous_response_id": result["id"],
                "input": "Continue verification",
            }
            compacted = completed_response(
                send({**followup, "input": [{"type": "compaction_trigger"}]})
            )
            assert compacted["output"] == [checkpoint]
            send(followup, status=400)
            assert len(upstream.received) == 6
            upstream.actions.put(JsonResponse(reply([text("COMPACTION_CONTINUED")])))
            resumed = completed_response(
                send({**followup, "previous_response_id": compacted["id"]})
            )
            assert "COMPACTION_CONTINUED" in json.dumps(resumed)
            assert checkpoint_text in json.dumps(upstream.received[-1]["input"])
            assert len(upstream.received) == 7
            worker.eof()
            worker.finish()
        finally:
            worker.abort()
        upstream.assert_clean()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    command = (
        [str(args.binary.resolve())]
        if args.binary
        else [sys.executable, "-B", str(ROOT / "worker.py")]
    )
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="aero-worker-e2e-") as temporary:
        directory = Path(temporary)
        legacy_and_restart(command, directory / "legacy")
        configure_and_drain(command, directory / "drain")
        empty_profiles_and_fallback(command, directory / "empty")
        incremental_turn(command, directory / "incremental")
    print(
        json.dumps(
            {
                "module": "aero",
                "version": "0.10.0-beta",
                "worker": "passed",
                "legacy_compacted_session_and_restart": "passed",
                "live_configure_and_eof_drain": "passed",
                "delegated_and_fallback_credentials": "passed",
                "empty_profile_installation": "passed",
                "scoped_incremental_continuation": "passed",
                "seconds": round(time.monotonic() - started, 3),
            }
        )
    )
