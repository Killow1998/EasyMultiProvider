import argparse
import concurrent.futures
import http.client
import json
import logging
import logging.handlers
import secrets
import select
import socket
import sqlite3
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from engine import AeroError, Engine, digest
from events import ResponseEvents
from settings import DEFAULT_RUNTIME, ROOT, gateway_key, load_config


class Cancellation:
    def __init__(self):
        self.event = threading.Event()
        self.lock = threading.Lock()
        self.connection = None
        self.stream_socket = None

    def __call__(self):
        return self.event.is_set()

    def attach(self, connection):
        with self.lock:
            self.connection = connection
            if self.event.is_set():
                self._shutdown_locked()
                return False
            return True

    def publish(self, stream_socket):
        with self.lock:
            self.stream_socket = stream_socket
            if self.event.is_set():
                self._shutdown_locked()
                return False
            return True

    def detach(self, connection):
        with self.lock:
            if self.connection is connection:
                self.connection = None
                self.stream_socket = None

    def _shutdown_locked(self):
        target = self.stream_socket or (
            self.connection.sock if self.connection else None
        )
        if target is not None:
            try:
                target.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if self.connection:
            try:
                self.connection.close()
            except OSError:
                pass

    def cancel(self):
        with self.lock:
            self.event.set()
            self._shutdown_locked()


class EmpBackend:
    def __init__(self, base_url, token, session, on_event=None, headers=None):
        self.url = urlsplit(base_url)
        self.token = token
        self.session = session
        self.on_event = on_event
        self.headers = headers or {}

    def __call__(self, body, cancelled):
        return self.request(body, cancelled, "/responses")

    def compact(self, body, cancelled):
        return self.request(body, cancelled, "/responses/compact")

    def request(self, body, cancelled, path):
        connection_class = (
            http.client.HTTPSConnection
            if self.url.scheme == "https"
            else http.client.HTTPConnection
        )
        connection = connection_class(self.url.hostname, self.url.port, timeout=2)
        if not cancelled.attach(connection):
            connection.close()
            raise AeroError("request cancelled", 499)
        try:
            if cancelled():
                raise AeroError("request cancelled", 499)
            connection.connect()
            if not cancelled.publish(connection.sock):
                raise AeroError("request cancelled", 499)
            connection.sock.settimeout(None)
            if cancelled():
                raise AeroError("request cancelled", 499)
            connection.request(
                "POST",
                self.url.path.rstrip("/") + path,
                body=json.dumps(body, ensure_ascii=False).encode(),
                headers={
                    **self.headers,
                    "Authorization": "Bearer " + self.token,
                    "Content-Type": "application/json",
                    "User-Agent": "aero/0.1",
                    "X-EMP-Client": "aero",
                    "session-id": self.session + ":" + digest(body["model"])[:12],
                },
            )
            response = connection.getresponse()
            if cancelled():
                raise AeroError("request cancelled", 499)
            if response.status != 200:
                detail = response.read(4096).decode(errors="replace")
                raise AeroError(f"EMP returned HTTP {response.status}: {detail}", 502)
            if "text/event-stream" not in response.getheader("Content-Type", ""):
                final = json.load(response)
                if cancelled():
                    raise AeroError("request cancelled", 499)
                return final
            data = []
            final = None
            for line in response:
                if cancelled():
                    raise AeroError("request cancelled", 499)
                line = line.rstrip(b"\r\n")
                if line.startswith(b"data:"):
                    value = line[5:]
                    data.append(value[1:] if value.startswith(b" ") else value)
                elif not line and data:
                    payload = b"\n".join(data)
                    data.clear()
                    if payload == b"[DONE]":
                        continue
                    event = json.loads(payload)
                    if self.on_event and path == "/responses":
                        self.on_event(event)
                    if event.get("type") in (
                        "response.completed",
                        "response.incomplete",
                    ):
                        final = event["response"]
                    elif event.get("type") in ("error", "response.failed"):
                        raise AeroError(
                            "EMP stream failed: "
                            + json.dumps(
                                event.get("error")
                                or event.get("response", {}).get("error"),
                                ensure_ascii=False,
                            ),
                            502,
                        )
            if final is None:
                raise AeroError("EMP stream ended without a completed response", 502)
            if cancelled():
                raise AeroError("request cancelled", 499)
            return final
        except AeroError:
            raise
        except Exception as error:
            if cancelled():
                raise AeroError("request cancelled", 499) from error
            raise
        finally:
            cancelled.detach(connection)
            connection.close()


