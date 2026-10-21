"""Restore/quit history acceptance using disposable files, locks and real Firefox."""
import argparse
import base64
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile

from e2e_environment import request, stop, wait_for
from e2e_refinements import Browser

HERE = Path(__file__).resolve().parent
SUMMARY = 'Portable fixture summary for native resume'
OPAQUE = 'native-ciphertext-must-remain-opaque'


def history(home, digit):
    identifier = f'{digit * 8}-{digit * 4}-4{digit * 3}-8{digit * 3}-{digit * 12}'
    path = home / 'sessions' / f'{identifier}.jsonl'
    path.parent.mkdir(exist_ok=True)
    items = [
        {'type': 'compaction', 'id': 'emp-summary', 'encrypted_content': 'emp1:' +
         base64.urlsafe_b64encode(SUMMARY.encode()).decode().rstrip('=')},
        {'type': 'reasoning', 'id': 'native-reasoning', 'encrypted_content': OPAQUE, 'summary': []},
        {'type': 'function_call', 'id': 'tool-call', 'call_id': 'tool', 'name': 'lookup', 'arguments': '{}'},
        {'type': 'function_call_output', 'id': 'tool-output', 'call_id': 'tool', 'output': 'fixture-result'},
    ]
    records = [
        ('session_meta', {'id': identifier, 'history_mode': 'paginated'}),
        ('event_msg', {'type': 'task_started', 'turn_id': 'closed-turn'}),
        ('event_msg', {'type': 'task_complete', 'turn_id': 'closed-turn'}),
        ('compacted', {'message': 'old fixture', 'replacement_history': items,
                       'replacement_history_metadata': [{'client_authored': False}] * len(items),
                       'window_number': 0}),
    ]
    original = ''.join(json.dumps({'timestamp': '2026-10-21T00:00:00Z', 'ordinal': index,
                                  'type': kind, 'payload': payload}) + '\n'
                       for index, (kind, payload) in enumerate(records)).encode()
    path.write_bytes(original)
    with sqlite3.connect(home / 'state_1.sqlite') as db:
        db.execute('CREATE TABLE IF NOT EXISTS threads (id TEXT, rollout_path TEXT, history_mode TEXT)')
        db.execute('INSERT INTO threads VALUES (?, ?, ?)', (identifier, str(path), 'paginated'))
    return identifier, path, original


def converted(path, original):
    data = path.read_bytes()
    assert data.startswith(original)
    suffix = [json.loads(line) for line in data[len(original):].splitlines()]
    assert suffix and suffix[0]['type'] == 'compacted'
    items = [item['payload'] for item in suffix if item['type'] == 'response_item']
    assert any(item.get('type') == 'message' and SUMMARY in json.dumps(item) for item in items)
    assert any(item.get('encrypted_content') == OPAQUE for item in items)
    assert any(item.get('type') == 'function_call' and item.get('call_id') == 'tool' for item in items)
    assert any(item.get('type') == 'function_call_output' and item.get('call_id') == 'tool' for item in items)
    assert not any(str(item.get('encrypted_content', '')).startswith('emp1:') for item in items)


