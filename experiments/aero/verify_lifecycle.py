import copy
import http.client
import json
import queue
import select
import socket
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from engine import AeroError, Engine, digest
from gateway import ROOT, Cancellation, create_server

TOKEN = "synthetic-lifecycle-token"


def completed_response(data):
    values = [
        json.loads(line[5:].strip())
        for line in data.splitlines()
        if line.startswith("data:")
    ]
    terminal = [
        value["response"] for value in values if value["type"] == "response.completed"
    ]
    assert len(terminal) == 1
    return terminal[0]


def synthetic_config(upstream_url=""):
    roles = {
        "planner": {"model": "planner-local"},
        "executor": {"model": "executor-local"},
    }
    return {
        "upstream": upstream_url,
        "review_interval": 30,
        "models": {"aero/lifecycle": roles},
    }


def response(output):
    return {
        "model": "controlled-upstream",
        "status": "completed",
        "output": output,
        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4},
    }


def message(value):
    return {
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": value}],
    }


def function_call(call_id="call_lifecycle"):
    return {
        "type": "function_call",
        "call_id": call_id,
        "name": "shell_command",
        "arguments": json.dumps(
            {"command": "test -f lifecycle.txt", "timeout_ms": 1000}
        ),
    }


def close_connection(handler):
    handler.close_connection = True


class JsonResponse:
    def __init__(self, value):
        self.value = value

    def handle(self, handler, body):
        payload = json.dumps(self.value).encode()
        handler.send_response(200)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(payload)))
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.wfile.write(payload)
        handler.wfile.flush()


class StreamResponse:
    def __init__(self, value):
        self.value = value

    def handle(self, handler, body):
        event = {"type": "response.completed", "response": self.value}
        payload = (
            "event: response.completed\ndata: " + json.dumps(event) + "\n\n"
        ).encode()
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Connection", "close")
        handler.end_headers()
        handler.wfile.write(payload)
        handler.wfile.flush()


class Stall:
    def __init__(self, kind, cleanup):
        self.kind = kind
        self.cleanup = cleanup
        self.release = threading.Event()
        self.received = threading.Event()
        self.sent = threading.Event()
        self.closed = threading.Event()

    def write_result(self, handler):
        value = response([function_call()])
        if self.kind == "sse":
            StreamResponse(value).handle(handler, {})
        else:
            JsonResponse(value).handle(handler, {})

    def wait_for_end(self, handler):
        while not self.cleanup.is_set():
            readable, _, _ = select.select([handler.connection], [], [], 0.02)
            if readable:
                try:
                    if not handler.connection.recv(1, socket.MSG_PEEK):
                        return True
                except OSError:
                    return True
            if self.release.is_set():
                self.write_result(handler)
                return False
        return True

    def handle(self, handler, body):
        self.received.set()
        if self.kind == "json":
            handler.send_response(200)
            handler.send_header("Content-Type", "application/json")
            handler.send_header("Content-Length", "100000")
            handler.send_header("Connection", "close")
            handler.end_headers()
            handler.wfile.write(b'{"status":"completed","output":[')
            handler.wfile.flush()
            self.sent.set()
        elif self.kind == "sse":
            handler.send_response(200)
            handler.send_header("Content-Type", "text/event-stream")
            handler.send_header("Connection", "close")
            handler.end_headers()
            handler.wfile.write(b'data: {"type":"response.completed","response":')
            handler.wfile.flush()
            self.sent.set()
        if self.wait_for_end(handler):
            self.closed.set()