class Store:
    def __init__(self, directory):
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "sessions.sqlite3"
        self.db = sqlite3.connect(path, check_same_thread=False)
        path.chmod(0o600)
        self.lock = threading.RLock()
        self.locks = {}
        self.db.execute(
            "CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, state TEXT NOT NULL)"
        )
        self.db.commit()

    def session_lock(self, key):
        with self.lock:
            return self.locks.setdefault(key, threading.Lock())

    def get(self, key):
        with self.lock:
            row = self.db.execute(
                "SELECT state FROM sessions WHERE id = ?", (key,)
            ).fetchone()
            return json.loads(row[0]) if row else None

    def put(self, key, state):
        with self.lock:
            self.db.execute(
                "INSERT INTO sessions VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET state = excluded.state",
                (key, json.dumps(state, ensure_ascii=False)),
            )
            self.db.commit()

    def close(self):
        self.db.close()


class GatewayServer(ThreadingHTTPServer):
    daemon_threads = True
    block_on_close = True

    def __init__(self, address, handler):
        self.stop_event = threading.Event()
        self.lifecycle = threading.Condition()
        self.active_requests = 0
        self.sockets = set()
        self.cancellations = set()
        self.threads = set()
        super().__init__(address, handler)

    def register_cancellation(self, cancellation):
        with self.lifecycle:
            if self.stop_event.is_set():
                cancellation.cancel()
                return False
            self.cancellations.add(cancellation)
            return True

    def unregister_cancellation(self, cancellation):
        with self.lifecycle:
            self.cancellations.discard(cancellation)

    def commit(self, cancellation, save, connected):
        with self.lifecycle:
            with cancellation.lock:
                if (
                    self.stop_event.is_set()
                    or cancellation.event.is_set()
                    or not connected()
                ):
                    return False
                save()
                return True

    def process_request(self, request, client_address):
        with self.lifecycle:
            if self.stop_event.is_set():
                try:
                    request.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                self.shutdown_request(request)
                return
            self.sockets.add(request)
            self.active_requests += 1
            thread = threading.Thread(
                target=self.process_request_thread,
                args=(request, client_address),
            )
            thread.daemon = self.daemon_threads
            self.threads.add(thread)
            try:
                thread.start()
            except Exception:
                self.threads.discard(thread)
                self.active_requests -= 1
                self.lifecycle.notify_all()
                self.shutdown_request(request)
                raise

    def process_request_thread(self, request, client_address):
        try:
            self.finish_request(request, client_address)
        except Exception:
            self.handle_error(request, client_address)
        finally:
            self.shutdown_request(request)
            with self.lifecycle:
                self.threads.discard(threading.current_thread())
                self.active_requests -= 1
                self.lifecycle.notify_all()

    def shutdown_request(self, request):
        with self.lifecycle:
            self.sockets.discard(request)
        super().shutdown_request(request)

    def _begin_stop(self):
        with self.lifecycle:
            if self.stop_event.is_set():
                return
            self.stop_event.set()
            sockets = list(self.sockets)
            cancellations = list(self.cancellations)
        for request in sockets:
            try:
                request.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for cancellation in cancellations:
            cancellation.cancel()

    def shutdown(self):
        self._begin_stop()
        super().shutdown()

    def server_close(self):
        self._begin_stop()
        super().server_close()
        with self.lifecycle:
            threads = list(self.threads)
        for thread in threads:
            thread.join()
        with self.lifecycle:
            while self.active_requests:
                self.lifecycle.wait()


