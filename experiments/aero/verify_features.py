import argparse
import base64
import contextlib
import http.client
import io
import json
import socket
import tempfile
import threading
import wave
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from engine import digest
from gateway import create_server
from verify import call, reply, text
from verify_lifecycle import (
    ControlledUpstream,
    JsonResponse,
    completed_response,
    synthetic_config,
)

TOKEN = "aero-feature-fixture"
PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="


@contextlib.contextmanager
def gateway(directory):
    with ControlledUpstream(TOKEN) as upstream:
        config = synthetic_config(upstream.url)
        server, store = create_server(config, directory, token=TOKEN)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            yield server, store, upstream
            upstream.assert_clean()
        finally:
            server.shutdown()
            server.server_close()
            store.close()
            thread.join()


def request(server, body, path="/v1/responses"):
    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=4)
    connection.request(
        "POST",
        path,
        json.dumps(body),
        {
            "Authorization": "Bearer " + TOKEN,
            "session-id": "feature-session",
            "Content-Type": "application/json",
        },
    )
    return connection, connection.getresponse()


def complete(server, body, path="/v1/responses"):
    connection, response = request(server, body, path)
    try:
        payload = response.read()
        assert response.status == 200, (response.status, payload)
        return json.loads(payload)
    finally:
        connection.close()


class LiveText:
    def __init__(self):
        self.release = threading.Event()

    def handle(self, handler, body):
        item = {
            "type": "message",
            "id": "upstream-live",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello world"}],
        }
        handler.send_response(200)
        handler.send_header("Content-Type", "text/event-stream")
        handler.send_header("Connection", "close")
        handler.end_headers()
        for event in [
            {
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {**item, "content": []},
            },
            {
                "type": "response.content_part.added",
                "output_index": 0,
                "item_id": item["id"],
                "content_index": 0,
                "part": {"type": "output_text", "text": ""},
            },
            {
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": item["id"],
                "content_index": 0,
                "delta": "Hello ",
            },
        ]:
            handler.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
        handler.wfile.flush()
        assert self.release.wait(6), (
            "The first text token was buffered until completion"
        )
        for event in [
            {
                "type": "response.output_text.delta",
                "output_index": 0,
                "item_id": item["id"],
                "content_index": 0,
                "delta": "world",
            },
            {"type": "response.completed", "response": reply([item])},
        ]:
            handler.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
        handler.wfile.flush()


def streaming(directory):
    with gateway(directory) as (server, _, upstream):
        action = LiveText()
        upstream.actions.put(action)
        connection, response = request(
            server,
            {"model": "aero/lifecycle", "input": "Explain streaming", "stream": True},
        )
        events = []
        try:
            while not action.release.is_set():
                line = response.readline()
                assert line, "The stream ended before the first live text delta"
                if line.startswith(b"data: "):
                    event = json.loads(line[6:])
                    events.append(event)
                    if event["type"] == "response.output_text.delta":
                        assert event["delta"] == "Hello "
                        assert not any(
                            item["type"] == "response.completed" for item in events
                        )
                        action.release.set()
            events.extend(
                json.loads(line[6:])
                for line in response.read().splitlines()
                if line.startswith(b"data: ")
            )
        finally:
            action.release.set()
            connection.close()
        terminal = next(
            event["response"]
            for event in events
            if event["type"] == "response.completed"
        )
        deltas = [
            event for event in events if event["type"] == "response.output_text.delta"
        ]
        assert "".join(event["delta"] for event in deltas) == "Hello world"
        assert terminal["output"][0]["content"][0]["text"] == "Hello world"
        assert len({event["item_id"] for event in deltas}) == 1
        assert terminal["output"][0]["id"] == deltas[0]["item_id"]
        assert len(upstream.received) == 1


class CompactReply(JsonResponse):
    def handle(self, handler, body):
        assert handler.path == "/v1/responses/compact"
        super().handle(handler, body)


