"""Linux native tray acceptance on a private D-Bus, with disposable EMP state."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import urllib.request
from unittest.mock import patch

from e2e_environment import free_port, request, stop, wait_for
from e2e_refinements import Browser


class Watcher:
    """Stand in for the desktop panel, never registering on the user's bus."""
    def __init__(self):
        from gi.repository import Gio, GLib
        self.Gio, self.GLib = Gio, GLib
        self.items = []
        self.connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        interface = Gio.DBusNodeInfo.new_for_xml('''<node>
          <interface name="org.kde.StatusNotifierWatcher">
            <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
            <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
            <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
            <property name="ProtocolVersion" type="i" access="read"/>
          </interface></node>''').interfaces[0]
        self.registration = self.connection.register_object('/StatusNotifierWatcher', interface,
            self.method, self.property, None)
        self.owner = Gio.bus_own_name_on_connection(self.connection, 'org.kde.StatusNotifierWatcher',
            Gio.BusNameOwnerFlags.NONE, None, None)
        self.loop = GLib.MainLoop()
        self.thread = threading.Thread(target=self.loop.run, daemon=True)
        self.thread.start()
        wait_for(lambda: self.call('org.freedesktop.DBus', '/org/freedesktop/DBus',
            'org.freedesktop.DBus', 'NameHasOwner', '(s)', ('org.kde.StatusNotifierWatcher',))[0])

    def method(self, connection, sender, path, interface, method, parameters, invocation):
        self.items.append(parameters.unpack()[0])
        invocation.return_value(self.GLib.Variant('()', ()))

    def property(self, connection, sender, path, interface, name):
        values = {'IsStatusNotifierHostRegistered': ('b', True),
                  'RegisteredStatusNotifierItems': ('as', self.items), 'ProtocolVersion': ('i', 0)}
        kind, value = values[name]
        return self.GLib.Variant(kind, value)

    def call(self, name, path, interface, method, signature, arguments):
        return self.connection.call_sync(name, path, interface, method,
            self.GLib.Variant(signature, arguments), None, self.Gio.DBusCallFlags.NONE, 3000, None).unpack()

    def get(self, item, property_name):
        return self.call(item, '/StatusNotifierItem', 'org.freedesktop.DBus.Properties', 'Get',
            '(ss)', ('org.kde.StatusNotifierItem', property_name))[0]

    def menu(self, item):
        layout = self.call(item, self.get(item, 'Menu'), 'com.canonical.dbusmenu',
            'GetLayout', '(iias)', (0, -1, []))[1]
        return [(node[0], node[1]) for node in layout[2]]

    def click(self, item, label):
        matches = [identifier for identifier, properties in self.menu(item) if properties.get('label') == label]
        assert len(matches) == 1, label
        self.call(item, self.get(item, 'Menu'), 'com.canonical.dbusmenu', 'Event',
            '(isvu)', (matches[0], 'clicked', self.GLib.Variant('s', ''), 0))

    def close(self):
        self.loop.quit()
        self.thread.join(timeout=3)
        self.Gio.bus_unown_name(self.owner)
        self.connection.unregister_object(self.registration)


