"""Local requests and disposable backend lifecycle, without synthetic services."""
import json
import os
from pathlib import Path
import re
import signal
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit


def request(url, payload=None, headers=None, method=None, *, timeout=15):
    if urlsplit(url).hostname != '127.0.0.1':
        raise ValueError('Preview requests must target a local backend')
    req = urllib.request.Request(url, data=None if payload is None else json.dumps(payload).encode(),
                                 headers={'Content-Type': 'application/json', **(headers or {})}, method=method)
    try:
        # Inherited HTTP_PROXY must not send local WebDriver/backend traffic away.
        response = urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req, timeout=timeout)
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
    raise TimeoutError('Isolated backend did not become ready')


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def stop(process, process_group=False):
    if process.poll() is None:
        if process_group:
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            if process_group:
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            process.wait()


def start_backend(stack, binary, root, configuration, env_overrides=None, *, device_fixture=False):
    config = root / 'config.json'
    config.write_text(json.dumps(configuration))
    env = {key: value for key, value in os.environ.items() if key not in {
        'OPENAI_API_KEY', 'CODEX_API_KEY', 'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN',
        'ANTHROPIC_BASE_URL', 'EASY_MULTI_PROVIDER_CONFIG'}}
    for key in ['HOME', 'CODEX_HOME', 'XDG_CONFIG_HOME', 'APPDATA', 'LOCALAPPDATA']:
        env[key] = str(root / key.lower())
        Path(env[key]).mkdir()
    env.update(env_overrides or {})
    command = []
    if device_fixture:
        env.pop('NVM_DIR', None)
        if sys.platform == 'linux' and Path('/usr/lib/chatgpt/resources/codex').exists():
            bwrap = shutil.which('bwrap')
            if not bwrap:
                raise RuntimeError('Device-login fixture requires bwrap to hide the installed ChatGPT runtime')
            fixture_bin = env['PATH'].split(os.pathsep)[0]
            env['PATH'] = '/fixture:/usr/bin:/bin'
            command = [bwrap, '--ro-bind', '/usr', '/usr', '--ro-bind', '/bin', '/bin',
                       '--ro-bind', '/lib', '/lib', '--ro-bind', '/lib64', '/lib64',
                       '--ro-bind', '/etc', '/etc', '--ro-bind', '/home', '/home',
                       '--dev', '/dev', '--proc', '/proc', '--bind', '/tmp', '/tmp',
                       '--bind', str(root.parent), str(root.parent), '--ro-bind', fixture_bin,
                       '/fixture', '--tmpfs', '/usr/lib/chatgpt', '--']
    log_path = root / 'backend.log'
    log = stack.enter_context(log_path.open('w'))
    process = subprocess.Popen([*command, str(binary), 'serve', '--config', str(config), '--port', '0'],
                               env=env, cwd=root, stdout=log, stderr=log,
                               start_new_session=bool(command))
    stack.callback(stop, process, bool(command))
    url = wait_for(lambda: re.search(r'Open in browser: (http://[^\s]+)', log_path.read_text(encoding='utf-8')))[1]
    base = url.split('/?')[0]
    state = root / 'state'
    token = wait_for(lambda: json.loads((state / 'web-session.json').read_text(encoding='utf-8'))['token']
                     if (state / 'web-session.json').exists() else None)
    return base, url, state, {'X-EMP-Session': token}
