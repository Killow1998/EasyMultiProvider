import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import platform
import subprocess
import threading
import time


class Release(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if getattr(self.server, 'delay_download', False) and self.path.startswith('/releases/download/'):
            self.server.delayed_requests += 1
            time.sleep(65)
        tag = 'v' + self.server.version
        releases = {tag: self.server.assets, **getattr(self.server, 'module_releases', {})}

        def metadata(release_tag, assets):
            notes = getattr(self.server, 'release_notes', {}).get(release_tag)
            return dict(tag_name=release_tag, draft=False, prerelease='-beta' in release_tag, body=notes, assets=[
                dict(name=name, size=len(data), digest='sha256:' + hashlib.sha256(data).hexdigest(),
                     browser_download_url=f'http://127.0.0.1:{self.server.server_port}/releases/download/{release_tag}/{name}')
                for name, data in assets.items()
            ])

        paginated = getattr(self.server, 'paginate_modules', False) and self.path.startswith('/releases?')
        second_page = paginated and 'page=2' in self.path
        if self.path == '/releases/tags/' + tag:
            body = json.dumps(metadata(tag, self.server.assets)).encode()
        elif self.path.split('?', 1)[0] == '/releases':
            entries = releases.items()
            if paginated:
                entries = [(key, value) for key, value in entries if (key != tag) == second_page]
            body = json.dumps([metadata(release_tag, assets) for release_tag, assets in entries]).encode()
        elif self.path.startswith('/releases/download/'):
            release_tag, name = self.path.rsplit('/', 2)[-2:]
            if hasattr(self.server, 'package_requests'):
                self.server.package_requests.append(release_tag)
            if name not in releases.get(release_tag, {}):
                self.send_error(404)
                return
            body = releases[release_tag][name]
            if self.server.corrupt == name:
                body = bytes([body[0] ^ 1]) + body[1:]
        else:
            self.send_error(404)
            return
        self.send_response(200)
        if paginated and not second_page:
            self.send_header('Link', f'<http://127.0.0.1:{self.server.server_port}/releases?per_page=100&page=2>; rel="next"')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def http_server(stack, handler, tls=None):
    server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
    if tls:
        server.socket = tls.wrap_socket(server.socket, server_side=True)
    stack.callback(server.server_close)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    stack.callback(server.shutdown)
    return server


def local_module_release(stack, binary, modules):
    system = {'Darwin': 'macos', 'Linux': 'linux', 'Windows': 'windows'}[platform.system()]
    machine = platform.machine().lower()
    arch = {'amd64': 'x86_64', 'arm64': 'aarch64'}.get(machine, machine)
    extension = '.exe' if system == 'windows' else ''
    assets = {
        f'EMP-mod-{module}-{system}-{arch}{extension}':
            binary.with_name(f'empmod-{module}{extension}').read_bytes()
        for module in modules
    }
    release = http_server(stack, Release)
    release.version = subprocess.check_output([str(binary), '--version'], text=True).split()[-1]
    release.assets = assets
    release.corrupt = None
    base = f'http://127.0.0.1:{release.server_port}'
    return {'EMP_UPDATE_TEST_REPOSITORY_URL': base,
            'EMP_UPDATE_TEST_API_URL': base + '/releases?per_page=100'}