def compaction(directory):
    with gateway(directory) as (server, store, upstream):
        body = {
            "model": "aero/lifecycle",
            "input": [{"role": "user", "content": "Implement the project"}],
            "tools": [
                {
                    "type": "function",
                    "name": "shell_command",
                    "parameters": {"type": "object"},
                }
            ],
        }
        upstream.actions.put(
            JsonResponse(
                reply(
                    [
                        call(
                            "exit_plan_mode",
                            {"plan": "Write and verify the project"},
                            1,
                        )
                    ]
                )
            )
        )
        upstream.actions.put(
            JsonResponse(reply([call("shell_command", {"command": "verify"}, 2)]))
        )
        first = complete(server, body)
        compact_body = {
            **body,
            "input": body["input"]
            + first["output"]
            + [
                {
                    "type": "function_call_output",
                    "call_id": "call_2",
                    "output": "Verified",
                }
            ],
        }
        checkpoint = {
            "type": "compaction",
            "id": "checkpoint-native",
            "encrypted_content": "native-encrypted-checkpoint",
        }
        upstream.actions.put(
            CompactReply(
                {
                    "id": "compact-response",
                    "object": "response.compaction",
                    "output": [checkpoint],
                    "usage": {
                        "input_tokens": 8,
                        "output_tokens": 2,
                        "total_tokens": 10,
                    },
                }
            )
        )
        upstream.actions.put(
            JsonResponse(
                reply(
                    [
                        text(
                            "CHECKPOINT: project implemented, verification passed; finish the current plan"
                        )
                    ]
                )
            )
        )
        compacted = complete(server, compact_body, "/v1/responses/compact")
        assert compacted["output"] == [checkpoint]
        assert compacted["usage"]["input_tokens"] == 18
        assert complete(server, compact_body, "/v1/responses/compact") == compacted
        assert len(upstream.received) == 4
        state = store.get(digest(["feature-session", "aero/lifecycle"]))
        assert state["role"] == "executor"
        assert state["plan"] == "Write and verify the project"
        assert len(state["history"]) == 1
        assert "CHECKPOINT" in state["history"][0]["content"][0]["text"]
        upstream.actions.put(
            JsonResponse(
                reply(
                    [call("enter_plan_mode", {"reason": "Report after compaction"}, 3)]
                )
            )
        )
        upstream.actions.put(
            JsonResponse(
                reply([text("Continued in the same session after compaction")])
            )
        )
        continued = complete(
            server,
            {
                **body,
                "input": compacted["output"]
                + [{"role": "user", "content": "Continue and finish"}],
            },
        )
        assert "Continued in the same session" in json.dumps(continued)
        assert "CHECKPOINT" in json.dumps(upstream.received[-1]["input"])
        assert "native-encrypted-checkpoint" not in json.dumps(
            upstream.received[-1]["input"]
        )
        portable = {
            "type": "compaction",
            "encrypted_content": "emp1:"
            + base64.urlsafe_b64encode(
                b"Project verified. Preserve the current plan."
            ).decode(),
        }
        upstream.actions.put(
            CompactReply(
                {
                    "id": "trigger-checkpoint",
                    "object": "response.compaction",
                    "output": [portable],
                    "usage": {},
                }
            )
        )
        connection, response = request(
            server,
            {
                **body,
                "stream": True,
                "input": compacted["output"]
                + [{"role": "user", "content": "Continue and finish"}]
                + continued["output"]
                + [{"type": "compaction_trigger"}],
            },
        )
        try:
            assert response.status == 200
            triggered = completed_response(response.read().decode())
            assert triggered["output"] == [portable]
            assert (
                triggered["status"] == "completed" and triggered["object"] == "response"
            )
        finally:
            connection.close()


def media(directory):
    audio = io.BytesIO()
    with wave.open(audio, "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(16000)
        output.writeframes(b"\0\0" * 320)
    wav = base64.b64encode(audio.getvalue()).decode()
    with gateway(directory) as (server, _, upstream):
        upstream.actions.put(
            JsonResponse(
                reply(
                    [
                        call(
                            "exit_plan_mode",
                            {
                                "plan": "Inspect the image and audio and generate the requested media"
                            },
                            1,
                        )
                    ]
                )
            )
        )
        image = {
            "type": "image_generation_call",
            "id": "generated-image",
            "status": "completed",
            "result": PNG,
        }
        speech = {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_audio", "data": wav, "format": "wav"}],
        }
        upstream.actions.put(JsonResponse(reply([image, speech])))
        upstream.actions.put(JsonResponse(reply([text("Image and audio verified")])))
        body = {
            "model": "aero/lifecycle",
            "input": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Inspect and return media"},
                        {
                            "type": "input_image",
                            "image_url": "data:image/png;base64," + PNG,
                        },
                        {
                            "type": "input_audio",
                            "input_audio": {"data": wav, "format": "wav"},
                        },
                    ],
                }
            ],
            "tools": [{"type": "image_generation"}],
        }
        result = complete(server, body)
        assert (
            next(
                item
                for item in result["output"]
                if item["type"] == "image_generation_call"
            )["result"]
            == PNG
        )
        returned_audio = next(
            part["data"]
            for item in result["output"]
            if item["type"] == "message"
            for part in item["content"]
            if part["type"] == "output_audio"
        )
        assert base64.b64decode(returned_audio) == audio.getvalue()
        assert PNG in json.dumps(upstream.received[0]["input"])
        assert wav in json.dumps(upstream.received[1]["input"])
        assert any(
            tool["type"] == "image_generation" for tool in upstream.received[1]["tools"]
        )
        assert PNG in json.dumps(upstream.received[2]["input"])