def handler_for(
    config, store, logger, backend_factory=None, token=None, config_path=None
):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format, *args):
            pass

        def allowed(self):
            origin = self.headers.get("Origin")
            if origin and origin != "http://" + self.headers.get("Host", ""):
                return False
            return token is None or secrets.compare_digest(
                self.headers.get("Authorization", ""), "Bearer " + token
            )

        def send_json(self, status, body):
            payload = json.dumps(body, ensure_ascii=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def client_gone(self):
            try:
                readable, _, _ = select.select([self.connection], [], [], 0)
                return bool(readable and not self.connection.recv(1, socket.MSG_PEEK))
            except (OSError, ValueError):
                return True

        def stream_write(self, payload, cancellation):
            if cancellation() or self.client_gone():
                cancellation.cancel()
                return False
            try:
                self.wfile.write(payload)
                self.wfile.flush()
                return True
            except (BrokenPipeError, ConnectionResetError, OSError):
                cancellation.cancel()
                return False

        def forward_media(self, payload, cancellation):
            settings = json.loads(config_path.read_bytes()) if config_path else config
            target = urlsplit(settings["upstream"])
            connection_type = (
                http.client.HTTPSConnection
                if target.scheme == "https"
                else http.client.HTTPConnection
            )
            connection = connection_type(target.hostname, target.port, timeout=2)
            if not cancellation.attach(connection):
                return
            self.close_connection = True
            try:
                connection.connect()
                if not cancellation.publish(connection.sock):
                    return
                connection.sock.settimeout(None)
                headers = {
                    key: value
                    for key, value in self.headers.items()
                    if key.lower()
                    not in (
                        "host",
                        "authorization",
                        "x-aero-caller-key",
                        "content-length",
                        "transfer-encoding",
                    )
                }
                headers["Authorization"] = "Bearer " + (
                    self.headers.get("X-Aero-Caller-Key", token)
                    if config_path
                    else token
                )
                headers["Content-Length"] = str(len(payload))
                if self.headers.get("Upgrade", "").lower() != "websocket":
                    headers["Connection"] = "close"
                connection.request(
                    self.command,
                    target.path.rstrip("/") + self.path.removeprefix("/v1"),
                    body=payload,
                    headers=headers,
                )
                while not cancellation():
                    readable, _, _ = select.select(
                        [connection.sock, self.connection], [], [], 0.05
                    )
                    if self.connection in readable:
                        data = self.connection.recv(65536)
                        if not data:
                            return
                        connection.sock.sendall(data)
                    if connection.sock in readable:
                        data = connection.sock.recv(65536)
                        if not data or not self.stream_write(data, cancellation):
                            return
            finally:
                cancellation.detach(connection)
                connection.close()

        def do_GET(self):
            if self.path == "/healthz":
                self.send_json(200, {"status": "ok", "models": list(config["models"])})
            elif (
                self.path.split("?", 1)[0] in ("/v1/models", "/models")
                and self.allowed()
            ):
                self.send_json(
                    200,
                    {
                        "object": "list",
                        "data": [
                            {"id": model, "object": "model", "owned_by": "aero"}
                            for model in config["models"]
                        ],
                    },
                )
            elif self.path.startswith("/v1/live/") and self.allowed():
                cancellation = Cancellation()
                if self.server.register_cancellation(cancellation):
                    try:
                        self.forward_media(b"", cancellation)
                    finally:
                        cancellation.cancel()
                        self.server.unregister_cancellation(cancellation)
            else:
                self.send_json(404, {"error": {"message": "not found"}})

        def do_POST(self):
            streaming = False
            cancellation = Cancellation()
            registered = self.server.register_cancellation(cancellation)
            locked = False
            try:
                if not registered:
                    return
                if not self.allowed():
                    raise AeroError(
                        "local Codex caller authentication is required", 401
                    )
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 64 * 1024 * 1024:
                    raise AeroError("request body must be 1..67108864 bytes")
                payload = self.rfile.read(size)
                if self.path in (
                    "/v1/images/generations",
                    "/v1/images/edits",
                    "/v1/live",
                ):
                    self.forward_media(payload, cancellation)
                    return
                if self.path not in ("/v1/responses", "/v1/responses/compact"):
                    raise AeroError("Unknown inference endpoint", 404)
                body = json.loads(payload)
                session = (
                    self.headers.get("session-id")
                    or self.headers.get("session_id")
                    or body.get("prompt_cache_key")
                )
                if not isinstance(session, str) or not session:
                    raise AeroError("session_id header or prompt_cache_key is required")
                key = digest([session, body.get("model")])

                def observe(event):
                    logger.info(
                        json.dumps({"session": key[:12], **event}, ensure_ascii=False)
                    )

                request_config = (
                    json.loads(config_path.read_bytes()) if config_path else config
                )
                items = body.get("input", [])
                compact = self.path.endswith("/compact") or (
                    isinstance(items, list)
                    and bool(items)
                    and items[-1].get("type") == "compaction_trigger"
                )
                events = (
                    ResponseEvents(body.get("model")) if body.get("stream") else None
                )
                backend = (
                    backend_factory(session)
                    if backend_factory
                    else EmpBackend(
                        request_config["upstream"],
                        self.headers.get("X-Aero-Caller-Key", token)
                        if config_path
                        else token,
                        session,
                        events.upstream if events and not compact else None,
                        {
                            key: value
                            for key, value in self.headers.items()
                            if key.lower()
                            in (
                                "x-codex-turn-metadata",
                                "x-codex-thread-id",
                                "x-codex-parent-thread-id",
                                "originator",
                                "version",
                            )
                        },
                    )
                )
                engine = Engine(request_config, backend, lambda state: None)
                session_lock = store.session_lock(key)
                while not session_lock.acquire(timeout=0.05):
                    if cancellation() or self.client_gone():
                        cancellation.cancel()
                        raise AeroError("request cancelled", 499)
                locked = True
                try:
                    if cancellation():
                        raise AeroError("request cancelled", 499)
                    previous = store.get(key)
                    if engine.needs_history(body, previous):
                        self.send_response(409)
                        self.send_header("X-Aero-History", "required")
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                    if events:
                        streaming = True
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.send_header("Cache-Control", "no-cache")
                        self.send_header("Connection", "close")
                        self.end_headers()
                        self.close_connection = True
                        if not self.stream_write(b": aero connected\n\n", cancellation):
                            return
                    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                        future = (
                            pool.submit(
                                engine.compact, body, previous, cancellation, observe
                            )
                            if compact
                            else pool.submit(
                                engine.run,
                                body,
                                previous,
                                cancellation,
                                observe,
                                events,
                            )
                        )
                        heartbeat = time.monotonic()
                        while True:
                            if events:
                                frames = events.drain()
                                if frames and not self.stream_write(
                                    frames, cancellation
                                ):
                                    return
                            done, _ = concurrent.futures.wait((future,), timeout=0.05)
                            if done:
                                response, candidate = future.result()
                                break
                            if cancellation() or self.client_gone():
                                cancellation.cancel()
                                observe({"kind": "cancelled"})
                                return
                            if streaming and time.monotonic() - heartbeat >= 1:
                                if not self.stream_write(
                                    b": aero working\n\n", cancellation
                                ):
                                    return
                                heartbeat = time.monotonic()
                    if not self.server.commit(
                        cancellation,
                        lambda: store.put(key, candidate),
                        lambda: not self.client_gone(),
                    ):
                        observe({"kind": "cancelled"})
                        return
                    if streaming:
                        events.finish(response)
                        self.stream_write(events.drain(), cancellation)
                    else:
                        if cancellation() or self.client_gone():
                            cancellation.cancel()
                            return
                        self.send_json(200, response)
                finally:
                    if locked:
                        session_lock.release()
                        locked = False
            except (BrokenPipeError, ConnectionResetError):
                cancellation.cancel()
            except AeroError as error:
                if cancellation():
                    logger.info(json.dumps({"kind": "cancelled"}, ensure_ascii=False))
                    return
                status = error.status
                self.report_error(status, error, streaming)
            except Exception as error:
                cancellation.cancel()
                if not self.client_gone():
                    self.report_error(500, error, streaming)
            finally:
                cancellation.cancel()
                if registered:
                    self.server.unregister_cancellation(cancellation)

        def report_error(self, status, error, streaming):
            logger.error(
                json.dumps(
                    {
                        "kind": "request_failed",
                        "status": status,
                        "error": str(error),
                    },
                    ensure_ascii=False,
                )
            )
            payload = {"error": {"type": "aero_error", "message": str(error)}}
            if self.server.stop_event.is_set():
                return
            try:
                if streaming:
                    event = {
                        "type": "response.failed",
                        "response": {
                            "id": "resp_aero_failed",
                            "status": "failed",
                            "error": {"code": "aero_error", "message": str(error)},
                        },
                    }
                    self.wfile.write(
                        (
                            "event: response.failed\ndata: "
                            + json.dumps(event, ensure_ascii=False)
                            + "\n\n"
                        ).encode()
                    )
                    self.wfile.flush()
                else:
                    self.send_json(status, payload)
            except OSError:
                self.close_connection = True

    return Handler


def create_server(
    config, directory, port=0, backend_factory=None, token=None, config_path=None
):
    store = Store(directory)
    logger = logging.getLogger("aero." + str(directory))
    logger.setLevel(logging.INFO)
    logger.propagate = False
    if not logger.handlers:
        handler = logging.handlers.RotatingFileHandler(
            directory / "events.jsonl",
            maxBytes=10_000_000,
            backupCount=9,
            encoding="utf-8",
        )
        handler.setFormatter(logging.Formatter("%(message)s"))
        logger.addHandler(handler)
    server = GatewayServer(
        ("127.0.0.1", port),
        handler_for(config, store, logger, backend_factory, token, config_path),
    )
    return server, store


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, default=ROOT / "profiles.json")
    parser.add_argument("--runtime", type=Path, default=DEFAULT_RUNTIME)
    parser.add_argument("--port", type=int, default=4219)
    parser.add_argument("--managed", action="store_true")
    args = parser.parse_args()
    try:
        settings = load_config(args.config)
        bearer = gateway_key(settings)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    gateway, database = create_server(
        settings,
        args.runtime.expanduser().resolve(),
        args.port,
        token=bearer,
        config_path=args.config.resolve() if args.managed else None,
    )
    if args.managed:

        def parent_closed():
            sys.stdin.buffer.read()
            gateway.shutdown()

        threading.Thread(target=parent_closed, daemon=True).start()
    print(
        json.dumps(
            {
                "url": f"http://127.0.0.1:{gateway.server_port}/v1",
                "models": list(settings["models"]),
            },
        ),
        flush=True,
    )
    try:
        gateway.serve_forever()
    finally:
        gateway.server_close()
        database.close()