def launch(stack, binary, root, env, attempt):
    log_path = root / f'backend-{attempt}.log'
    log = stack.enter_context(log_path.open('w'))
    process = subprocess.Popen([str(binary), 'serve', '--config', str(root / 'config.json'), '--port', '0'],
                               env=env, cwd=root, stdout=log, stderr=log)
    stack.callback(stop, process)
    url = wait_for(lambda: re.search(r'Open in browser: (http://[^\s]+)', log_path.read_text()))[1]
    token = wait_for(lambda: json.loads((root / 'state/web-session.json').read_text())['token']
                     if (root / 'state/web-session.json').exists() else None)
    return process, url.split('/?')[0], url, {'X-EMP-Session': token}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-idle-history-') as temporary, contextlib.ExitStack() as stack:
        root, binary = Path(temporary), args.emp.resolve()
        home = root / 'codex'
        home.mkdir()
        original_config = 'model = "fixture-native"\nopenai_base_url = "http://127.0.0.1:9/v1"\n'
        (home / 'config.toml').write_text(original_config)
        env = {'PATH': os.defpath, 'HOME': str(root / 'host-home'), 'CODEX_HOME': str(home),
               'XDG_CONFIG_HOME': str(root / 'xdg'), 'HTTP_PROXY': 'http://127.0.0.1:9',
               'HTTPS_PROXY': 'http://127.0.0.1:9', 'ALL_PROXY': 'http://127.0.0.1:9',
               'NO_PROXY': 'localhost,127.0.0.1'}
        (root / 'config.json').write_text(json.dumps({'auto_enable_on_start': False,
            'providers': [{'id': 'demo', 'name': 'Demo', 'base_url': 'http://127.0.0.1:9/v1',
                           'protocol': 'chat_completions', 'api_key': 'fixture-only'}],
            'models': [{'id': 'demo/model', 'provider': 'demo', 'upstream_id': 'fixture', 'enabled': True}]}))
        locks = home / 'thread-writer-locks'
        locks.mkdir()
        (locks / '.coordination.lock').touch()
        idle = history(home, '1')
        busy = history(home, '2')
        writer = stack.enter_context((locks / f'{busy[0]}.lock').open('w'))
        fcntl.flock(writer, fcntl.LOCK_EX | fcntl.LOCK_NB)
        process, base, url, headers = launch(stack, binary, root, env, 1)

        def post(path):
            current = {'X-EMP-Session': json.loads((root / 'state/web-session.json').read_text())['token']}
            status, data = request(base + path, {'confirm_reload': True}, current)
            assert status == 200, data
            return data

        post('/api/integration/enable')
        outcome = post('/api/integration/restore')
        assert outcome['configuration']['state'] == 'native'
        report = outcome['history_restore']
        assert report['status'] == 'partial' and report['report']['threads_repaired'] == 1
        assert report['report']['threads_deferred'] == 1
        converted(idle[1], idle[2])
        assert busy[1].read_bytes() == busy[2]
        assert (home / 'config.toml').read_text() == original_config
        assert request(base + '/healthz')[0] == 200
        checks.append('Restore converts idle history, preserves the live writer and keeps EMP running')

        browser = Browser(stack, root)
        browser.navigate(url)
        for language, width in [('zh-CN', 390), ('en', 1280)]:
            request(browser.base + '/window/rect', {'width': width, 'height': 1000})
            browser.execute('setLanguage(arguments[0]);', language)
            wait_for(lambda: browser.execute('return !document.getElementById("native_history_retry").hidden;'))
            assert browser.execute('return !document.getElementById("native_history_notice").hidden && !document.getElementById("integration_toggle").disabled && document.getElementById("integration_toggle").dataset.action==="enable";')
            assert browser.execute('return document.getElementById("native_history_retry").textContent;') == (
                '继续转换历史' if language == 'zh-CN' else 'Continue history conversion')
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            assert browser.execute('const button=document.getElementById("native_history_retry").getBoundingClientRect(), text=document.querySelector("#native_history_notice p");return button.left>=0 && button.right<=innerWidth && button.bottom<=innerHeight && text.scrollWidth<=text.clientWidth;')
            _, screenshot = request(browser.base + '/screenshot')
            args.output.with_name(args.output.stem + f'-{language}.png').write_bytes(base64.b64decode(screenshot['value']))
        checks.append('Chinese/English pending-history recovery is visible on narrow/desktop layouts')

        stop(process)
        process, base, url, headers = launch(stack, binary, root, env, 2)
        pending = request(base + '/api/integration', headers=headers)[1]
        assert pending['history_restore']['status'] == 'partial'
        assert busy[1].read_bytes() == busy[2]
        fcntl.flock(writer, fcntl.LOCK_UN)
        writer.close()
        browser.navigate(url)
        browser.execute('document.getElementById("native_history_retry").click();document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return integrationInfo.history_restore?.status==="complete" && !integrationBusy;'))
        assert browser.execute('return document.getElementById("native_history_retry").hidden;')
        converted(busy[1], busy[2])
        checks.append('Pending recovery survives restart and completes from the existing WebUI after the writer closes')

        fingerprints = [(item[1].stat().st_mtime_ns, hashlib.sha256(item[1].read_bytes()).hexdigest()) for item in [idle, busy]]
        assert post('/api/integration/restore')['history_restore']['report']['threads_repaired'] == 0
        assert fingerprints == [(item[1].stat().st_mtime_ns, hashlib.sha256(item[1].read_bytes()).hexdigest()) for item in [idle, busy]]
        repair_root = home / 'easy-multi-provider/integration/history-repair'
        originals = {item[0]: item[2] for item in [idle, busy]}
        backed_up = set()
        for manifest_path in repair_root.glob('repair-*/manifest.json'):
            manifest = json.loads(manifest_path.read_text())
            assert manifest['status'] == 'committed'
            for entry in manifest['entries']:
                backup = home / entry['backup']
                assert backup.read_bytes() == originals[entry['thread_id']]
                backed_up.add(entry['thread_id'])
        assert backed_up == set(originals)
        checks.append('Backups are byte-exact; native opaque state/tool pairs survive; a second restore makes no history writes')

        post('/api/integration/enable')
        on_quit = history(home, '3')
        applied = (home / 'config.toml').read_text()
        (home / 'config.toml').write_text(applied.replace(base, 'http://127.0.0.1:8'))
        current = {'X-EMP-Session': json.loads((root / 'state/web-session.json').read_text())['token']}
        assert request(base + '/api/integration/restore', {'confirm_reload': True}, current)[0] == 409
        assert on_quit[1].read_bytes() == on_quit[2]
        (home / 'config.toml').write_text(applied)
        post('/api/quit')
        assert process.wait(timeout=15) == 0
        converted(on_quit[1], on_quit[2])
        assert (home / 'config.toml').read_text() == original_config
        offline = history(home, '4')
        result = subprocess.run([str(binary), 'restore', '--json'], env=env, cwd=root,
                                capture_output=True, timeout=30, check=True)
        assert json.loads(result.stdout)['history_restore']['report']['threads_repaired'] == 1
        converted(offline[1], offline[2])
        checks.append('Normal Quit and offline Restore share idle-history conversion and preserve native configuration')
    with binary.open('rb') as stream:
        binary_sha256 = hashlib.file_digest(stream, 'sha256').hexdigest()
    args.output.write_text(json.dumps({'passed': True, 'binary_sha256': binary_sha256,
                                      'real_model_calls': 0, 'checks': checks}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': len(checks), 'output': str(args.output)}))


if __name__ == '__main__':
    main()