class ControlledUpstream:
    def __init__(self, token):
        self.token = token
        self.actions = queue.Queue()
        self.received = []
        self.errors = []
        self.connections = set()
        self.cleanup = threading.Event()
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format, *args):
                pass

            def do_POST(self):
                close_connection(self)
                owner.add_connection(self.connection)
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    body = json.loads(self.rfile.read(size))
                    if self.headers.get("Authorization") != "Bearer " + owner.token:
                        self.send_error(401)
                        return
                    owner.record(body)
                    action = owner.next_action()
                    action.handle(self, body)
                except (BrokenPipeError, ConnectionResetError):
                    if not owner.cleanup.is_set():
                        owner.record_error("upstream connection interrupted")
                except Exception as error:
                    owner.record_error(repr(error))
                finally:
                    owner.remove_connection(self.connection)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = False
        self.server.block_on_close = True
        self.thread = threading.Thread(target=self.server.serve_forever)

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def add_connection(self, connection):
        with self.lock:
            self.connections.add(connection)

    def remove_connection(self, connection):
        with self.lock:
            self.connections.discard(connection)

    def record(self, body):
        with self.lock:
            self.received.append(body)

    def record_error(self, error):
        with self.lock:
            self.errors.append(error)

    def next_action(self):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline and not self.cleanup.is_set():
            try:
                return self.actions.get(timeout=0.05)
            except queue.Empty:
                continue
        raise AssertionError("controlled upstream has no request action")

    def connection_count(self):
        with self.lock:
            return len(self.connections)

    def received_count(self):
        with self.lock:
            return len(self.received)

    def assert_clean(self):
        with self.lock:
            errors = list(self.errors)
        assert not errors, errors

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.cleanup.set()
        with self.lock:
            connections = list(self.connections)
        for connection in connections:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class ServeExit(Exception):
    pass


def wait_event(event, label, timeout=2):
    assert event.wait(timeout), f"timed out waiting for {label}"


def wait_until(predicate, label, timeout=2):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.01)
    raise AssertionError(f"timed out waiting for {label}")


def call_bounded(action, label, timeout=2):
    errors = []
    done = threading.Event()

    def run_action():
        try:
            action()
        except BaseException as error:
            errors.append(error)
        finally:
            done.set()

    thread = threading.Thread(target=run_action, daemon=True)
    thread.start()
    assert done.wait(timeout), f"timed out waiting for {label}"
    if errors:
        raise errors[0]


def start_gateway(config, directory):
    server, store = create_server(config, directory, token=TOKEN)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    return server, store, thread


def stop_gateway(server, store, thread):
    try:
        if thread.is_alive():
            server.shutdown()
    finally:
        try:
            server.server_close()
            thread.join(timeout=2)
        finally:
            store.close()
        assert not thread.is_alive(), "gateway serving thread did not stop"


def open_request(server, body, session, authorization=None):
    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
    payload = json.dumps(body).encode()
    connection.request(
        "POST",
        "/v1/responses",
        payload,
        {
            "Authorization": authorization or "Bearer " + TOKEN,
            "Content-Type": "application/json",
            "Content-Length": str(len(payload)),
            "session_id": session,
        },
    )
    return connection


def disconnect(connection, downstream_response=None):
    sock = connection.sock
    if sock is not None:
        try:
            sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
    if downstream_response is not None:
        downstream_response.close()
    connection.close()


def complete_request(server, body, session, authorization=None):
    connection = open_request(server, body, session, authorization)
    response_headers = None
    try:
        response_headers = connection.getresponse()
        data = response_headers.read().decode()
        return response_headers.status, data
    finally:
        disconnect(connection, response_headers)


def engine_body():
    return {
        "model": "aero/lifecycle",
        "input": [{"role": "user", "content": "Implement lifecycle reliability"}],
        "tools": [
            {
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object", "properties": {}},
            }
        ],
        "stream": False,
    }


def seed_executor(config, session):
    seed_body = engine_body()
    replies = [
        response(
            [
                {
                    "type": "function_call",
                    "call_id": "call_plan",
                    "name": "exit_plan_mode",
                    "arguments": json.dumps({"plan": "Verify lifecycle"}),
                }
            ]
        ),
        response([function_call("call_seed")]),
    ]
    first, state = Engine(
        config,
        lambda request, cancelled: replies.pop(0),
        lambda value: None,
    ).run(seed_body)
    key = digest([session, seed_body["model"]])
    continuation = {
        **seed_body,
        "input": seed_body["input"]
        + first["output"]
        + [
            {
                "type": "function_call_output",
                "call_id": "call_seed",
                "output": "seed complete",
            }
        ],
    }
    return key, state, continuation, seed_body


