"""Startup recovery in Firefox using disposable EMP data only."""
import argparse
import base64
import contextlib
import json
import os
import queue
import subprocess
import tempfile
import threading
from pathlib import Path
from e2e import Browser
from e2e_environment import request, stop, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-recovery-browser-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        browser = Browser(stack, root)
        for keep in [False, True]:
            data = root / ('backup' if keep else 'clear')
            data.mkdir()
            (data / 'state').mkdir(mode=0o700)
            key = data / 'state/master.key'
            key.write_text('invalid-fixture-key')
            key.chmod(0o600)
            config = data / 'config.json'
            config.write_text(json.dumps({'auto_enable_on_start': False}))
            original = config.read_bytes()
            codex = data / 'codex'
            codex.mkdir()
            (codex / 'config.toml').write_text('# native settings preserved\n')
            env = {k: v for k, v in os.environ.items() if k not in [
                'EASY_MULTI_PROVIDER_MASTER_KEY', 'EASY_MULTI_PROVIDER_MASTER_KEY_FILE', 'EASY_MULTI_PROVIDER_CONFIG', 'BROWSER']}
            env.update({'CODEX_HOME': str(codex), 'HOME': str(data), 'USERPROFILE': str(data),
                        'XDG_CONFIG_HOME': str(data / 'xdg'), 'NO_PROXY': '127.0.0.1,localhost',
                        'HTTP_PROXY': 'http://127.0.0.1:1', 'HTTPS_PROXY': 'http://127.0.0.1:1'})
            errors = stack.enter_context((data / 'stderr.log').open('w'))
            process = subprocess.Popen([str(args.emp.resolve()), 'serve', '--config', str(config), '--port', '0'],
                                       env=env, stdout=subprocess.PIPE, stderr=errors, text=True)
            stack.callback(stop, process)
            lines = queue.Queue()
            def drain(output=process.stdout, destination=lines):
                for line in output:
                    destination.put(line.strip())
            threading.Thread(target=drain, daemon=True).start()

            def next_url(prefix):
                while True:
                    line = lines.get(timeout=20)
                    if line.startswith(prefix):
                        return line[len(prefix):]

            recovery_url = next_url('Recovery page: ')
            status, result = request(browser.base + '/url', {'url': recovery_url})
            assert status == 200, result
            wait_for(lambda: browser.execute('return !document.getElementById("reset").disabled;'))
            assert browser.execute('return !document.getElementById("retry") && !document.getElementById("path");')
            assert not browser.execute('return document.getElementById("keepBackup").checked;')
            for language in ['zh', 'en']:
                for width in [390, 1280]:
                    request(browser.base + '/window/rect', {'width': width, 'height': 900})
                    browser.execute('const s=document.getElementById("language");s.value=arguments[0];s.dispatchEvent(new Event("change"));document.getElementById("reset").click();', language)
                    assert browser.execute('return document.getElementById("confirm").open;')
                    assert browser.execute('return document.documentElement.scrollWidth<=innerWidth && document.getElementById("confirm").scrollWidth<=document.getElementById("confirm").clientWidth;')
                    if not keep:
                        _, screenshot = request(browser.base + '/screenshot')
                        image = args.output.with_name(args.output.stem + f'-{language}-{width}.png')
                        image.write_bytes(base64.b64decode(screenshot['value']))
                    browser.execute('document.getElementById("cancel").click();')
                    assert config.read_bytes() == original
                    assert key.read_text() == 'invalid-fixture-key'
            browser.execute('document.getElementById("reset").click();document.getElementById("keepBackup").checked=arguments[0];document.getElementById("commit").click();', keep)
            wait_for(lambda: browser.execute('return document.getElementById("status").textContent.includes("EMP is starting");'))
            normal_url = next_url('Open in browser: ')
            if keep:
                backups = list(data.glob('recovery-backup-*'))
                assert len(backups) == 1
                assert (backups[0] / 'state/master.key').read_text() == 'invalid-fixture-key'
                assert 'Old data backup:' in browser.execute('return document.getElementById("status").textContent;')
            else:
                assert not list(data.glob('recovery-*'))
            status, result = request(browser.base + '/url', {'url': normal_url})
            assert status == 200, result
            wait_for(lambda: browser.execute('return !!localStorage.getItem("emp_management_session_v1");'))
            token = browser.execute('return localStorage.getItem("emp_management_session_v1");')
            base = normal_url.split('/?')[0]
            assert request(base + '/api/quit', {}, {'X-EMP-Session': token})[0] == 200
            assert process.wait(timeout=10) == 0
            assert (codex / 'config.toml').read_text() == '# native settings preserved\n'
            checks.append({'backup_selected': keep, 'cancel_preserves_data': True,
                           'default_backup_unchecked': True, 'normal_ui_after_reset': True,
                           'languages': ['zh', 'en'], 'widths': [390, 1280]})
    args.output.write_text(json.dumps({'checks': checks, 'real_credentials_used': False}, indent=2) + '\n')
    print(json.dumps({'passed': len(checks), 'output': str(args.output)}))


if __name__ == '__main__':
    main()