def media_proxy(directory):
    received = []
    mask = b"\x11\x22\x33\x44"
    frame = (
        b"\x81\x83"
        + mask
        + bytes(value ^ mask[index % 4] for index, value in enumerate(b"ack"))
    )

    class MediaHandler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            payload = self.rfile.read(int(self.headers["Content-Length"]))
            assert self.headers["Authorization"] == "Bearer " + TOKEN
            received.append((self.path, self.headers["Content-Type"], payload))
            value = (
                b"v=0\r\nVoice fixture\r\n"
                if self.path == "/v1/live"
                else json.dumps({"data": [{"b64_json": PNG}]}).encode()
            )
            self.send_response(201 if self.path == "/v1/live" else 200)
            self.send_header("Content-Length", str(len(value)))
            self.send_header(
                "Content-Type",
                "application/sdp" if self.path == "/v1/live" else "application/json",
            )
            if self.path == "/v1/live":
                self.send_header("Location", "/v1/live/voice-fixture")
            self.end_headers()
            self.wfile.write(value)

        def do_GET(self):
            assert self.path == "/v1/live/voice-fixture"
            assert self.headers["Authorization"] == "Bearer " + TOKEN
            self.send_response(101)
            self.send_header("Connection", "Upgrade")
            self.send_header("Upgrade", "websocket")
            self.end_headers()
            self.wfile.write(b"\x81\x05ready")
            self.wfile.flush()
            assert self.rfile.read(len(frame)) == frame
            self.wfile.write(b"\x81\x03ack")
            self.wfile.flush()

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), MediaHandler)
    upstream_thread = threading.Thread(target=upstream.serve_forever)
    upstream_thread.start()
    config = synthetic_config(f"http://127.0.0.1:{upstream.server_port}/v1")
    server, store = create_server(config, directory, token=TOKEN)
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        for path in ["/v1/images/generations", "/v1/images/edits", "/v1/live"]:
            payload = b"--fixture\r\nContent-Type: audio/wav\r\n\r\nRIFF\x00\xff\r\n--fixture--\r\n"
            content_type = "multipart/form-data; boundary=fixture"
            connection = http.client.HTTPConnection(
                "127.0.0.1", server.server_port, timeout=3
            )
            connection.request(
                "POST",
                path,
                payload,
                {"Authorization": "Bearer " + TOKEN, "Content-Type": content_type},
            )
            response = connection.getresponse()
            assert response.status == (201 if path == "/v1/live" else 200)
            if path == "/v1/live":
                assert response.getheader("Location") == "/v1/live/voice-fixture"
                assert response.read().startswith(b"v=0")
            else:
                assert json.loads(response.read())["data"][0]["b64_json"] == PNG
            connection.close()
            assert received[-1] == (path, content_type, payload)
        with socket.create_connection(
            ("127.0.0.1", server.server_port), timeout=3
        ) as client:
            client.sendall(
                (
                    "GET /v1/live/voice-fixture HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer "
                    + TOKEN
                    + "\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
                ).encode()
            )
            with client.makefile("rb") as reader:
                assert reader.readline().startswith(b"HTTP/1.0 101")
                while reader.readline() != b"\r\n":
                    pass
                assert reader.read(7) == b"\x81\x05ready"
                client.sendall(frame)
                assert reader.read(5) == b"\x81\x03ack"
    finally:
        server.shutdown()
        server.server_close()
        store.close()
        thread.join()
        upstream.shutdown()
        upstream.server_close()
        upstream_thread.join()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--case", choices=["streaming", "compaction", "media", "media_proxy"]
    )
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="aero-features-") as scratch:
        for name in (
            [args.case]
            if args.case
            else ["streaming", "compaction", "media", "media_proxy"]
        ):
            globals()[name](Path(scratch) / name)
            print(name + ": passed")