def active_requests(server):
    with server.lifecycle:
        return server.active_requests


def cancellation_count(server):
    with server.lifecycle:
        return len(server.cancellations)


def handler_threads(server):
    with server.lifecycle:
        return len(server.threads)


def direct_boundaries(config):
    first, peer = socket.socketpair()
    transport = type(
        "Transport",
        (),
        {
            "sock": first,
            "closed": False,
            "close": lambda self: setattr(self, "closed", True),
        },
    )()
    cancellation = Cancellation()
    assert cancellation.attach(transport)
    cancellation.cancel()
    assert transport.closed
    first.close()
    peer.close()

    published, observer = socket.socketpair()
    stopped = Cancellation()
    stopped.cancel()
    assert not stopped.publish(published)
    observer.settimeout(1)
    assert observer.recv(1) == b""
    published.close()
    observer.close()

    successful_body = {
        "model": "aero/lifecycle",
        "input": "Persist a successful response",
    }
    _, successful_state = Engine(
        config,
        lambda request, cancelled: response([message("cached result")]),
        lambda state: None,
    ).run(successful_body)
    replay_backend = []
    replay_saves = []
    checks = 0

    def cancel_during_replay():
        nonlocal checks
        checks += 1
        return checks > 1

    try:
        Engine(
            config,
            lambda request, cancelled: replay_backend.append(request),
            replay_saves.append,
        ).run(
            successful_body,
            state=successful_state,
            cancelled=cancel_during_replay,
        )
    except AeroError as error:
        assert error.status == 499, error
    else:
        raise AssertionError("replay succeeded after cancellation")
    assert checks == 2 and not replay_backend and not replay_saves

    key, state, body, replay_body = seed_executor(config, "direct")
    backend_calls = []
    saves = []
    try:
        Engine(
            config,
            lambda request, cancelled: backend_calls.append(request),
            saves.append,
        ).run(replay_body, state=state, cancelled=stopped)
    except AeroError as error:
        assert error.status == 499, error
    else:
        raise AssertionError("cancelled persisted replay succeeded")
    assert not backend_calls and not saves

    backend_saves = []
    cancelling = Cancellation()

    def cancel_before_tool(request, cancelled):
        assert cancelled is cancelling
        cancelling.cancel()
        return response([function_call()])

    try:
        Engine(config, cancel_before_tool, backend_saves.append).run(
            body, cancelled=cancelling
        )
    except AeroError as error:
        assert error.status == 499, error
    else:
        raise AssertionError("backend cancellation released a tool")
    assert not backend_saves
    assert key


def authentication_and_protocol(server, upstream):
    status, _ = complete_request(
        server, {"stream": False}, "auth", authorization="Bearer wrong"
    )
    assert status == 401
    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
    try:
        connection.request("POST", "/v1/responses", "{}", {"Content-Length": "2"})
        missing = connection.getresponse()
        assert missing.status == 401
        missing.read()
    finally:
        connection.close()

    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
    try:
        connection.request(
            "POST",
            "/v1/responses",
            "{}",
            {
                "Authorization": "Bearer " + TOKEN,
                "Origin": "https://invalid.example",
                "Content-Length": "2",
            },
        )
        invalid = connection.getresponse()
        assert invalid.status == 401
        invalid.read()
    finally:
        connection.close()

    stream_body = {
        "model": "aero/lifecycle",
        "input": "Stream a lifecycle result",
        "stream": True,
    }
    upstream.actions.put(StreamResponse(response([message("stream okay")])))
    status, stream_data = complete_request(server, stream_body, "stream-success")
    assert status == 200
    assert "response.completed" in stream_data and "stream okay" in stream_data
    calls = upstream.received_count()
    status, replay_data = complete_request(server, stream_body, "stream-success")
    assert status == 200
    assert completed_response(stream_data) == completed_response(replay_data)
    assert upstream.received_count() == calls

    json_body = {
        "model": "aero/lifecycle",
        "input": "Return JSON lifecycle result",
        "stream": False,
    }
    upstream.actions.put(JsonResponse(response([message("json okay")])))
    status, json_data = complete_request(server, json_body, "json-success")
    assert status == 200
    assert json.loads(json_data)["output"][0]["content"][0]["text"] == "json okay"
    calls = upstream.received_count()
    status, json_replay = complete_request(server, json_body, "json-success")
    assert status == 200 and json_replay == json_data
    assert upstream.received_count() == calls