def run_case(stack, binary, root, watcher, language, *, recovery=False, reset=False, detached=False):
    codex = root / 'codex'
    codex.mkdir()
    original = '# original user settings\nmodel = "fixture-native"\n'
    native = codex / 'config.toml'
    native.write_text(original)
    configuration = {'language': language, 'auto_enable_on_start': False,
        'providers': [{'id': 'demo', 'name': 'Demo', 'base_url': 'http://127.0.0.1:9/v1',
                       'protocol': 'chat_completions', 'api_key': 'fixture-only'}],
        'models': [{'id': 'demo/model', 'provider': 'demo', 'upstream_id': 'fixture', 'enabled': True}]}
    config = root / 'easy-multi-provider/config.json' if detached else root / 'config.json'
    config.parent.mkdir(exist_ok=True)
    config.write_text('{broken fixture' if recovery else json.dumps(configuration))
    opened = root / 'opened'
    pid_path = root / 'service-pid'
    browser = root / 'browser'
    browser.write_text('#!/bin/sh\nprintf "%s\\n" "$1" >> ' + shlex.quote(str(opened))
        + '\nprintf "%s" "$PPID" > ' + shlex.quote(str(pid_path)) + '\n')
    browser.chmod(0o700)
    env = {'PATH': '/usr/bin:/bin', 'HOME': str(root), 'XDG_CONFIG_HOME': str(root),
           'CODEX_HOME': str(codex), 'DBUS_SESSION_BUS_ADDRESS': os.environ['DBUS_SESSION_BUS_ADDRESS'],
           'WAYLAND_DISPLAY': 'emp-isolated-fixture', 'BROWSER': shlex.quote(str(browser)),
           'HTTP_PROXY': 'http://127.0.0.1:9', 'HTTPS_PROXY': 'http://127.0.0.1:9',
           'NO_PROXY': 'localhost,127.0.0.1'}
    command = [str(binary), 'serve', '--config', str(config), '--port', '0', '--open-browser']
    if detached:
        archive = root / 'archive'
        archive.mkdir()
        shutil.copy2(binary, archive / 'EMP')
        source = Path(__file__).resolve().parents[2]
        shutil.copy2(source / 'packaging/install-user.sh', archive / 'install-user.sh')
        shutil.copy2(source / 'assets/branding/easy-multi-provider-icon.svg', archive / 'easy-multi-provider.svg')
        env['XDG_DATA_HOME'] = str(root / 'data')
        subprocess.run(['/bin/sh', str(archive / 'install-user.sh')], env=env, check=True, capture_output=True)
        desktop = root / 'data/applications/easy-multi-provider-user.desktop'
        assert 'Terminal=false' in desktop.read_text()
        config.write_text(json.dumps({**configuration, 'port': free_port()}))
        command = ['/usr/bin/script', '--quiet', '--return', '--command',
                   shlex.quote(str(root / '.local/bin/EMP')), '/dev/null']
    log_path = root / 'backend.log'
    log = stack.enter_context(log_path.open('w'))
    count = len(watcher.items)
    process = subprocess.Popen(command, cwd=root, env=env, stdout=log, stderr=log)
    stack.callback(stop, process)
    pattern = r'Recovery page: (http://127.0.0.1:\d+)/#recovery=([^\s]+)' if recovery else r'EMP listening on (http://127.0.0.1:\d+)'
    pidfd = None
    if detached:
        service_pid = wait_for(lambda: pid_path.read_text().strip() if pid_path.exists() else None)
        pid = int(service_pid)
        assert Path(f'/proc/{pid}/cwd').resolve() == root
        assert os.getsid(pid) == pid, 'Desktop service has its own session after the launch terminal closes'
        pidfd = os.pidfd_open(pid)
        stack.callback(os.close, pidfd)
        def stop_detached():
            with contextlib.suppress(ProcessLookupError):
                signal.pidfd_send_signal(pidfd, signal.SIGTERM)
        stack.callback(stop_detached)
        assert process.wait(timeout=5) == 0, 'The desktop launcher returns without keeping a terminal alive'
        base = opened.read_text().splitlines()[0].split('/?')[0]
    else:
        match = wait_for(lambda: re.search(pattern, log_path.read_text()))
        base = match[1]
    item = wait_for(lambda: watcher.items[-1] if len(watcher.items) > count else None)
    wait_for(opened.exists)
    labels = ['打开 WebUI', '启动 EMP', '退出 EMP'] if language == 'zh-CN' else ['Open WebUI', 'Enable EMP', 'Quit EMP']
    restore = '恢复原生' if language == 'zh-CN' else 'Restore Native'
    assert [properties['label'] for _, properties in watcher.menu(item)] == labels
    assert watcher.get(item, 'ItemIsMenu') is True
    assert watcher.get(item, 'IconPixmap')[0][:2] == (48, 48)
    if recovery:
        assert watcher.menu(item)[1][1].get('enabled') is False
        watcher.click(item, labels[0])
        wait_for(lambda: len(opened.read_text().splitlines()) == 2)
        assert opened.read_text().splitlines()[0] == opened.read_text().splitlines()[1]
        if reset:
            registered = len(watcher.items)
            req = urllib.request.Request(base + '/api/recovery/reset', data=b'', method='POST', headers={
                'X-EMP-Recovery': match[2], 'X-EMP-Recovery-Confirm': 'clear-and-reset'})
            with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(req) as response:
                assert json.load(response)['restarting']
            item = wait_for(lambda: watcher.items[-1] if len(watcher.items) > registered else None)
            assert len(watcher.menu(item)) == 3
            watcher.click(item, labels[2])
        else:
            watcher.click(item, labels[2])
        assert process.wait(timeout=25) == 0, log_path.read_text()
        assert not reset or config.read_text().startswith('{')
        if not reset:
            assert config.read_text() == '{broken fixture'
        assert native.read_text() == original
        return
    initial_url = opened.read_text().splitlines()[0]
    initial_token = initial_url.split('bootstrap=')[1]
    status, _ = request(base + '/api/session', {}, {'X-EMP-Bootstrap': initial_token})
    assert status == 200
    watcher.click(item, labels[0])
    wait_for(lambda: len(opened.read_text().splitlines()) == 2)
    reopen_url = opened.read_text().splitlines()[-1]
    assert reopen_url != initial_url, 'Every tray open must issue a fresh single-use sign-in link'
    token = reopen_url.split('bootstrap=')[1]
    status, session = request(base + '/api/session', {}, {'X-EMP-Bootstrap': token})
    assert status == 200
    assert request(base + '/api/session', {}, {'X-EMP-Bootstrap': token})[0] == 401
    headers = {'X-EMP-Session': session['session']}
    browser = None
    if language == 'zh-CN' and not detached:
        watcher.click(item, labels[0])
        wait_for(lambda: len(opened.read_text().splitlines()) == 3)
        # Snap's browser launcher needs the existing user bus. EMP and its
        # synthetic panel remain exclusively on the private test bus.
        browser_bus = os.environ.get('EMP_TRAY_BROWSER_BUS_ADDRESS', '')
        with patch.dict(os.environ, {'DBUS_SESSION_BUS_ADDRESS': browser_bus}):
            browser = Browser(stack, root)
        browser.navigate(opened.read_text().splitlines()[-1])
        wait_for(lambda: browser.execute('return managementEvents.connected() && document.getElementById("integration").dataset.state === "native";'))
        browser.execute('window.trayReviewMarker="preserved";openSettings();')
    watcher.click(item, labels[1])
    wait_for(lambda: watcher.menu(item)[1][1]['label'] == restore)
    assert request(base + '/healthz')[0] == 200
    assert '/v1' in native.read_text()
    if browser:
        wait_for(lambda: browser.execute('return document.getElementById("integration").dataset.state === "emp_applied";'))
        assert browser.execute('return window.trayReviewMarker === "preserved" && !document.getElementById("modal_backdrop").classList.contains("hidden");')
        browser.execute('managementEvents.stop();')
    watcher.click(item, restore)
    wait_for(lambda: watcher.menu(item)[1][1]['label'] == labels[1])
    assert request(base + '/healthz')[0] == 200, 'Restore keeps the service and tray alive'
    assert 'fixture-native' in native.read_text() and base not in native.read_text()
    if browser:
        browser.execute('window.dispatchEvent(new Event("focus"));')
        wait_for(lambda: browser.execute('return document.getElementById("integration").dataset.state === "native";'))
    watcher.click(item, labels[1])
    wait_for(lambda: watcher.menu(item)[1][1]['label'] == restore)
    if browser:
        browser.execute('managementEvents.start();')
        wait_for(lambda: browser.execute('return managementEvents.connected() && document.getElementById("integration").dataset.state === "emp_applied";'))
        assert browser.execute('return window.trayReviewMarker === "preserved";')
    if language == 'zh-CN':
        watcher.click(item, labels[2])
    else:
        assert request(base + '/api/quit', {}, headers)[0] == 200
    assert process.wait(timeout=25) == 0, log_path.read_text()
    if pidfd is not None:
        poll = select.poll()
        poll.register(pidfd, select.POLLIN)
        assert poll.poll(25000), 'Quit must finish the detached service as well as the launcher'
    assert 'fixture-native' in native.read_text() and base not in native.read_text()
    assert not watcher.call('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
        'NameHasOwner', '(s)', (item,))[0], 'Exit removes its tray registration'


