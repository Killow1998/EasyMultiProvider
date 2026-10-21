"""Serve candidate assets against a disposable backend for preview and acceptance."""
import http.client
import mimetypes
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit


def candidate_frontend(stack, backend, web, index_extra="", port=0, index_head="", pages=None):
    destination = urlsplit(backend)

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def serve(self):
            path = urlsplit(self.path).path
            if self.command == 'GET' and path in (pages or {}):
                body = pages[path]().encode()
                status, content_type = 200, 'text/html; charset=utf-8'
            elif path.startswith('/api/recovery'):
                # Recovery demos simulate requests in the page; never forward a reset.
                status, content_type, body = 403, 'application/json', b'{"error":"preview_only"}'
            elif self.command == 'GET' and (path == '/' or path.startswith('/assets/')):
                name = 'index.html' if path == '/' else path.removeprefix('/assets/')
                if '/' in name or not (web / name).is_file():
                    self.send_error(404)
                    return
                body = (web / name).read_bytes()
                if name == "index.html" and index_extra:
                    extra = index_extra() if callable(index_extra) else index_extra
                    body = body.replace(b"</body>", extra.encode() + b"</body>")
                if name == "index.html" and index_head:
                    body = body.replace(b"<head>", b"<head>"+index_head.encode())
                status, content_type = 200, mimetypes.guess_type(name)[0] or 'application/octet-stream'
            elif path == '/api/accounts/events':
                status, content_type, body = 204, 'text/event-stream', b''
            else:
                body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                if path == '/api/migration/preview' and self.server.preview_gate:
                    self.server.preview_started.set()
                    self.server.preview_gate.wait(60)
                headers = {key: value for key, value in self.headers.items()
                           if key.lower() not in {'host', 'origin', 'connection'}}
                if 'Origin' in self.headers:
                    headers['Origin'] = backend
                # Unoptimized scrypt is slow; preview generation keeps its normal deadline.
                timeout = 90 if path in {'/api/migration/decode', '/api/migration/export'} else 15
                connection = http.client.HTTPConnection(destination.hostname, destination.port, timeout=timeout)
                try:
                    connection.request(self.command, self.path, body=body, headers=headers)
                    response = connection.getresponse()
                    status, content_type, body = response.status, response.getheader('Content-Type'), response.read()
                finally:
                    connection.close()
            try:
                self.send_response(status)
                self.send_header('Content-Type', content_type or 'application/json')
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass  # An intentionally cancelled fixture request.

        do_GET = serve
        do_POST = serve
        do_PUT = serve
        do_DELETE = serve

    server = ThreadingHTTPServer(('127.0.0.1', port), Handler)
    server.preview_gate = None
    server.preview_started = threading.Event()
    stack.callback(server.server_close)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    return f'http://127.0.0.1:{server.server_port}', server
