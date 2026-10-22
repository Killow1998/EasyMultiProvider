"""Synthetic upstream and configuration for frontend acceptance."""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from local_backend import free_port, request, start_backend as run_backend, stop, wait_for


class Upstream(BaseHTTPRequestHandler):
    failed = True
    calls = 0

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        type(self).calls += 1
        status = 401 if self.failed else 200
        body = {'error': {'type': 'authentication_error', 'code': 'authentication_error', 'message': 'Fixture rejected'}} if self.failed else {
            'id': 'fixture', 'model': 'upstream-model', 'choices': [
                {'index': 0, 'message': {'role': 'assistant', 'content': 'ok'}, 'finish_reason': 'stop'}],
            'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'total_tokens': 12}}
        encoded = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def start_backend(stack, binary, root, configuration=None, env_overrides=None):
    upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
    stack.callback(upstream.server_close)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    stack.callback(upstream.shutdown)
    fixture={'providers': [{'id': 'demo', 'name': 'Demo', 'base_url':
        f'http://127.0.0.1:{upstream.server_port}/v1', 'protocol': 'chat_completions',
        'auth_mode': 'api_key', 'api_key': 'fixture-only'}], 'models': [{'id': 'demo/model',
        'provider': 'demo', 'upstream_id': 'upstream-model', 'enabled': True}]}
    fixture.update(configuration or {})
    return (*run_backend(stack, binary, root, fixture, env_overrides), Upstream)
