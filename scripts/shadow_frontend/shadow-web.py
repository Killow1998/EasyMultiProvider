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
import subprocess
import threading
import urllib.parse
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from frontend import frontend
from shadow_migration import MigrationPreview
from migration_selection import PreviewError
from shadow_release_notes import ReleaseNotes

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
                body = frontend(name, self.server.bootstrap, getattr(self.server, 'real_context', None))
            except FileNotFoundError:
                return self.error(404, 'Not found')
            return self.send(200, body, mimetypes.guess_type(name)[0] or 'application/octet-stream')
        if not secrets.compare_digest(self.headers.get('X-EMP-Session', ''), self.server.session):
            return self.error(401, 'management session is required')
        snapshot = getattr(self.server, 'snapshot', None)
        if path == '/api/shadow/status' and snapshot:
            return self.send(200, snapshot.status())
        if path == '/api/usage' and snapshot:
            try:
                def source_read(target):
                    from local_backend import request
                    token = json.loads(self.server.state_dir.joinpath('web-session.json').read_text())['token']
                    origin = f'http://127.0.0.1:{self.server.backend_port}'
                    return request(origin+target,headers={'X-EMP-Session':token,'Origin':origin},timeout=30)
                status, payload = snapshot.usage(self.path, source_read)
                return self.send(status, payload)
            except (OSError, ValueError, http.client.HTTPException):
                return self.error(503, '无法读取候选版账单快照 / Candidate accounting snapshot is unavailable')
        if path == '/api/shadow/release-notes':
            try:
                return self.send(200,self.server.release_notes.read())
            except (OSError,ValueError):
                return self.error(502,'无法读取官方更新说明，请稍后重试。 / Could not read official release notes.')
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
                body = upstream.read()
                if path == '/api/config' and upstream.status == 200:
                    body = json.loads(body)
                    # Older live backends lack this projection. Read their actual
                    # catalog; never substitute demonstration models into Shadow.
                    # Versionless reads intentionally strip Ultra for old clients.
                    models = body.get('catalog_models', [])
                    if any('ultra_reasoning_levels' not in model or 'ultra_default_effort' not in model for model in models):
                        from local_backend import request
                        status, catalog = request(f'http://127.0.0.1:{self.server.backend_port}/v1/models?client_version=0.163.0',
                            headers={'X-EMP-Session':token}, timeout=30)
                        if status != 200:
                            return self.error(503, 'Could not read the live model catalog')
                        entries = {model['slug']:model for model in catalog.get('models', [])}
                        concrete = ['none','minimal','low','medium','high','xhigh','max']
                        for model in models:
                            entry = entries.get(model['id'], {})
                            ordered_levels = [level.get('effort') for level in entry.get('supported_reasoning_levels', [])]
                            levels = set(ordered_levels)
                            model['ultra_reasoning_levels'] = [level for level in concrete if level in levels] if model.get('source_type') != 'provider' and 'ultra' in levels else []
                            if 'ultra_default_effort' not in model:
                                # Published catalogs can contain an EMP override; never label that as official.
                                override = body.get('ultra_reasoning_efforts', {}).get(model.get('family_id'))
                                default = entry.get('multi_agent_reasoning_effort')
                                fallback = next((level for level in reversed(ordered_levels) if level in concrete), 'medium')
                                model['ultra_default_effort'] = (default if default in concrete and default in levels else 'max' if 'max' in levels else fallback) if model['ultra_reasoning_levels'] and not override else None
                if path == '/api/config' and snapshot and upstream.status == 200:
                    status, candidate = snapshot.read('/api/config')
                    if status != 200:
                        return self.error(503, 'Candidate configuration is unavailable')
                    for key in ['emp_version', 'codex_context_settings']:
                        if key in candidate:
                            body[key] = candidate[key]
                return self.send(upstream.status, body, upstream.getheader('Content-Type', 'application/json'))
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
        if self.path in {'/api/shadow/snapshot', '/api/shadow/demo'} and getattr(self.server, 'snapshot', None):
            if not secrets.compare_digest(self.headers.get('X-EMP-Session', ''), self.server.session):
                return self.error(401, 'management session is required')
            try:
                if self.path.endswith('/demo'):
                    length=int(self.headers.get('Content-Length','0'))
                    if not 0<length<=256:return self.error(400,'Invalid scenario')
                    scenario=json.loads(self.rfile.read(length)).get('scenario','')
                    return self.send(200, {'url':self.server.open_demo(scenario)})
                info = self.server.snapshot.refresh()
                self.send(200, {'snapshot':info, 'command_id':0})
                return
            except (OSError, ValueError, sqlite3.Error, TimeoutError):
                return self.error(503, '无法更新影子快照 / Could not refresh the Shadow snapshot')
        if self.path in {'/api/shadow/migration/decode', '/api/shadow/migration/preview', '/api/shadow/migration/export','/api/shadow/migration/demo'}:
            if not secrets.compare_digest(self.headers.get('X-EMP-Session',''),self.server.session):
                return self.error(401,'management session is required')
            try:
                length = int(self.headers.get('Content-Length','0'))
                if not 0 < length <= 64*1024*1024:
                    return self.error(413,'迁移包过大 / Migration request too large')
                body = json.loads(self.rfile.read(length))
                if not isinstance(body,dict):
                    return self.error(400,'Invalid request')
                result = self.server.migration.handle(self.path.rsplit('/',1)[1],body)
                return self.send(200,result)
            except PreviewError as error:
                return self.error(400,str(error))
            except (ValueError,KeyError,TypeError,OSError,subprocess.TimeoutExpired):
                return self.error(400,'迁移预览失败，请检查文件、密码和所选服务。 / Check the package, password and selected services.')
        self.error(403, '影子页只保存预览配置。')

    def do_DELETE(self):
        self.error(403, '影子页只保存预览配置。')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=4201)
    parser.add_argument('--emp', type=Path, help='Candidate binary; preview real data with isolated accounting by default')
    parser.add_argument('--demo', action='store_true', help='Explicitly use synthetic demonstration data')
    parser.add_argument('--codex-home', type=Path, help='Read-only Codex metadata source for candidate accounting')
    parser.add_argument('--scratch-dir', type=Path, help='Disk-backed private directory for disposable snapshots')
    parser.add_argument('--backend-port', type=int, default=4200)
    parser.add_argument('--state-dir', type=Path, default=default_state_dir(), help='EMP state directory containing web-session.json and usage.sqlite3')
    parser.add_argument('--open', action='store_true')
    parser.add_argument('--migration-helper',type=Path,help='Built emp-state shadow_migration_codec example')
    parser.add_argument('--config',type=Path,help='Read-only production configuration for migration preview')
    parser.add_argument('--release-api',help='Loopback release metadata endpoint for isolated acceptance')
    args = parser.parse_args()
    if args.release_api and urllib.parse.urlsplit(args.release_api).hostname != '127.0.0.1':
        parser.error('--release-api must use a loopback fixture')
    project = Path(__file__).resolve().parents[2]
    name = 'shadow_migration_codec.exe' if sys.platform == 'win32' else 'shadow_migration_codec'
    candidates = [project/'target/debug/examples'/name,
                  project.parent/'.emp-rust-target-root/per-worktree'/project.name/'debug/examples'/name]
    args.migration_helper = args.migration_helper or next((path for path in candidates if path.is_file()),None)
    if args.emp:
        if args.demo:
            from preview import serve
            serve(args.emp, args.port, args.open)
        else:
            from real_preview import serve
            serve(ShadowHandler, args)
        return
    if args.demo:
        parser.error('--demo requires --emp')
    server = ThreadingHTTPServer(('127.0.0.1', args.port), ShadowHandler)
    server.daemon_threads = True
    port = server.server_port
    server.hosts = {f'127.0.0.1:{port}', f'localhost:{port}'}
    server.backend_port = args.backend_port
    server.state_dir = args.state_dir.expanduser().resolve()
    server.bootstrap = secrets.token_urlsafe(32)
    server.session = secrets.token_urlsafe(32)
    config = args.config or Path(os.environ.get('EASY_MULTI_PROVIDER_CONFIG',server.state_dir.parent/'config.json'))
    native_auth = Path(os.environ.get('CODEX_HOME',Path.home()/'.codex'))/'auth.json'
    server.migration = MigrationPreview(args.migration_helper,config,native_auth)
    server.release_notes = ReleaseNotes(args.release_api) if args.release_api else ReleaseNotes()
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
