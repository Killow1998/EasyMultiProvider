"""Disposable HTTP/backend processes for frontend acceptance."""
import json
import os
import re
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def request(url, payload=None, headers=None, method=None):
    req = urllib.request.Request(url, data=None if payload is None else json.dumps(payload).encode(),
                                 headers={'Content-Type': 'application/json', **(headers or {})}, method=method)
    try:
        response = urllib.request.urlopen(req, timeout=15)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, json.loads(response.read() or '{}')


def wait_for(check, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(.1)
    raise TimeoutError('Isolated fixture did not become ready')


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


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


def start_backend(stack, binary, root):
    upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
    stack.callback(upstream.server_close)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    stack.callback(upstream.shutdown)
    config = root / 'config.json'
    config.write_text(json.dumps({'providers': [{'id': 'demo', 'name': 'Demo', 'base_url':
        f'http://127.0.0.1:{upstream.server_port}/v1', 'protocol': 'chat_completions',
        'auth_mode': 'api_key', 'api_key': 'fixture-only'}], 'models': [{'id': 'demo/model',
        'provider': 'demo', 'upstream_id': 'upstream-model', 'enabled': True}]}))
    env = {key: value for key, value in os.environ.items() if key not in {
        'OPENAI_API_KEY', 'CODEX_API_KEY', 'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN',
        'ANTHROPIC_BASE_URL', 'EASY_MULTI_PROVIDER_CONFIG'}}
    for key in ['HOME', 'CODEX_HOME', 'XDG_CONFIG_HOME', 'APPDATA', 'LOCALAPPDATA']:
        env[key] = str(root / key.lower())
        Path(env[key]).mkdir()
    log_path = root / 'backend.log'
    log = stack.enter_context(log_path.open('w'))
    process = subprocess.Popen([str(binary), 'serve', '--config', str(config), '--port', '0'],
                               env=env, cwd=root, stdout=log, stderr=log)
    stack.callback(stop, process)
    url = wait_for(lambda: re.search(r'Open in browser: (http://[^\s]+)', log_path.read_text(encoding='utf-8')))[1]
    base = url.split('/?')[0]
    state = root / 'state'
    token = wait_for(lambda: json.loads((state / 'web-session.json').read_text(encoding='utf-8'))['token']
                     if (state / 'web-session.json').exists() else None)
    return base, url, state, {'X-EMP-Session': token}, Upstream