def main():
    if os.environ.get('EMP_TRAY_PRIVATE_BUS') != '1':
        env = dict(os.environ, EMP_TRAY_PRIVATE_BUS='1',
            EMP_TRAY_BROWSER_BUS_ADDRESS=os.environ.get('DBUS_SESSION_BUS_ADDRESS', ''))
        os.execvpe('dbus-run-session', ['dbus-run-session', '--', '/usr/bin/python3', *sys.argv], env)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-tray-') as directory, contextlib.ExitStack() as stack:
        watcher = Watcher()
        stack.callback(watcher.close)
        for language in ['zh-CN', 'en']:
            root = Path(directory) / language
            root.mkdir()
            run_case(stack, args.emp.resolve(), root, watcher, language)
            checks.append(language + ': three native menu items, menu-only activation, fresh single-use browser login, enable/restore, and clean exit')
            if language == 'zh-CN':
                checks.append('Actual WebUI follows tray changes over SSE, focus recovery and reconnect without reloading the document or closing an open settings dialog')
        desktop = Path(directory) / 'desktop user $100%'
        desktop.mkdir()
        run_case(stack, args.emp.resolve(), desktop, watcher, 'en', detached=True)
        checks.append('Installed Linux desktop: no terminal, service survives private launch-terminal closure, preserved configuration, native menu actions, and complete service exit')
        for reset in [False, True]:
            root = Path(directory) / ('recovery-reset' if reset else 'recovery-quit')
            root.mkdir()
            run_case(stack, args.emp.resolve(), root, watcher, 'zh-CN', recovery=True, reset=reset)
            checks.append('Recovery: retained tray, reopen recovery page, ' + ('reset to normal service and quit' if reset else 'quit without changing configuration'))
    args.output.write_text(json.dumps({'checks': checks}, indent=2) + '\n')
    print(json.dumps({'passed': len(checks), 'checks': checks}))


if __name__ == '__main__':
    main()
