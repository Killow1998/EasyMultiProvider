import importlib.util
from io import BytesIO
from pathlib import Path
import struct
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "tools" / "benchmark_emp_ws_delay.py"
SPEC = importlib.util.spec_from_file_location("benchmark_emp_ws_delay", SCRIPT)
ws_delay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ws_delay)


def _server_frame(opcode, payload=b"", *, fin=True):
    first = (0x80 if fin else 0) | opcode
    if len(payload) < 126:
        header = bytes((first, len(payload)))
    elif len(payload) <= 0xFFFF:
        header = bytes((first, 126)) + struct.pack("!H", len(payload))
    else:
        header = bytes((first, 127)) + struct.pack("!Q", len(payload))
    return header + payload


class PartialReader:
    def __init__(self, value, chunk_size=1):
        self._reader = BytesIO(value)
        self._chunk_size = chunk_size

    def read(self, size=-1):
        if size < 0:
            size = self._chunk_size
        return self._reader.read(min(size, self._chunk_size))


class BenchmarkWebSocketDelayTests(unittest.TestCase):
    def test_accept_matches_rfc6455_example_and_rejects_invalid_key(self):
        self.assertEqual(
            ws_delay.websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        )
        with self.assertRaises(ws_delay.WebSocketProtocolError):
            ws_delay.websocket_accept("not-a-key")

    def test_upgrade_validation_requires_101_accept_and_upgrade_tokens(self):
        key = "dGhlIHNhbXBsZSBub25jZQ=="
        headers = {
            "upgrade": ["h2c, WebSocket"],
            "connection": ["keep-alive, uPgRaDe"],
            "sec-websocket-accept": [ws_delay.websocket_accept(key)],
        }
        self.assertTrue(ws_delay._validate_upgrade_response(101, headers, key))
        self.assertFalse(ws_delay._validate_upgrade_response(200, headers, key))
        self.assertFalse(ws_delay._validate_upgrade_response(
            101, {**headers, "sec-websocket-accept": ["wrong"]}, key
        ))
        self.assertFalse(ws_delay._validate_upgrade_response(
            101, {**headers, "sec-websocket-extensions": ["permessage-deflate"]}, key
        ))

    def test_client_frames_are_masked_and_support_extended_lengths(self):
        key = b"test"
        for payload in (b"short", b"x" * 126, b"y" * 65536):
            frame = ws_delay.encode_client_frame(0x1, payload, mask_key=key)
            self.assertTrue(frame[1] & 0x80)
            offset = 2
            marker = frame[1] & 0x7F
            if marker == 126:
                length = struct.unpack("!H", frame[offset:offset + 2])[0]
                offset += 2
            elif marker == 127:
                length = struct.unpack("!Q", frame[offset:offset + 8])[0]
                offset += 8
            else:
                length = marker
            mask = frame[offset:offset + 4]
            offset += 4
            decoded = bytes(value ^ mask[index & 3]
                            for index, value in enumerate(frame[offset:]))
            self.assertEqual(length, len(payload))
            self.assertEqual(mask, key)
            self.assertEqual(decoded, payload)

    def test_reader_handles_partial_tcp_chunks_fragmentation_and_ping(self):
        wire = (
            _server_frame(0x1, b'{"type":', fin=False)
            + _server_frame(0x9, b"ping")
            + _server_frame(0x0, b'"ok"}')
        )
        sent_controls = []
        opcode, payload = ws_delay.read_server_message(
            PartialReader(wire), lambda op, data: sent_controls.append((op, data))
        )
        self.assertEqual(opcode, 0x1)
        self.assertEqual(payload, b'{"type":"ok"}')
        self.assertEqual(sent_controls, [(0xA, b"ping")])

    def test_server_frame_parser_handles_extended_lengths_across_partial_reads(self):
        for payload in (b"x" * 126, b"y" * 65536):
            with self.subTest(length=len(payload)):
                opcode, decoded = ws_delay.read_server_message(
                    PartialReader(_server_frame(0x1, payload), chunk_size=97),
                    lambda *_args: self.fail(),
                )
                self.assertEqual(opcode, 0x1)
                self.assertEqual(decoded, payload)

    def test_close_frame_is_returned_and_decoded(self):
        payload = struct.pack("!H", 1000) + b"done"
        opcode, value = ws_delay.read_server_message(
            PartialReader(_server_frame(0x8, payload)), lambda *_args: self.fail()
        )
        self.assertEqual(opcode, 0x8)
        self.assertEqual(
            ws_delay._parse_close(value), {"code": 1000, "reason_length": 4}
        )
        self.assertNotIn("done", repr(ws_delay._parse_close(value)))

    def test_control_metadata_signature_is_separate_and_does_not_copy_payload(self):
        signature = ws_delay._control_signature({
            "type": "codex.response.metadata",
            "headers": {
                "X-Models-Etag": "fixture-etag",
                "Authorization": "Bearer secret",
            },
            "payload": "must-not-be-reported",
        })
        self.assertEqual(signature["type"], "codex.response.metadata")
        self.assertEqual(signature["headers"], {
            "x-models-etag": "fixture-etag",
            "authorization": "<redacted>",
        })
        self.assertEqual(signature["other_fields"], ["payload"])
        self.assertNotIn("must-not-be-reported", repr(signature))
        self.assertNotIn("Bearer secret", repr(signature))

    def test_transcript_orders_control_metadata_before_response_and_rejects_late_events(self):
        valid = [
            "codex.response.metadata",
            "response.created",
            "response.output_text.delta",
            "response.completed",
        ]
        invalid = [valid[1], valid[0], *valid[2:]]
        self.assertTrue(ws_delay._metadata_precedes_response_created(valid))
        self.assertFalse(ws_delay._metadata_precedes_response_created(invalid))
        self.assertTrue(ws_delay._is_invalid_post_terminal_event(
            "response.output_text.delta"
        ))
        self.assertFalse(ws_delay._is_invalid_post_terminal_event(
            "codex.response.metadata"
        ))

    def test_upstream_request_shape_diagnoses_nonce_and_redacts_all_values(self):
        body = {
            "model": ws_delay.benchmark.UPSTREAM_MODEL,
            "stream": True,
            "metadata": {"benchmark_nonce": "fixture-nonce"},
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "ws-scheduled-fixture-nonce"}],
            }],
        }
        shape, rejection = ws_delay._request_shape(
            "/v1/responses",
            "Bearer " + ws_delay.benchmark.FIXTURE_PROVIDER_KEY,
            body,
        )
        self.assertEqual(rejection, "")
        self.assertTrue(shape["authorization_matches_fixture"])
        self.assertTrue(shape["stream_is_true"])
        self.assertEqual(shape["input_type"], "array")
        self.assertEqual(shape["input_array_item_types"], ["message"])
        self.assertEqual(shape["input_content_item_types"], ["input_text"])
        self.assertTrue(shape["metadata_nonce_present"])
        self.assertTrue(shape["input_nonce_present"])
        self.assertTrue(shape["nonce_consistent"])
        self.assertNotIn("fixture-nonce", repr(shape))
        self.assertNotIn(ws_delay.benchmark.FIXTURE_PROVIDER_KEY, repr(shape))

    def test_upstream_rejection_diagnostic_distinguishes_stream_and_route(self):
        body = {
            "model": ws_delay.benchmark.UPSTREAM_MODEL,
            "stream": False,
            "input": "ws-scheduled-fixture-nonce",
        }
        _, stream_rejection = ws_delay._request_shape(
            "/v1/responses",
            "Bearer " + ws_delay.benchmark.FIXTURE_PROVIDER_KEY,
            body,
        )
        _, path_rejection = ws_delay._request_shape(
            "/wrong", "Bearer " + ws_delay.benchmark.FIXTURE_PROVIDER_KEY, body
        )
        self.assertEqual(stream_rejection, "upstream_stream_not_true")
        self.assertEqual(path_rejection, "upstream_path_mismatch")

    def test_concurrency_limits_select_expected_delay_gate(self):
        ws_delay.validate_parameters(1, 0, 1)
        self.assertEqual(ws_delay.p95_limit_for_concurrency(1), 5.0)
        self.assertEqual(ws_delay.p95_limit_for_concurrency(2), 5.0)
        self.assertEqual(ws_delay.p95_limit_for_concurrency(128), 20.0)
        for concurrency in (0, 129):
            with self.subTest(concurrency=concurrency), self.assertRaises(
                ws_delay.benchmark.BenchmarkError
            ):
                ws_delay.validate_parameters(1, 0, concurrency)


if __name__ == "__main__":
    unittest.main()