def disconnect_case(server, store, upstream, key, body, before):
    timings = {}
    for kind, stream in (("headers", False), ("json", False), ("sse", True)):
        wire_body = {**body, "stream": stream}
        stall = Stall(kind, upstream.cleanup)
        upstream.actions.put(stall)
        calls = upstream.received_count()
        connection = open_request(server, wire_body, "cancelled")
        downstream_response = None
        captured = b""
        try:
            wait_event(
                stall.sent if kind in ("json", "sse") else stall.received,
                kind + " upstream boundary",
            )
            if stream:
                downstream_response = connection.getresponse()
                assert downstream_response.status == 200
                captured = downstream_response.readline()
            disconnected = time.monotonic()
            disconnect(connection, downstream_response)
            connection = None
            wait_event(stall.closed, kind + " upstream closure")
            wait_until(lambda: active_requests(server) == 0, kind + " handler cleanup")
            elapsed = time.monotonic() - disconnected
            assert elapsed < 2, f"{kind} cancellation took {elapsed:.3f}s"
            assert store.get(key) == before, f"{kind} advanced persisted state"
            assert upstream.received_count() == calls + 1, (
                f"{kind} started another model turn"
            )
            assert b"response.completed" not in captured
            assert b"shell_command" not in captured
            stall.release.set()
            timings[kind] = round(elapsed, 3)
        finally:
            stall.release.set()
            if connection is not None:
                disconnect(connection, downstream_response)
    return timings


def successful_tool_retry(server, upstream, body):
    wire_body = {**body, "stream": True}
    calls = upstream.received_count()
    upstream.actions.put(JsonResponse(response([function_call()])))
    status, data = complete_request(server, wire_body, "cancelled")
    assert status == 200
    events = [
        json.loads(line[5:].strip())
        for line in data.splitlines()
        if line.startswith("data:")
    ]
    completed = [event for event in events if event["type"] == "response.completed"]
    assert completed
    item = completed[-1]["response"]["output"][0]
    assert item["type"] == "function_call" and item["name"] == "shell_command"
    assert upstream.received_count() == calls + 1
    returned = upstream.received_count()
    status, replay = complete_request(server, wire_body, "cancelled")
    assert status == 200 and completed_response(replay) == completed_response(data)
    assert upstream.received_count() == returned


def completion_race(config, directory, upstream):
    server, store, thread = start_gateway(config, directory)
    key, state, body, _ = seed_executor(config, "completion-race")
    store.put(key, state)
    before = store.get(key)
    upstream.actions.put(JsonResponse(response([function_call()])))
    reached = threading.Event()
    release = threading.Event()
    original_commit = server.commit

    def held_commit(*args, **kwargs):
        reached.set()
        assert release.wait(2), "timed out holding gateway commit"
        return original_commit(*args, **kwargs)

    server.commit = held_commit
    connection = None
    try:
        calls = upstream.received_count()
        connection = open_request(server, body, "completion-race")
        wait_event(reached, "gateway completion barrier")
        disconnected = time.monotonic()
        disconnect(connection)
        connection = None
        release.set()
        wait_until(
            lambda: active_requests(server) == 0,
            "completion-race handler cleanup",
        )
        elapsed = time.monotonic() - disconnected
        assert store.get(key) == before, "completion race persisted a tool response"
        assert upstream.received_count() == calls + 1, (
            "completion race started another turn"
        )
        assert handler_threads(server) == 0
        upstream.actions.put(JsonResponse(response([function_call("call_retry")])))
        status, data = complete_request(server, body, "completion-race")
        assert status == 200
        assert json.loads(data)["output"][0]["call_id"] == "call_retry"
        returned = upstream.received_count()
        status, replay = complete_request(server, body, "completion-race")
        assert status == 200 and replay == data
        assert upstream.received_count() == returned
        return round(elapsed, 3)
    finally:
        release.set()
        server.commit = original_commit
        if connection is not None:
            disconnect(connection)
        stop_gateway(server, store, thread)


