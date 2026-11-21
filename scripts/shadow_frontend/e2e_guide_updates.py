"""Guide publication, download, and offline recovery with a disposable EMP."""
import argparse
import base64
import contextlib
import hashlib
from http.server import BaseHTTPRequestHandler
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request

from e2e_official_modules import Emp
from e2e_refinements import Browser
from e2e_tutorials import screenshot
from local_backend import wait_for
from module_release_fixture import http_server

PROJECT = Path(__file__).resolve().parents[2]
MANIFEST = '/guide/manifest.json'
API = '/api/resources/tutorials'
DOC = 'README.zh-CN.md'
GIF = 'docs/assets/tutorials/00-native-context.gif'
GIF_URL = '/assets/tutorial-00-native-context.gif'
TINY_GIF = base64.b64decode('R0lGODlhAQABAIABAP///wAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==')


class Release(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.server.requests.append(self.path)
        body = self.server.assets.get(self.path)
        if body is None:
            self.send_error(503)
            return
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def publish(server, files, version, *, corrupted=()):
    manifest = {
        'schema_version': 1, 'version': version,
        'files': {name: {'size': len(body), 'sha256': hashlib.sha256(body).hexdigest()}
                  for name, body in files.items()},
    }
    server.assets = {MANIFEST: json.dumps(manifest).encode(),
                     **{f'/guide-v{version}/content/{name}':
                        (bytes([body[0] ^ 1]) + body[1:] if name in corrupted else body)
                        for name, body in files.items()}}


def get(emp, path):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(emp.base + path, timeout=10) as response:
        return response.read()


def expect_version(emp, version):
    return wait_for(lambda: (emp.api(API) or {}).get('version') == version)


def reopen(browser):
    browser.execute('closeModal(); tutorials.open(); document.querySelector("[data-tutorial=team_join]").click();')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--no-browser', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-guide-e2e-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        package = root / 'guide-v1'
        subprocess.run(['node', 'scripts/prepare_guide_release.cjs', '1', str(package)],
                       cwd=PROJECT, check=True)
        manifest = json.loads((package / 'manifest.json').read_text())
        files = {name: (package / 'content' / name).read_bytes() for name in manifest['files']}
        server = http_server(stack, Release)
        server.assets, server.requests = {}, []
        source = f'http://127.0.0.1:{server.server_port}'
        emp = Emp(stack, args.emp.resolve(), root / 'instance', {}, source, guide_source=source)
        pid = emp.process.pid
        assert emp.api(API) is None
        wait_for(lambda: MANIFEST in server.requests)
        assert get(emp, GIF_URL) == files[GIF]
        assert '<h2' in get(emp, '/guide/docs/self-update.md').decode()

        publish(server, files, 1)
        expect_version(emp, 1)
        assert get(emp, GIF_URL) == files[GIF]
        assert [entry for entry in server.requests if '/content/' in entry] == [
            '/guide-v1/content/tutorials.json']
        if not args.no_browser:
            browser = Browser(stack, root, reduced_motion=0)
            browser.navigate(emp.url)
            browser.execute('window.guideTestMarker = true; tutorials.open();')
            browser.execute('document.querySelector("[data-tutorial=native_context]").click();')
            wait_for(lambda: browser.execute('return document.querySelector("#tutorial_media img")?.naturalWidth > 1;'))

        pack = json.loads(files['tutorials.json'])
        for lesson in pack['team_join'].values():
            lesson['title'] = 'Guide release 2'
            lesson['steps'][0] = 'Use <Team>'
        updated = {**files, GIF: TINY_GIF, 'tutorials.json': json.dumps(pack).encode(),
                   DOC: b'# Cloud README 2\n\n<script>alert("ignored")</script>\n\n'
                        b'[Unsafe](javascript:alert%281%29) | [Guide](docs/tutorials.md)\n'}
        publish(server, updated, 2)
        expect_version(emp, 2)
        assert set(entry for entry in server.requests if entry.startswith('/guide-v2/')) == {
            f'/guide-v2/content/{DOC}', f'/guide-v2/content/{GIF}', '/guide-v2/content/tutorials.json'}
        assert get(emp, GIF_URL) == TINY_GIF
        article = get(emp, f'/guide/{DOC}').decode()
        assert '<h1 id="cloud-readme-2">Cloud README 2</h1>' in article
        assert '<script>' not in article and 'javascript:' not in article
        assert 'href="/guide/docs/tutorials.md"' in article
        if not args.no_browser:
            reopen(browser)
            wait_for(lambda: browser.execute('return document.getElementById("tutorial_title").textContent === "Guide release 2";'))
            assert browser.execute('return window.guideTestMarker && document.querySelector("#tutorial_join_steps li").textContent === "Use <Team>";')
            screenshot(browser, args.output / 'hot-updated-tutorial.png')
            browser.execute('document.querySelector("[data-tutorial=native_context]").click();')
            wait_for(lambda: browser.execute('return document.querySelector("#tutorial_media img")?.naturalWidth === 1;'))
            assert browser.execute('return document.querySelector(".tutorial-documents a[href*=README]") !== null;')
            browser.execute('location.href = "/guide/docs/self-update.md#linux-installation-and-migration";')
            wait_for(lambda: browser.execute('return document.getElementById("linux-installation-and-migration")?.textContent;'))
            screenshot(browser, args.output / 'installation-and-updates.png')

        manifest_checks = server.requests.count(MANIFEST)
        wait_for(lambda: emp.api(API) is not None and server.requests.count(MANIFEST) > manifest_checks)
        assert sum(entry.startswith('/guide-v2/') for entry in server.requests) == 3
        failed = {**updated, DOC: b'# Incomplete guide 3\n', GIF: files[GIF][:-1]}
        publish(server, failed, 3, corrupted={GIF})
        wait_for(lambda: emp.api(API) and f'/guide-v3/content/{GIF}' in server.requests)
        assert emp.api(API)['version'] == 2 and get(emp, GIF_URL) == TINY_GIF
        assert get(emp, f'/guide/{DOC}').decode() == article
        server.assets = {MANIFEST: json.dumps({'schema_version': 2, 'version': 4, 'files': {}}).encode()}
        manifest_checks = server.requests.count(MANIFEST)
        wait_for(lambda: emp.api(API) and server.requests.count(MANIFEST) > manifest_checks)
        assert emp.api(API)['version'] == 2
        publish(server, files, 1)
        manifest_checks = server.requests.count(MANIFEST)
        wait_for(lambda: emp.api(API) and server.requests.count(MANIFEST) > manifest_checks)
        assert emp.api(API)['version'] == 2 and emp.process.pid == pid

        server.assets = {}
        emp.restart()
        assert emp.api(API)['version'] == 2 and get(emp, GIF_URL) == TINY_GIF
        assert get(emp, f'/guide/{DOC}').decode() == article
    print('Guide release generation, embedded fallback, delta download, hot activation, offline restart, and invalid/old manifests passed.')


if __name__ == '__main__':
    main()
