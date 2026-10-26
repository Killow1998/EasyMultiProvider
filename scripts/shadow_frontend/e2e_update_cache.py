"""Release notes remain readable while the real updater revalidates its disk cache."""
import argparse
import contextlib
import json
import re
import shutil
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_environment import request, stop, wait_for
from e2e_refinements import Browser


class Releases(BaseHTTPRequestHandler):
    gate = threading.Event()
    seen = []
    revision = 1
    fail = False
    version = ''
    history = False

    def log_message(self, *args):
        pass

    def do_GET(self):
        validator = self.headers.get('If-None-Match')
        type(self).seen.append(validator)
        if not self.gate.wait(15):
            return
        etag = f'"notes-{self.revision}"'
        status = 403 if self.fail else 304 if validator == etag else 200
        release = {
            'tag_name': 'v' + self.version, 'draft': False, 'prerelease': False, 'assets': [],
            'body': f'## 简体中文\n本地更新说明 {self.revision}\n## English\nCached release notes {self.revision}',
        }
        if self.history:
            name = 'EMP-linux-x86_64.tar.gz'
            latest = {**release, 'tag_name': 'v9.0.2',
                'body': '## 简体中文\n- 修复最新版本问题\n## English\n- Fix latest issue',
                'assets': [{'name': name, 'digest': 'sha256:' + 'a' * 64, 'size': 8,
                    'browser_download_url': f'http://127.0.0.1:{self.server.server_port}/releases/download/v9.0.2/{name}'}]}
            release = [release, {**latest, 'tag_name': 'v9.0.3-beta1', 'prerelease': True},
                {**release, 'tag_name': 'v9.0.1', 'body': '## 简体中文\n- 改进中间版本\n## English\n- Improve intermediate version'}, latest]
        body = b'' if status != 200 else json.dumps(release).encode()
        self.send_response(status)
        self.send_header('ETag', etag)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def backend(stack, binary, root, release_base):
    config = root / 'config.json'
    if not config.exists():
        config.write_text('{"auto_enable_on_start":false,"providers":[],"models":[]}')
    env = {name: str(root / name.lower()) for name in ['HOME', 'CODEX_HOME', 'XDG_CONFIG_HOME']}
    env.update({'PATH': '/usr/bin:/bin', 'HTTP_PROXY': 'http://127.0.0.1:9',
        'HTTPS_PROXY': 'http://127.0.0.1:9', 'NO_PROXY': '127.0.0.1,localhost',
        'EMP_UPDATE_TEST_REPOSITORY_URL': release_base,
        'EMP_UPDATE_TEST_API_URL': release_base + '/latest'})
    log_path = root / 'backend.log'
    log = stack.enter_context(log_path.open('w'))
    process = subprocess.Popen([str(binary), 'serve', '--config', str(config), '--port', '0'],
        env=env, cwd=root, stdout=log, stderr=log)
    stack.callback(stop, process)
    url = wait_for(lambda: re.search(r'Open in browser: (http://[^\s]+)', log_path.read_text()))[1]
    base = url.split('/?')[0]
    token = json.loads((root / 'state/web-session.json').read_text())['token']
    return base, url, {'X-EMP-Session': token, 'Origin': base}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    Releases.version = subprocess.check_output([str(args.emp.resolve()), '--version'], text=True).strip().split()[-1]
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-update-cache-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        # A read-only source checkout correctly requires manual installation.
        # Exercise portable updates on a writable, disposable installation instead.
        binary = root / args.emp.name
        shutil.copy2(args.emp.resolve(), binary)
        releases = ThreadingHTTPServer(('127.0.0.1', 0), Releases)
        stack.callback(releases.server_close)
        threading.Thread(target=releases.serve_forever, daemon=True).start()
        stack.callback(releases.shutdown)
        stack.callback(Releases.gate.set)
        release_base = f'http://127.0.0.1:{releases.server_port}'
        Releases.gate.set()
        with contextlib.ExitStack() as first:
            base, _, headers = backend(first, binary, root, release_base)
            assert request(base + '/api/updates/check', {}, headers)[0] == 202
            def current():
                snapshot = request(base + '/api/updates', headers=headers)[1]
                assert snapshot['state'] != 'error', {'snapshot': snapshot, 'requests': Releases.seen}
                return snapshot['state'] == 'current'
            wait_for(current)
            assert Releases.seen == [None]
        assert (root / 'state/update-release.json').exists()
        Releases.gate.clear()
        base, url, headers = backend(stack, binary, root, release_base)
        assert 'Cached release notes 1' in request(base + '/api/updates', headers=headers)[1]['release_notes']
        frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
        browser = Browser(stack, root)
        assert request(browser.base + '/url', {'url': url.replace(base, frontend)})[0] == 200
        wait_for(lambda: browser.execute('return !!state;'))
        headers['X-EMP-Session'] = browser.execute('return localStorage.getItem("emp_management_session_v1");')
        original_windows = request(browser.base + '/window/handles')[1]['value']
        for language, expected in [('zh-CN', '本地更新说明 1'), ('en', 'Cached release notes 1')]:
            browser.execute('setLanguage(arguments[0]);openUpdate();', language)
            wait_for(lambda: browser.execute('return document.getElementById("update_notes")?.textContent===arguments[0];', expected))
            assert not Releases.gate.is_set()
            browser.execute('closeModal();')
        assert Releases.seen == [None, '"notes-1"']
        checks.append('Disk cache survives EMP restart; Chinese/English notes render while the validator request is blocked')
        Releases.gate.set()
        wait_for(lambda: request(base + '/api/updates', headers=headers)[1]['state'] == 'current')
        checks.append('Conditional request returns 304 with no body; cached notes remain available')
        Releases.revision = 2
        browser.execute('openUpdate();')
        wait_for(lambda: browser.execute('return document.getElementById("update_notes")?.textContent==="Cached release notes 2";'))
        assert Releases.seen[-1] == '"notes-1"'
        checks.append('Editing notes without changing the release version replaces the cached body and validator')
        browser.execute('closeModal();')
        Releases.fail = True
        browser.execute('openUpdate();')
        wait_for(lambda: request(base + '/api/updates', headers=headers)[1]['state'] == 'error')
        assert browser.execute('return document.getElementById("update_notes").textContent;') == 'Cached release notes 2'
        assert Releases.seen[-1] == '"notes-2"'
        browser.execute('closeModal();openUpdate();')
        assert browser.execute('return document.getElementById("update_notes").textContent;') == 'Cached release notes 2'
        assert request(browser.base + '/window/handles')[1]['value'] == original_windows
        checks.append('Failed validation and reopening retain the last good notes; no installation or extra browser tab')
        Releases.fail, Releases.history, Releases.revision = False, True, 3
        browser.execute('closeModal();openUpdate();')
        wait_for(lambda: request(base + '/api/updates', headers=headers)[1]['state'] == 'available')
        for language, expected in [('zh-CN', ['新增与改进', '问题修复', '改进中间版本', '修复最新版本问题']),
                                   ('en', ['New and improved', 'Fixes', 'Improve intermediate version', 'Fix latest issue'])]:
            browser.execute('setLanguage(arguments[0]);closeModal();openUpdate();', language)
            wait_for(lambda: browser.execute('return document.getElementById("update_notes").textContent.includes(arguments[0]);', expected[-1]))
            notes = browser.execute('return document.getElementById("update_notes").textContent;')
            assert all(part in notes for part in expected), notes
            assert 'v9.0.1' in notes and 'v9.0.2' in notes and 'beta' not in notes and 'Cached release notes' not in notes
        checks.append('Cross-version notes include intermediate stable releases, group improvements/fixes and exclude installed versions and previews in both languages')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks,
        'real_downloads': 0, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
