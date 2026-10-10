#!/usr/bin/env python3
"""Local frontend preview. The only production requests made here are GETs."""
import argparse
import http.client
import json
import mimetypes
import os
import sys
import secrets
import sqlite3
import threading
import urllib.parse
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from shadow_usage import aggregate, parameters
from frontend import frontend

def default_state_dir():
    configured = os.environ.get('EASY_MULTI_PROVIDER_CONFIG')
    if configured:
        return Path(configured).expanduser().resolve().parent / 'state'
    if sys.platform == 'win32':
        root = Path(os.environ.get('LOCALAPPDATA') or os.environ.get('APPDATA') or Path.home()/'AppData/Local')
        return root/'EasyMultiProvider/state'
    if sys.platform == 'darwin':
        return Path.home()/'Library/Application Support/EasyMultiProvider/state'
    return Path(os.environ.get('XDG_CONFIG_HOME') or Path.home()/'.config')/'easy-multi-provider/state'

READ_PATHS = {
    '/api/config', '/api/accounts', '/api/accounts/events', '/api/integration',
    '/api/calls', '/api/usage', '/api/diagnostics', '/api/support-report',
    '/api/updates', '/api/runtime/claude-cli', '/api/capabilities',
    '/api/request-limits', '/api/models/vision-test-image', '/api/models/audio-test-sound',
}


def allowed_read(path):
    if path in READ_PATHS:
        return True
    parts = path.split('/')
    return (len(parts) == 5 and parts[1] == 'api'
            and parts[2] in {'accounts', 'providers'}
            and parts[3] not in {'', '.', '..'}
            and parts[4] in {'models', 'quota-history'})


class ShadowHandler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass  # URLs, query strings and provider/account data stay out of logs.

    def origin_allowed(self):
        host = self.headers.get('Host', '')
        origin = self.headers.get('Origin')
        return (host in self.server.hosts
                and (not origin or origin == 'http://' + host))

    def send(self, status, body, content_type='application/json'):
        if not isinstance(body, bytes):
            body = json.dumps(body, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Content-Security-Policy', "frame-ancestors 'none'")
        self.send_header('Referrer-Policy', 'no-referrer')
        self.send_header('Connection', 'close')
        self.end_headers()
        self.wfile.write(body)
        self.close_connection = True

    def error(self, status, message):
        self.send(status, {'error': {'message': message}})

    def do_GET(self):
        if not self.origin_allowed() or not self.path.startswith('/'):
            return self.error(403, 'Invalid origin')
        path = urllib.parse.urlsplit(self.path).path
        if path == '/' or path.startswith('/assets/'):
            name = 'index.html' if path == '/' else path[len('/assets/'):]
            if '/' in name or name in {'', '.', '..'}:
                return self.error(404, 'Not found')
            try:
                body = frontend(name, self.server.bootstrap)
            except FileNotFoundError:
                return self.error(404, 'Not found')
            return self.send(200, body, mimetypes.guess_type(name)[0] or 'application/octet-stream')
        if not secrets.compare_digest(self.headers.get('X-EMP-Session', ''), self.server.session):
            return self.error(401, 'management session is required')
        if path == '/api/shadow/usage':
            try:
                filters = parameters(urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query))
            except (ValueError, OverflowError):
                return self.error(400, '请选择有效的统计时间段与筛选条件。')
            connection = http.client.HTTPConnection('127.0.0.1', self.server.backend_port, timeout=30)
            try:
                query = {key: filters[key] for key in ['start', 'end', 'category']}
                if filters.get('account'):
                    query['account_id'] = filters['account']
                token = json.loads(self.server.state_dir.joinpath('web-session.json').read_text(encoding='utf-8'))['token']
                connection.request('GET', '/api/usage?'+urllib.parse.urlencode(query), headers={
                    'X-EMP-Session': token, 'Origin': f'http://127.0.0.1:{self.server.backend_port}',
                })
                upstream = connection.getresponse()
                body = upstream.read()
                if upstream.status != 200:
                    return self.send(upstream.status, body)
                return self.send(200, aggregate(self.server.state_dir/'usage.sqlite3', filters, json.loads(body)))
            except (BrokenPipeError, ConnectionResetError):
                pass
            except (OSError, ValueError, KeyError, sqlite3.Error, http.client.HTTPException):
                return self.error(503, '统计数据读取失败，请重试。')
            finally:
                connection.close()
        if not allowed_read(path):
            return self.error(404, 'Not found')
        connection = http.client.HTTPConnection('127.0.0.1', self.server.backend_port, timeout=30)
        try:
            token = json.loads(self.server.state_dir.joinpath('web-session.json').read_text(encoding='utf-8'))['token']
            connection.request('GET', self.path, headers={
                'X-EMP-Session': token, 'Origin': f'http://127.0.0.1:{self.server.backend_port}',
                'Accept': self.headers.get('Accept', 'application/json'),
            })
            upstream = connection.getresponse()
            if path != '/api/accounts/events' or upstream.status != 200:
                return self.send(upstream.status, upstream.read(), upstream.getheader('Content-Type', 'application/json'))
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Content-Type-Options', 'nosniff')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.close_connection = True
            while line := upstream.readline():
                self.wfile.write(line)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except (OSError, ValueError, KeyError, http.client.HTTPException):
            if path != '/api/accounts/events':
                self.error(502, '无法读取 EMP 数据，请检查 EMP 是否正在运行。')
        finally:
            connection.close()

    def do_POST(self):
        if not self.origin_allowed():
            return self.error(403, 'Invalid origin')
        if self.path == '/api/session' and secrets.compare_digest(
                self.headers.get('X-EMP-Bootstrap', ''), self.server.bootstrap):
            return self.send(200, {'session': self.server.session})
        self.error(403, '影子页只保存预览配置。')

    def do_DELETE(self):
        self.error(403, '影子页只保存预览配置。')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=4201)
    parser.add_argument('--backend-port', type=int, default=4200)
    parser.add_argument('--state-dir', type=Path, default=default_state_dir(), help='EMP state directory containing web-session.json and usage.sqlite3')
    parser.add_argument('--open', action='store_true')
    args = parser.parse_args()
    server = ThreadingHTTPServer(('127.0.0.1', args.port), ShadowHandler)
    server.daemon_threads = True
    port = server.server_port
    server.hosts = {f'127.0.0.1:{port}', f'localhost:{port}'}
    server.backend_port = args.backend_port
    server.state_dir = args.state_dir.expanduser().resolve()
    server.bootstrap = secrets.token_urlsafe(32)
    server.session = secrets.token_urlsafe(32)
    if args.open:
        threading.Timer(0.5, lambda: webbrowser.open(f'http://127.0.0.1:{port}/')).start()
    print(f'EMP shadow: http://127.0.0.1:{port}/', flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == '__main__':
    main()
