#!/usr/bin/env python3
"""Exercise the actual EMP MCP process against a local, unpaid image upstream."""
import base64
import http.server
import json
import pathlib
import queue
import subprocess
import sys
import tempfile
import threading

observed = queue.Queue()
disconnected = threading.Event()


class Images(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        observed.put(body)
        if body['prompt'] == 'wait':
            self.connection.settimeout(5)
            if self.connection.recv(1) == b'':
                disconnected.set()
            return
        payload = b'<svg xmlns="http://www.w3.org/2000/svg"/>' if body['output_format'] == 'svg' else b'GIF89a'
        encoded = json.dumps({'data': [{'b64_json': base64.b64encode(payload).decode()}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def send(process, message):
    process.stdin.write(json.dumps({'jsonrpc': '2.0', **message}) + '\n')
    process.stdin.flush()


def call(process, request_id, **args):
    send(process, {'id': request_id, 'method': 'tools/call', 'params': {
        'name': 'imagegen', 'arguments': args,
        '_meta': {'x-codex-turn-metadata': {'turn_id': 'fixture'}}}})


with tempfile.TemporaryDirectory(prefix='emp-image-mcp-') as directory:
    root = pathlib.Path(directory)
    key = root / 'gateway-key'
    key.write_text('synthetic-key')
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Images)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    process = subprocess.Popen([sys.argv[1], '--emp-image-mcp',
        f'http://127.0.0.1:{server.server_port}/v1', str(key)],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    replies = queue.Queue()
    def read_replies():
        for line in process.stdout:
            replies.put(json.loads(line))

    reader = threading.Thread(target=read_replies, daemon=True)
    reader.start()
    try:
        send(process, {'id': 0, 'method': 'unknown'})
        reply = replies.get(timeout=3)
        assert reply['jsonrpc'] == '2.0' and reply['id'] == 0 and reply['error']['code'] == -32601
        call(process, 1, prompt='wait')
        observed.get(timeout=5)
        send(process, {'method': 'notifications/cancelled', 'params': {'requestId': 1}})
        send(process, {'id': 2, 'method': 'ping'})
        assert replies.get(timeout=3)['id'] == 2, 'ping must not wait for image generation'
        assert disconnected.wait(3), 'cancel must close the upstream connection'
        assert not list(root.glob('generated_images/*')), 'cancelled image must not be saved'
        for request_id, model, format in [(3, 'gpt-image-2.6-sunburst', 'svg'), (4, 'gpt-image-2.6-flare', 'gif')]:
            args = dict(prompt='generate', model=model, output_format=format, complexity='low')
            if format == 'gif':
                args.update(frame_count=4, frame_duration_ms=200)
            call(process, request_id, **args)
            upstream = observed.get(timeout=5)
            for name, value in args.items():
                assert upstream[name] == value
            reply = replies.get(timeout=5)
            assert reply['id'] == request_id and not reply['result'].get('isError'), reply
            path = pathlib.Path(json.loads(reply['result']['content'][0]['text'])['path'])
            assert path.suffix == f'.{format}' and path.read_bytes(), path
        disconnected.clear()
        call(process, 5, prompt='wait')
        observed.get(timeout=5)
        process.stdin.close()
        process.wait(timeout=5)
        assert process.returncode == 0, process.stderr.read()
        assert disconnected.wait(3), 'MCP EOF must close the upstream connection'
        print(json.dumps({'cancel_and_ping': 'passed', 'eof_cancel': 'passed',
            'image_26_svg_gif': 'passed', 'paid_requests': 0}))
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        server.shutdown()
        server.server_close()