def shutdown_case(config, directory, upstream):
    server, store, thread = start_gateway(config, directory)
    key, state, body, _ = seed_executor(config, "shutdown")
    store.put(key, state)
    before = store.get(key)
    stall = Stall("headers", upstream.cleanup)
    upstream.actions.put(stall)
    calls = upstream.received_count()
    client = None
    waiter = None
    partial = None
    stopped = False
    store_closed = False
    try:
        client = open_request(server, body, "shutdown")
        wait_event(stall.received, "shutdown active upstream")
        waiter = open_request(server, body, "shutdown")
        wait_until(
            lambda: cancellation_count(server) >= 2,
            "same-session shutdown waiter",
        )
        partial = socket.create_connection(("127.0.0.1", server.server_port), timeout=3)
        partial.sendall(
            b"POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1\r\n"
            b"Authorization: Bearer "
            + TOKEN.encode()
            + b"\r\nContent-Length: 100000\r\n\r\n{"
        )
        wait_until(
            lambda: cancellation_count(server) >= 3,
            "partial-body shutdown request",
        )
        started = time.monotonic()
        call_bounded(
            lambda: (server.shutdown(), server.server_close()),
            "gateway shutdown",
        )
        stopped = True
        elapsed = time.monotonic() - started
        wait_event(stall.closed, "shutdown upstream closure")
        wait_until(lambda: upstream.connection_count() == 0, "upstream handler exit")
        thread.join(timeout=2)
        assert not thread.is_alive()
        assert active_requests(server) == 0
        assert handler_threads(server) == 0
        assert upstream.received_count() == calls + 1
        assert store.get(key) == before, "shutdown advanced persisted state"
        store.close()
        store_closed = True
        return round(elapsed, 3)
    finally:
        for connection in (client, waiter):
            if connection is not None:
                disconnect(connection)
        if partial is not None:
            partial.close()
        if not stopped:
            call_bounded(
                lambda: (server.shutdown(), server.server_close()),
                "gateway shutdown cleanup",
            )
        if not store_closed:
            store.close()


def standalone_close(config, directory, upstream):
    server, store = create_server(config, directory / "gateway", token=TOKEN)
    key, state, body, _ = seed_executor(config, "standalone")
    store.put(key, state)
    before = store.get(key)
    trigger = threading.Event()
    exits = []

    def service_actions():
        if trigger.is_set():
            raise ServeExit

    server.service_actions = service_actions

    def serve():
        try:
            server.serve_forever(poll_interval=0.02)
        except ServeExit as error:
            exits.append(error)

    thread = threading.Thread(target=serve)
    stall = Stall("headers", upstream.cleanup)
    upstream.actions.put(stall)
    calls = upstream.received_count()
    connection = None
    closed = False
    store_closed = False
    thread.start()
    try:
        connection = open_request(server, body, "standalone")
        wait_event(stall.received, "standalone upstream request")
        started = time.monotonic()
        trigger.set()
        thread.join(timeout=2)
        assert not thread.is_alive()
        assert exits
        call_bounded(server.server_close, "standalone server_close")
        closed = True
        elapsed = time.monotonic() - started
        wait_event(stall.closed, "standalone upstream closure")
        wait_until(lambda: upstream.connection_count() == 0, "standalone upstream exit")
        assert active_requests(server) == 0
        assert handler_threads(server) == 0
        assert upstream.received_count() == calls + 1
        assert store.get(key) == before
        store.close()
        store_closed = True
        return round(elapsed, 3)
    finally:
        trigger.set()
        if connection is not None:
            disconnect(connection)
        if not closed:
            call_bounded(server.server_close, "standalone cleanup")
        if not store_closed:
            store.close()


def completed_worker_timeout(config, directory):
    key, state, body, _ = seed_executor(config, "completed-timeout")
    calls = []
    workers = []
    message = "controlled worker timeout"

    def backend(request, cancelled):
        calls.append(request)
        workers.append(threading.current_thread())
        raise TimeoutError(message)

    def backend_factory(session):
        assert session == "completed-timeout"
        return backend

    server, store = create_server(
        config, directory, token=TOKEN, backend_factory=backend_factory
    )
    store.put(key, state)
    before = copy.deepcopy(store.get(key))
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    result = []
    try:
        started = time.monotonic()
        call_bounded(
            lambda: result.append(complete_request(server, body, "completed-timeout")),
            "completed worker timeout response",
        )
        elapsed = time.monotonic() - started
        assert result, "completed worker timeout returned no response"
        status, data = result[0]
        assert status == 500, (status, data)
        assert json.loads(data)["error"]["message"] == message
        assert "response.completed" not in data
        assert "shell_command" not in data
        assert elapsed < 2, f"completed worker timeout took {elapsed:.3f}s"
        wait_until(
            lambda: active_requests(server) == 0,
            "completed timeout handler cleanup",
        )
        assert cancellation_count(server) == 0
        assert handler_threads(server) == 0
        assert len(calls) == 1
        assert len(workers) == 1 and not workers[0].is_alive()
        assert store.get(key) == before
        return round(elapsed, 3)
    finally:
        stop_gateway(server, store, thread)


def run():
    with tempfile.TemporaryDirectory(
        prefix="point-aero-lifecycle-", dir="/tmp"
    ) as scratch:
        directory = Path(scratch)
        with ControlledUpstream(TOKEN) as upstream:
            config = synthetic_config(upstream.url)
            direct_boundaries(config)

            server, store, thread = start_gateway(config, directory / "gateway")
            key, state, body, _ = seed_executor(config, "cancelled")
            store.put(key, state)
            before = copy.deepcopy(store.get(key))
            try:
                authentication_and_protocol(server, upstream)
                timings = disconnect_case(server, store, upstream, key, body, before)
                successful_tool_retry(server, upstream, body)
            finally:
                stop_gateway(server, store, thread)

            race_seconds = completion_race(
                config, directory / "completion-race", upstream
            )
            shutdown_seconds = shutdown_case(config, directory / "shutdown", upstream)
            standalone_seconds = standalone_close(
                config, directory / "standalone", upstream
            )
            timeout_seconds = completed_worker_timeout(
                config, directory / "completed-timeout"
            )
            upstream.assert_clean()
            result = {
                "lifecycle": "passed",
                "disconnect_seconds": timings,
                "completion_race_seconds": race_seconds,
                "shutdown_seconds": shutdown_seconds,
                "standalone_server_close_seconds": standalone_seconds,
                "timeout_response_seconds": timeout_seconds,
                "covered": [
                    "streaming and nonstreaming caller authentication",
                    "SSE and JSON success with replay",
                    "pre-response client disconnect",
                    "stalled JSON body disconnect",
                    "mid-event SSE disconnect",
                    "no successful tool frame after disconnect",
                    "unchanged persisted state after cancellation",
                    "successful external tool response and replay",
                    "completion-versus-disconnect commit boundary",
                    "same-session waiter shutdown",
                    "unfinished request-body shutdown",
                    "upstream and handler termination before database close",
                    "standalone server_close after serving exception",
                    "cancel-before-engine, replay and backend return",
                    "cancel-before-upstream-socket publication",
                    "completed worker TimeoutError reaches caller without state advancement",
                ],
            }
            evidence = ROOT / "evidence"
            evidence.mkdir(parents=True, exist_ok=True)
            (evidence / "verification-lifecycle.json").write_text(
                json.dumps(result, ensure_ascii=False, indent=2) + "\n"
            )
            print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    run()
