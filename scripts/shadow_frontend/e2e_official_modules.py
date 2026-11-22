"""Install official module binaries and exercise three workspace roles in disposable EMPs."""
import argparse
import base64
import contextlib
from http.client import HTTPConnection
import hashlib
from http.server import BaseHTTPRequestHandler
import itertools
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from urllib.error import URLError
from urllib.parse import unquote
from urllib.request import urlopen

from candidate_web import candidate_frontend
from e2e import Browser
from e2e_request_modes import websocket_request
from local_backend import free_port, request, stop, wait_for
from module_release_fixture import Release, http_server

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.codex_feature_acceptance import fixture
from scripts.native_restore_acceptance.app_server import AppServer


KINDS = ('decision', 'gateway', 'aggregate', 'team')
NO_NETWORK = {
    'PATH': os.environ['PATH'],
    'HTTP_PROXY': 'http://127.0.0.1:9',
    'HTTPS_PROXY': 'http://127.0.0.1:9',
    'ALL_PROXY': 'http://127.0.0.1:9',
    'NO_PROXY': 'localhost,127.0.0.1',
}
LOCAL_KEY = 'fixture-local-key'
PROVIDER_KEY = 'fixture-team-upstream-secret'
PASSWORD = 'only-for-this-workspace-test'


class Upstream(fixture.Upstream):
    def do_POST(self):
        if self.path.endswith(('/alpha/search', '/images/generations', '/images/edits')):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.append({'path':self.path, 'body':body, 'headers':dict(self.headers)})
            payload = json.dumps({'data': [{'b64_json':'ZmFrZQ=='}]}).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(payload)))
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(payload)
            return
        if self.path.endswith(('/decisions', '/systemone')):
            data = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            if data.get('state') == 'hold-during-disable':
                self.server.decision_started.set()
                assert self.server.decision_release.wait(15)
            payload = json.dumps({'model': data['model'], 'answers': {'ok': True},
                                  'usage': {'input_tokens': 2, 'output_tokens': 1}}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        return super().do_POST()


def https_workspace(stack, root, upstream_port):
    def openssl(*args):
        subprocess.run(['openssl', *args], cwd=root, check=True, capture_output=True)

    openssl('req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=EMP fixture root',
            '-days', '2', '-keyout', 'workspace-ca.key', '-out', 'workspace-ca.pem')
    openssl('req', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=EMP workspace fixture',
            '-addext', 'subjectAltName=IP:127.0.0.1', '-keyout', 'workspace.key', '-out', 'workspace.csr')
    openssl('x509', '-req', '-in', 'workspace.csr', '-CA', 'workspace-ca.pem',
            '-CAkey', 'workspace-ca.key', '-CAcreateserial', '-copy_extensions', 'copy', '-days', '2', '-out', 'workspace.pem')
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(root/'workspace.pem', root/'workspace.key')

    overrides = {}

    class WorkspaceProxy(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def forward(self):
            if (override := overrides.get(self.path)) is not None:
                payload = json.dumps(override() if callable(override) else override).encode()
                self.send_response(200)
                self.send_header('Content-Type','application/json')
                self.send_header('Content-Length',str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                return
            with contextlib.closing(HTTPConnection('127.0.0.1', upstream_port, timeout=15)) as upstream:
                headers = {name:value for name,value in self.headers.items() if name.lower() in ('authorization','content-type')}
                payload = self.rfile.read(int(self.headers.get('Content-Length',0))) if self.command == 'POST' else None
                try:
                    upstream.request(self.command, self.path, body=payload, headers=headers)
                except ConnectionRefusedError:
                    self.send_error(502)
                    return
                reply = upstream.getresponse()
                body = reply.read()
                self.send_response(reply.status)
                self.send_header('Content-Type','application/json')
                self.send_header('Content-Length',str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        do_GET = do_POST = forward

    proxy = http_server(stack, WorkspaceProxy, tls)
    return f'https://127.0.0.1:{proxy.server_port}', root/'workspace-ca.pem', overrides


class Emp:
    def __init__(self, stack, binary, root, config, release, ca=None, *, guide_source=None):
        self.stack, self.binary, self.root = stack, binary, root
        root.mkdir()
        self.config = root / 'config.json'
        self.config.write_text(json.dumps({'auto_enable_on_start': False, **config}))
        self.home = root / 'codex'
        self.home.mkdir()
        self.auth = self.home / 'auth.json'
        self.auth.write_text(json.dumps({'auth_mode': 'apikey', 'OPENAI_API_KEY': LOCAL_KEY}))
        helpers = root / 'helpers'
        helpers.mkdir()
        quota_helper = helpers / 'codex'
        shutil.copyfile(Path(__file__).with_name('fixtures') / 'device-codex.py', quota_helper)
        quota_helper.chmod(0o700)
        self.env = {**NO_NETWORK, 'HOME': str(root), 'CODEX_HOME': str(self.home),
                    'PATH': str(helpers) + os.pathsep + os.defpath,
                    'XDG_CONFIG_HOME': str(root / 'xdg'), 'APPDATA': str(root / 'appdata'),
                    'LOCALAPPDATA': str(root / 'localappdata'),
                    'EMP_UPDATE_TEST_REPOSITORY_URL': release,
                    'EMP_UPDATE_TEST_API_URL': release + '/releases?per_page=100'}
        if ca:
            self.env['SSL_CERT_FILE'] = str(ca)
        if guide_source:
            self.env['EMP_GUIDE_TEST_SOURCE'] = guide_source
        if os.environ.get('EMP_E2E_DESKTOP_NOTICE_LOG'):
            self.env['DBUS_SESSION_BUS_ADDRESS'] = os.environ['DBUS_SESSION_BUS_ADDRESS']
        self.restarts = 0
        self.start()

    def start(self):
        self.restarts += 1
        path = self.root / f'emp-{self.restarts}.log'
        log = self.stack.enter_context(path.open('w'))
        self.process = subprocess.Popen([str(self.binary), 'serve', '--config', str(self.config), '--port', '0'],
                                        env=self.env, cwd=self.root, stdout=log, stderr=log)
        self.stack.callback(stop, self.process)

        def ready():
            output = path.read_text()
            if self.process.poll() is not None or 'EMP startup needs attention' in output:
                raise RuntimeError(re.sub(r'(bootstrap|recovery)=[^&\s]+', r'\1=[fixture]', output[-1500:]))
            return re.search(r'Open in browser: (http://[^\s]+)', output)

        try:
            self.url = wait_for(ready,45)[1]
        except TimeoutError:
            raise RuntimeError(f"{self.root.name} startup timed out: " +
                               re.sub(r'(bootstrap|recovery)=[^&\s]+',r'\1=[fixture]',path.read_text()[-2000:])) from None
        self.base = self.url.split('/?')[0]

    def restart(self):
        stop(self.process)
        self.start()

    def api(self, path, body=None, expected=200):
        session = json.loads((self.root / 'state/web-session.json').read_text())['token']
        status, result = request(self.base + path, body, {'X-EMP-Session':session}, timeout=45)
        assert status == expected, (path, status, result)
        return result

    def install(self, kind, expected=200):
        return self.api('/api/modules', {'id': kind, 'action': 'install'}, expected)

    def team(self, operation, body=None, expected=200):
        return self.api('/api/team/' + operation, {} if body is None else body, expected)

    def login(self, server, name):
        identity = self.team('login', {'base': server, 'name': name, 'password': PASSWORD})
        assert identity['identity']['member']['name'] == name and 'token' not in str(identity)
        return identity['identity']['member']


def start_vps(stack, binary, root, port):
    env = {**NO_NETWORK, 'EMP_TEAM_ADMIN': 'deployment', 'EMP_TEAM_ADMIN_PASSWORD': PASSWORD,
           'EMP_TEAM_SPACE_PASSWORD': 'fixture-space-encryption-password',
           'EMP_TEAM_NAME': 'Lab'}
    proc = subprocess.Popen([str(binary), 'serve', '--state', str(root),
                             '--listen', f'127.0.0.1:{port}'], env=env)
    stack.callback(stop, proc)
    base = f'http://127.0.0.1:{port}'

    def ready():
        if proc.poll() is not None:
            raise RuntimeError(f'Workspace service exited with {proc.returncode}')
        try:
            return request(base + '/healthz')[1].get('mode') == 'workspace'
        except (URLError, OSError):
            return False

    wait_for(ready)
    return proc, base


def bundled_transition(stack, root, old, current, release, url, suff):
    release.delay_download = True
    release.delayed_requests = 0
    elapsed = []
    for corrupt in [False, True]:
        folder = root / ('bundled-rollback' if corrupt else 'bundled-upgrade')
        folder.mkdir()
        installed = folder/'EMP'
        shutil.copy2(old, installed)
        expected = hashlib.sha256(old.read_bytes()).digest()
        listener = free_port()
        instance = Emp(stack, installed, folder/'profile', {}, url)
        stop(instance.process)
        job = folder/'.emp-update-fixture'
        job.mkdir()
        candidate = job/'candidate'
        shutil.copy2(current, candidate)
        if corrupt:
            payload = (current.parent/f'EMP-mod-gateway-{suff}').read_bytes()
            executable = candidate.read_bytes()
            offset = executable.find(payload)
            assert offset >= 0, 'packaged Core lacks the transitional Gateway payload'
            with candidate.open('r+b') as file:
                file.seek(offset + 32)
                file.write(bytes([payload[32] ^ 1]))
        manifest = {'target':str(installed), 'candidate':str(candidate),'relative_binary':'',
                    'parents':[],'args':['serve','--config',str(instance.config),'--port',str(listener)],
                    'version':release.version,'nonce':'fixture-bundled-update'}
        plan = job/'plan.json'
        plan.write_text(json.dumps(manifest))
        worker = job/'worker'
        shutil.copy2(old, worker)
        paths = instance.root/'state/modules/modules.json'
        assert not paths.exists()
        if not corrupt:
            paths.parent.mkdir(exist_ok=True)
            paths.write_text(json.dumps({module:{'enabled':True,'versions':{'0.14.2':'0'*64}}
                                         for module in ('gateway','decision','aggregate')}))
        try:
            start = time.monotonic()
            outcome = subprocess.run([str(worker),'--emp-apply-update',str(plan)],env=instance.env,
                                     cwd=instance.root,timeout=95,stdout=subprocess.DEVNULL)
            elapsed.append(round(time.monotonic() - start, 2))
            assert outcome.returncode == int(corrupt), outcome.returncode
            assert elapsed[-1] < 60 and release.delayed_requests == 0, elapsed
            instance.base = f'http://127.0.0.1:{listener}'
            def healthy():
                try:
                    return request(instance.base+'/healthz',timeout=1)[0] == 200
                except OSError:
                    return False
            wait_for(healthy)
            if corrupt:
                assert hashlib.sha256(installed.read_bytes()).digest() == expected
                assert not paths.exists()
            else:
                rows = instance.api('/api/modules')['modules']
                assert {row['id'] for row in rows if row['active']} == {'gateway','decision','aggregate'}
                assert all(row['installed_version'] == row['version'] != release.version
                           for row in rows if row['active'])
        finally:
            release.corrupt = None
            try:
                instance.api('/api/quit',{})
            except (OSError, AssertionError):
                pass
    release.delay_download = False
    return elapsed



def codex_session(stack, codex, emp, log):
    version = subprocess.check_output([str(codex), '--version'], env=emp.env, text=True).strip().split()[-1]
    catalog = emp.api('/v1/models?client_version=' + version)
    model = next((item['slug'] for item in catalog['models']
                  if unquote(item['slug']).startswith('lab-admin/')), None)
    assert model, [(item['slug'], item['display_name']) for item in catalog['models']]
    model_path = emp.home / 'models.json'
    model_path.write_text(json.dumps(catalog))
    (emp.home / 'config.toml').write_text(
        f'model={json.dumps(model)}\nmodel_provider="fixture"\n'
        f'model_catalog_json={json.dumps(str(model_path))}\n'
        'approval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
        '[features]\nplugins=false\napps=false\nmemories=false\n'
        '[model_providers.fixture]\nname="EMP fixture"\nwire_api="responses"\n'
        f'base_url={json.dumps(emp.base + "/v1")}\nenv_key="EMP_FIXTURE_AUTH"\n'
        'supports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n'
    )
    client = AppServer(codex, {**emp.env, 'EMP_FIXTURE_AUTH': LOCAL_KEY}, log)
    stack.callback(client.close)
    picker = client.rpc('model/list', {'limit': 100})
    assert any(item['model'] == model for item in picker['data']), [item['model'] for item in picker['data']]
    started = client.rpc('thread/start', {'model': model, 'cwd': str(emp.root)})
    assert started['model'] == model
    return version, client, started['thread']['id']


def browser_roles(browser, output, emps):
    for emp, role, expected in emps:
        browser.navigate(emp.url)
        wait_for(lambda: browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
        browser.execute('document.getElementById("team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("team_home");'))
        cards = browser.execute('return [...document.querySelectorAll("[data-team-choice]")].map(button=>button.dataset.teamChoice);')
        assert cards == expected, (role, cards)
        wait_for(lambda: browser.execute('return document.querySelector("[data-team-created]")?.textContent !== "—" && document.querySelector("[data-team-joined]")?.textContent !== "—";'))
        if role == 'member':
            wait_for(lambda: browser.execute('return document.querySelector("[data-team-today]")?.textContent.includes(" / ");'))
        else:
            wait_for(lambda: browser.execute('return Number(document.querySelector("[data-team-member-count]")?.textContent) > 0;'))
            assert browser.execute('return !document.querySelector("[data-team-today]");')
        assert browser.execute('return document.querySelector(".team-meta dd").textContent;') == 'Lab'
        _, screenshot = request(browser.base + '/screenshot')
        (output / (role + '.png')).write_bytes(base64.b64decode(screenshot['value']))
        if role == 'member':
            assert browser.execute('return document.getElementById("shadow_migration_open").hidden;')
            browser.execute('document.querySelector("[data-team-subscriptions]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-workspace-subscription]");'))
            assert browser.execute('return !document.querySelector("[data-workspace-subscription] button");')
        else:
            assert browser.execute('return !!document.querySelector("[data-team-backup]") === arguments[0];', role == 'vps-admin')
            assert browser.execute('return document.getElementById("shadow_migration_open").nextElementSibling.id==="team_workspace_open" && !document.getElementById("shadow_migration_open").hidden;')
            browser.execute('document.querySelector("[data-team-choice=manage]").click();')
            browser.execute('document.querySelector("[data-team-manage-subscriptions]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-review-service]");'))
            open_member_management(browser)
            _, screenshot = request(browser.base + '/screenshot')
            (output / (role + '-management.png')).write_bytes(base64.b64decode(screenshot['value']))


def open_member_management(browser):
    wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-manage-members]");'))
    browser.execute('document.querySelector("[data-team-manage-members]").click();')
    wait_for(lambda: browser.execute('return !!document.querySelector("[data-member-add]");'))


def verify_workspace_backup(stack, root, binary, vps, admin, browser, downloads):
    admin.team('backup',expected=403)
    if browser:
        browser.navigate(vps.url)
        wait_for(lambda: browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
        browser.execute('document.getElementById("team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-backup]");'))
        browser.execute('document.querySelector("[data-team-backup]").click();')
        def downloaded():
            return next((path for path in downloads.glob('*.empteam') if path.stat().st_size),None) if not list(downloads.glob('*.part')) else None
        file = wait_for(downloaded)
    else:
        file = root/'workspace.empteam'
        file.write_text(json.dumps(vps.team('backup')))
    text = file.read_text()
    backup = json.loads(text)
    assert backup['format']=='emp-team-backup-v1' and 'fixture.' not in text
    assert 'researcher' not in text and 'older-image-model' not in text
    restored = root/'restored-vps'
    command = [str(binary),'restore','--state',str(restored),'--from',str(file)]
    env = {**NO_NETWORK,'EMP_TEAM_SPACE_PASSWORD':'fixture-space-encryption-password'}
    wrong = subprocess.run(command,env={**env,'EMP_TEAM_SPACE_PASSWORD':'wrong-fixture-password'},
                           capture_output=True,timeout=30)
    assert wrong.returncode and not (restored/'workspace.sqlite3').exists()
    subprocess.run(command,env=env,check=True,timeout=30,capture_output=True)
    with sqlite3.connect(restored/'workspace.sqlite3') as db:
        assert db.execute('SELECT COUNT(*) FROM sessions').fetchone()==(0,)
        assert db.execute('SELECT COUNT(*) FROM join_invites').fetchone()==(0,)
    again = subprocess.run(command,env=env,capture_output=True,timeout=30)
    assert again.returncode and b'workspace_restore_destination_exists' in again.stderr
    _, address = start_vps(stack,binary,restored,free_port())
    status, login = request(address+'/workspace/v1/login',
        {'name':'deployment','password':PASSWORD,'device':'a'*32})
    assert status==200 and login['workspace']['id']==vps.team('status')['identity']['workspace']['id']
    headers = {'Authorization':'Bearer '+login['token']}
    assert request(address+'/workspace/v1/overview',headers=headers)[1] == vps.team('overview')
    original = vps.team('subscriptions')['subscriptions']
    assert request(address+'/workspace/v1/subscriptions',headers=headers)[1]['subscriptions'] == original
    assert request(address+'/workspace/v1/audit',{},headers)[1] == vps.team('audit')


def verify_vps_signout(root, vps, workspace, vps_url, member_name, browser):
    other_device = 'e'*32
    status, other = request(vps_url+'/workspace/v1/login',
        {'name':'deployment','password':PASSWORD,'device':other_device})
    assert status == 200
    bearer = {'Authorization':'Bearer '+other['token']}
    device = (vps.root/'state/team-device-id').read_text()
    with sqlite3.connect(root/'vps/workspace.sqlite3') as database:
        current = database.execute(
            'SELECT token FROM sessions WHERE member=? AND device=?',
            (other['member']['id'],device)
        ).fetchone()
        assert current
    if browser is None:
        assert not vps.team('logout')['joined']
        assert vps.login(workspace,member_name)['role'] == 'member'
    else:
        browser.navigate(vps.url)
        wait_for(lambda: browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
        browser.execute('document.getElementById("team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-leave]");'))
        browser.execute('document.querySelector("[data-team-leave]").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("team_leave_confirm");'))
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("team_login") && !document.getElementById("team_join_code");'))
        assert browser.execute('return document.getElementById("team_base").value;') == workspace
        browser.execute('document.getElementById("team_name").value=arguments[0]; '
            'document.getElementById("team_password").value=arguments[1]; '
            'document.getElementById("modal_submit").click();',member_name,PASSWORD)
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_home .team-role-member");'))
        assert vps.team('status')['identity']['member']['name'] == member_name
    with sqlite3.connect(root/'vps/workspace.sqlite3') as database:
        assert not database.execute('SELECT 1 FROM sessions WHERE token=?',current).fetchone()
    status, still_signed_in = request(vps_url+'/workspace/v1/status',headers=bearer)
    assert status == 200 and still_signed_in['member']['role'] == 'vps_admin'
    assert request(vps_url+'/workspace/v1/logout',{},bearer)[0] == 200
    assert request(vps_url+'/workspace/v1/status',headers=bearer)[0] == 401
    assert not vps.team('logout')['joined']
    assert vps.login(workspace,'deployment')['role'] == 'vps_admin'


def subscription_token(account, plan, expiry):
    claims = base64.urlsafe_b64encode(json.dumps({'exp':expiry,
        'https://api.openai.com/auth':{'chatgpt_account_id':account,'chatgpt_plan_type':plan}}).encode()).decode().rstrip('=')
    return 'fixture.' + claims + '.signature'


def ten_member_demo(admin, vps, member, vps_url, browser, output, keep_available=False):
    lab = [('mei','pro'),('lin','pro'),('ning','prolite'),('qiao','free'),
           ('lan','free'),('yu','free'),('an','free'),('wen','free')]
    accounts = {name:vps.team('members',{'name':name,'role':'member','password':PASSWORD,
                'publish':plan!='free','sync':True})['id'] for name,plan in lab}
    peer = {'id':accounts['lin'],'publish':True,'sync':True,'enabled':True}
    vps.team('member_access',{**peer,'role':'team_admin'})
    admin.team('budget',{'member':accounts['lin'],'daily_usd_nanos':500},403)
    assert next(row for row in vps.team('usage')['rows'] if row['member']==accounts['lin'])['budget_nanos'] is None
    vps.team('member_access',{**peer,'role':'member'})
    expired = None
    for name,plan in lab[:3]:
        login = request(vps_url+'/workspace/v1/login', {'name':name,'password':PASSWORD,'device':'1'*32})[1]
        headers = {'Authorization':'Bearer '+login['token']}
        expiry = int(time.time())+(86400 if keep_available else 25 if plan=='prolite' else 3600)
        auth = {'access_token':subscription_token(name,plan,expiry),'account_id':name,
                'expires_at':expiry,'plan':plan}
        upload = {'source':'subscription:@native','expected_version':0,'payload':{'models':[],
            'provider':{'id':'subscription:@native','name':'Native','auth_mode':'codex_subscription'},
            'subscription':auth}}
        status, published = request(vps_url+'/workspace/v1/publish',upload,headers)
        assert status==200 and published['review_required'] is True, published
        assert request(vps_url+'/workspace/v1/publish',upload,headers)[1]==published
        vps.team('review_service',{**published,'shared':True})
        if plan=='prolite':
            expired = (published['id'],headers,upload)
    subscriptions = member.team('subscriptions')['subscriptions']
    assert sorted(row['plan'] for row in subscriptions)==['pro','pro','pro','prolite']
    assert all(row['status']=='available' for row in subscriptions)
    assert 'fixture.' not in json.dumps(subscriptions)
    member.team('sync')
    expected_accounts = {'team-'+row['share'] for row in subscriptions}
    assert {row['id'] for row in member.api('/api/config')['accounts'] if row.get('team_shared')}==expected_accounts
    member.team('subscription_reminder',{'share':expired[0]},409)
    assert member.team('profiles')['members'][0]['name']=='研究员一号'
    assert len(member.team('profiles')['members'])==1
    own = member.team('profile',{'member':member.team('profiles')['members'][0]['member']})
    assert own['lifetime']['actual_usd_nanos']>=2_000_000_000 and own['activity']
    member.team('profile',{'member':accounts['mei']},403)
    member.team('profile_visibility',{'visible':True},403)
    profiles = admin.team('profiles')
    assert len(profiles['members'])==10 and profiles['created_at']
    assert all(row['joined_at'] for row in profiles['members'])
    offline = admin.team('profile',{'member':accounts['qiao']})
    assert not offline['activity'] and offline['lifetime']['input_tokens'] is None
    admin.team('profile_visibility',{'visible':True})
    assert len(member.team('profiles')['members'])==10
    assert member.team('profile',{'member':accounts['mei']})['activity']
    joined = request(vps_url+'/workspace/v1/login',{'name':'qiao','password':PASSWORD,'device':'2'*32})[1]
    assert joined['member']['id']==accounts['qiao']
    assert admin.team('profile',{'member':accounts['qiao']})['activity']
    if browser:
        browser.navigate(member.url)
        wait_for(lambda: browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
        browser.execute('document.getElementById("team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-profiles]");'))
        browser.execute('document.querySelector("[data-team-profiles]").click();')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-team-profile-member]").length===10;'))
        _, screenshot = request(browser.base+'/screenshot')
        (output/'ten-member-directory.png').write_bytes(base64.b64decode(screenshot['value']))
        browser.execute('Array.from(document.querySelectorAll("[data-team-profile-member]")).find(node=>node.dataset.teamProfileMember===arguments[0]).click();',accounts['qiao'])
        wait_for(lambda: browser.execute('return document.querySelectorAll(".team-token-activity>div").length===30;'))
        assert browser.execute('return document.querySelectorAll(".team-token-activity>div.seen").length===1;')
        _, screenshot = request(browser.base+'/screenshot')
        (output/'member-profile.png').write_bytes(base64.b64decode(screenshot['value']))
        browser.execute('document.getElementById("modal_back").click();document.getElementById("modal_back").click();document.querySelector("[data-team-subscriptions]").click();')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-workspace-subscription]").length===4;'))
        _, screenshot = request(browser.base+'/screenshot')
        (output/'four-shared-subscriptions.png').write_bytes(base64.b64decode(screenshot['value']))
        if not keep_available:
            wait_for(lambda: any(row['share']==expired[0] and row['status']=='expired'
                             for row in member.team('subscriptions')['subscriptions']),seconds=40)
            browser.execute('document.getElementById("modal_back").click();document.querySelector("[data-team-subscriptions]").click();')
            wait_for(lambda: browser.execute('return document.querySelectorAll(".team-subscription-status[data-valid=false]").length===1;'))
            assert browser.execute('return !document.querySelector("#team_subscriptions button:not(.shadow-help)");')
            browser.navigate(admin.url)
            wait_for(lambda: browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            browser.execute('document.querySelector("[data-team-choice=manage]").click();document.querySelector("[data-team-manage-subscriptions]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector(".team-subscription-dot[data-valid=true]") && !!document.querySelector(".team-subscription-dot[data-valid=false]");'))
            assert browser.execute('return !document.querySelector("[data-team-subscriptions]");')
            _, screenshot = request(browser.base+'/screenshot')
            (output/'subscription-management.png').write_bytes(base64.b64decode(screenshot['value']))
    admin.team('profile_visibility',{'visible':False})
    member.team('profile',{'member':accounts['mei']},403)
    if keep_available:
        return
    wait_for(lambda: any(row['share']==expired[0] and row['status']=='expired'
                         for row in member.team('subscriptions')['subscriptions']),seconds=40)
    member.team('subscription_reminder',{'share':expired[0]})
    rows = request(vps_url+'/workspace/v1/subscriptions',headers=expired[1])[1]['subscriptions']
    assert any(row['share']==expired[0] and row['reminded_at'] for row in rows)
    new_expiry = int(time.time())+3600
    update = {**expired[2],'expected_version':1,'payload':{**expired[2]['payload'],
        'subscription':{'account_id':'ning','plan':'prolite','expires_at':new_expiry,
                        'access_token':subscription_token('ning','prolite',new_expiry)}}}
    status, renewed = request(vps_url+'/workspace/v1/publish',update,expired[1])
    assert status==200 and renewed=={'id':expired[0],'version':2,'review_required':False}
    assert any(row['share']==expired[0] and row['status']=='available' for row in member.team('subscriptions')['subscriptions'])
    assert member.team('sync')['synced']==1
    roster = member.api('/api/config')['accounts']
    assert len([row for row in roster if row['id']=='team-'+expired[0]])==1
    short_expiry = new_expiry-1800
    update['expected_version'] = 2
    update['payload']['subscription'].update(expires_at=short_expiry,access_token=subscription_token('ning','prolite',short_expiry))
    assert request(vps_url+'/workspace/v1/publish',update,expired[1])[0]==409


def verify_team_usage(stack, emppy, root, vps, workspace, release, mock, ca, upstream, output, browser_enabled):
    release_url = f'http://127.0.0.1:{release.server_port}'
    upstream.catalog = {'models':[{'slug':'gpt-6.2-sol','display_name':'GPT-6.2 Sol','visibility':'list',
        'context_window':272000,'max_context_window':272000,'supported_in_api':True,'shell_type':'shell_command'}]}
    vps.install('team')
    vps.login(workspace, 'deployment')
    admin = Emp(stack, emppy, root/'team-usage-admin', {'codex_base_url':mock}, release_url, ca)
    member = Emp(stack, emppy, root/'team-usage-member', {'codex_base_url':mock}, release_url, ca)
    for emp, name, role in [(admin, 'owner', 'team_admin'), (member, 'member', 'member')]:
        vps.team('members', {'name':name, 'role':role, 'password':PASSWORD, 'publish':name=='owner', 'sync':True})
        emp.install('team')
    def auth(expiry):
        return {'auth_mode':'chatgpt','tokens':{'account_id':'shared-account',
            'access_token':subscription_token('shared-account','pro',expiry),
            'refresh_token':'synthetic-refresh-token','id_token':'synthetic-id-token'}}
    expiry = int(time.time()) + 3600
    account = member.api('/api/accounts/import', {'id':'personal-copy','prefix':'personal-copy',
        'name':'Personal copy','auth_json':auth(expiry)})['account']
    saved = next(row['auth_file'] for row in json.loads(member.config.read_text())['accounts']
                 if row['id']==account['id'])
    private_auth = Path(saved).read_bytes()
    admin.auth.write_text(json.dumps(auth(expiry+60)))
    admin.login(workspace, 'owner')
    published = admin.team('publish')
    review = {'id':published['id'],'version':published['version'],'shared':True}
    admin.team('review_service', review)
    rates = {key + suffix: value for suffix in ('', '_priority', '_ultrafast')
             for key, value in [('input_cost_per_token', 0.000002), ('output_cost_per_token', 0.00001)]}
    prices = json.dumps({'rate_format':1,'fetched_at':time.time(), 'prices':{'gpt-6.2-sol':rates}})
    (member.root/'state/api_prices.json').write_text(prices)
    stop(member.process)
    config = json.loads(member.config.read_text())
    native_catalog = member.home/'models_cache.json'
    native_catalog.write_text(json.dumps(upstream.catalog))
    config['native_catalog_path'] = str(native_catalog)
    personal = next(row for row in config['accounts'] if row['id'] == account['id'])
    personal.update(credential_status='invalid', hidden_models=['example-hidden-model'],
                    model_context_windows={'gpt-6.2-sol':192000})
    member.config.write_text(json.dumps(config))
    member.start()
    identity = member.login(workspace, 'member')
    assert member.team('sync')['synced'] == 1
    assert member.team('sync')['synced'] == 0
    local_account = next(row for row in member.api('/api/accounts')['accounts'] if row['id'] == account['id'])
    assert local_account['credential_status'] != 'invalid', local_account
    assert (local_account['name'],local_account['prefix'],local_account['hidden_models'],
            local_account['model_context_windows']) == ('Personal copy','personal-copy',
                ['example-hidden-model'],{'gpt-6.2-sol':192000})
    member.api('/api/config',member.api('/api/config'))
    persisted = next(row for row in json.loads(member.config.read_text())['accounts']
                     if row['id'] == account['id'])
    assert persisted['credential_status'] == 'invalid'
    shared_auth = auth(expiry+180)
    admin.auth.write_text(json.dumps(shared_auth))
    published = admin.team('publish')
    assert published['version'] == review['version']+1 and not published['review_required']
    review['version'] = published['version']
    assert member.team('sync')['synced'] == 1
    models = member.api('/v1/models?client_version=0.169.0')['models']
    model = next((row for row in models if unquote(row['slug']) == 'Personal copy/gpt-6.2-sol'), None)
    assert model and model['context_window'] == 192000, [row['slug'] for row in models]
    vps.team('budget', {'member':identity['id'],'daily_usd_nanos':90_000})
    initialize = {'action':'initialize','timezone':'America/Los_Angeles'}
    assert member.team('overview')['weekly_budget'] is None
    assert member.team('overview')['budget_mode'] == 'daily_usd'
    member.team('weekly_budget', initialize, 403)
    expected_reset = {'weekday':0,'hour':16,'timezone':'America/Los_Angeles'}
    browser = Browser(stack, root) if browser_enabled else None
    if browser:
        member.api('/api/accounts/personal-copy/models/refresh', {})
        browser.navigate(member.url)
        editor = '[data-service="account:personal-copy"] [data-ui-action="account-edit"]'
        wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);', editor))
        browser.execute('document.querySelector(arguments[0]).click();', editor)
        wait_for(lambda: browser.execute('''
            const input = document.querySelector('#subscription_model_list [data-model="gpt-6.2-sol"]');
            return input?.value === '192000' && !document.querySelector('#subscription_model_list [data-preference-kind]');
        '''))
        asset = next(name for name in release.assets if name.startswith('EMP-mod-team-'))
        team_row = next(row for row in admin.api('/api/modules')['modules'] if row['id']=='team')
        host_api = subprocess.run([emppy.with_name('empmod-team')], stdin=subprocess.DEVNULL, check=True,
                                  capture_output=True, text=True)
        host_api = json.loads(host_api.stdout)['host_api']
        tag = f'emp-mod-team-api{host_api}-v0.99.0-beta'
        unrelated = f'emp-mod-team-api{host_api + 1}-v1.0.0'
        release.module_releases = dict.fromkeys((tag, unrelated), {asset:release.assets[asset]})
        release.release_notes = {tag:'### 中文\n更新夹具说明 <b>纯文本</b>\n### English\nFixture release notes <b>plain text</b>'}
        release.package_requests = []
        release.paginate_modules = True
        browser.navigate(admin.url)
        wait_for(lambda: browser.execute('return document.querySelector("#emp_logo").closest("h1").ariaLabel === "EMPT";'))
        wait_for(lambda: browser.execute('return document.querySelector("#modules_open").classList.contains("has-update");'))
        browser.execute('document.querySelector("#modules_open").click();')
        update_button = '[data-module-id="team"][data-module-action="check_updates"]'
        wait_for(lambda: browser.execute('return document.querySelector(arguments[0])?.classList.contains("has-update");', update_button))
        assert browser.execute('return [...document.querySelectorAll(".module-actions")].every(row => row.children.length === 4 && [...row.children].every(button => button.querySelector("svg") && button.ariaLabel && !button.textContent.trim()));')
        (output/'module-update.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector(arguments[0]).click();', update_button)
        wait_for(lambda: browser.execute('return document.querySelector("#module_release_notes")?.textContent.includes("<b>");'))
        assert browser.execute('return !document.querySelector("#module_release_notes b") && document.querySelector("#modal_body").textContent.includes("0.99.0-beta");')
        (output/'module-update-notes.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        assert release.package_requests == []
        browser.execute('document.querySelector("#modal_back").click();')
        wait_for(lambda: browser.execute('return document.querySelector(arguments[0])?.classList.contains("has-update");', update_button))
        assert release.package_requests == []
        browser.execute('document.querySelector(arguments[0]).click();', update_button)
        release.module_releases = {unrelated:{asset:release.assets[asset]}}
        browser.execute('document.querySelector("#modal_submit").click();')
        wait_for(lambda: browser.execute('return document.querySelector("#modal_status").textContent.length > 0;'))
        assert release.package_requests == []
        assert next(row for row in admin.api('/api/modules')['modules'] if row['id']=='team') == team_row
        browser.execute('document.querySelector("#modal_back").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);', update_button))
        browser.execute('document.querySelector(arguments[0]).click();', update_button)
        wait_for(lambda: browser.execute('return document.querySelector("#modal_body").children.length === 2 && !document.querySelector("#module_release_notes");'))
        assert browser.execute('return !document.querySelector("#modules_open").classList.contains("has-update") && document.querySelector("#modal_submit").style.display === "none";')
        browser.execute('document.querySelector("#modal_back").click();')
        assert admin.api('/api/team/status')['joined']
        browser.execute('document.querySelector("#modal_close").click();')
        browser.execute('document.querySelector("#team_workspace_open").click();')
        wait_for(lambda: admin.team('list_members')['weekly_reset'])
        expected_reset['timezone'] = browser.execute('return Intl.DateTimeFormat().resolvedOptions().timeZone;')
        assert admin.team('list_members')['weekly_reset'] == expected_reset
    else:
        assert admin.team('weekly_budget', initialize)['weekly_reset'] == expected_reset
    assert vps.team('weekly_budget', {**initialize,'timezone':'Asia/Tokyo'})['weekly_reset'] == expected_reset
    admin.team('weekly_budget', {'action':'reset','weekday':5,'hour':24}, 400)
    weekly_limit = {'action':'member_limit','member':identity['id'],'usd_nanos':55_000}
    member.team('weekly_budget', weekly_limit, 403)
    admin.team('weekly_budget', {**weekly_limit,'usd_nanos':-1}, 400)
    admin.team('weekly_budget', weekly_limit)
    weekly_mode = {'action':'mode','mode':'weekly_usd'}
    member.team('weekly_budget', weekly_mode, 403)
    assert admin.team('weekly_budget', weekly_mode)['budget_mode'] == 'weekly_usd'
    vps.team('member_tiers', {'id':identity['id'],'fast':False,'ultrafast':False})
    member.team('status')
    local = {'Authorization':'Bearer '+LOCAL_KEY}
    body = {'model':account['prefix']+'/gpt-6.2-sol','input':'Synthetic team usage ' * 8}
    calls = len(upstream.requests)
    for tier, label in [('priority','Fast'), ('ultrafast','Ultrafast')]:
        status, error = request(member.base+'/v1/responses', {**body,'service_tier':tier}, local)
        assert status == 403 and f'{label} is not permitted' in json.dumps(error), (status, error)
    assert len(upstream.requests) == calls
    websocket_request(member.base, local, body)
    assert upstream.requests[-1]['body']['model'] == 'gpt-6.2-sol'
    headers = {key.lower(): value for key, value in upstream.requests[-1]['headers'].items()}
    assert headers['authorization'] == 'Bearer ' + shared_auth['tokens']['access_token']
    assert Path(saved).read_bytes() == private_auth
    def usage():
        row = member.team('usage', {'period':'lifetime'})['rows'][0]
        return row if row['requests'] else None
    row = wait_for(usage)
    assert (row['requests'],row['input_tokens'],row['output_tokens'],row['estimated_usd_nanos']) == (1,10,3,50_000), row
    assert row['models'][0]['model'] == 'gpt-6.2-sol', row
    assert not row['budget_reached']
    weekly = member.team('overview')['weekly_budget']
    assert (weekly['used'],weekly['limit'],weekly['reset']) == (50_000,55_000,expected_reset), weekly
    wait_for(lambda: any(notice['kind']=='weekly_usd' and not notice['reached']
                         for notice in member.api('/api/team/notices')))
    second = Emp(stack, emppy, root/'second-member-device', {'codex_base_url':mock}, release_url, ca)
    second.install('team')
    second.auth.write_text(json.dumps(auth(expiry+120)))
    (second.root/'state/api_prices.json').write_text(prices)
    second.restart()
    if browser:
        browser.navigate(member.url)
        wait_for(lambda: browser.execute('return !document.querySelector("#team_workspace_open").hidden;'))
        browser.execute('document.querySelector("#team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-copy-login]");'))
        browser.execute('''
            Object.defineProperty(navigator,'clipboard',{configurable:true,
              value:{writeText:async text => {window.fixtureTeamClipboard=text;}}});
            document.querySelector('[data-team-copy-login]').click();
        ''')
        login_info = wait_for(lambda: browser.execute('return window.fixtureTeamClipboard;'))
        assert json.loads(login_info) == {'workspace':'Lab','base':workspace,'name':'member'}
        (output/'copy-workspace-sign-in.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.navigate(second.url)
        wait_for(lambda: browser.execute('return !document.querySelector("#team_workspace_open").hidden;'))
        browser.execute('document.querySelector("#team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_switch_entry");'))
        browser.execute('document.querySelector("#team_switch_entry").click();')
        browser.execute('''
          const field = document.querySelector('#team_name');
          const paste = text => {
            const event = new ClipboardEvent('paste',{bubbles:true,cancelable:true});
            Object.defineProperty(event,'clipboardData',{value:{getData:format => format === 'text/plain' ? text : ''}});
            return field.dispatchEvent(event);
          };
          if (!paste('member')) throw Error('Ordinary username paste was intercepted');
          if (paste(arguments[0])) throw Error('Copied sign-in info was not recognized');
        ''', login_info)
        (output/'paste-workspace-sign-in.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        filled = browser.execute('''
            const password = document.querySelector('#team_password');
            return {address:document.querySelector('#team_base').value === arguments[0],
              account:document.querySelector('#team_name').value === 'member',
              focus:password === document.activeElement, emptyPassword:password.value === '',
              error:document.querySelector('#modal_status').textContent};
        ''', workspace)
        assert filled == {'address':True,'account':True,'focus':True,'emptyPassword':True,'error':''}, filled
        assert not second.api('/api/team/status')['joined']
        browser.execute('document.querySelector("#team_password").value=arguments[0]; document.querySelector("#modal_submit").click();', PASSWORD)
        wait_for(lambda: second.api('/api/team/status').get('identity',{}).get('member',{}).get('id') == identity['id'])
    else:
        assert second.login(workspace, 'member')['id'] == identity['id']
    assert second.team('sync') == {'synced':0,'skipped_native':1}
    assert not second.api('/api/config')['accounts']
    native_headers = {'Authorization':'Bearer '+auth(expiry+120)['tokens']['access_token'],
                      'chatgpt-account-id':'shared-account'}
    websocket_request(second.base, native_headers, {**body,'model':'gpt-6.2-sol','service_tier':'priority'})
    assert upstream.requests[-1]['body']['model'] == 'gpt-6.2-sol'
    assert upstream.requests[-1]['body']['service_tier'] == 'priority'
    row = wait_for(lambda: row if (row := usage()) and row['requests']==2 else None)
    assert (row['input_tokens'],row['output_tokens'],row['estimated_usd_nanos']) == (20,6,100_000), row
    assert not row['budget_reached'] and row['budget_usage_nanos'] is None
    assert second.team('overview')['weekly_budget']['used'] == 100_000
    wait_for(lambda: {notice['kind'] for notice in second.api('/api/team/notices') if notice['reached']}
             == {'weekly_usd'})
    daily_mode = {'action':'mode','mode':'daily_usd'}
    assert admin.team('weekly_budget', daily_mode)['budget_mode'] == 'daily_usd'
    assert second.team('overview')['weekly_budget'] is None
    assert usage()['budget_reached']
    assert admin.team('weekly_budget', weekly_mode)['budget_mode'] == 'weekly_usd'
    assert second.team('overview')['weekly_budget']['limit'] == 55_000
    vps.team('budget', {'member':identity['id'],'daily_usd_nanos':5_000_000_000})
    if browser:
        browser_roles(browser, output, [(member,'member',['publish','sync']),
                                        (admin,'team-admin',['manage','publish','sync'])])
        def button_sizes():
            return browser.execute('''
                const handover = document.querySelector("[data-team-handover]").getBoundingClientRect();
                const add = document.querySelector("[data-member-add]").getBoundingClientRect();
                return {gap:add.left-handover.right, handoverHeight:handover.height, addHeight:add.height};
            ''')
        before = button_sizes()
        assert 0 <= before['gap'] <= 10, before
        wait_for(lambda: browser.execute('return !document.querySelector("[data-team-budget-mode]").hidden;'))
        for mode in ('daily_usd','weekly_usd'):
            browser.execute('''
              const picker=document.querySelector('[data-team-budget-mode]');
              picker.value=arguments[0]; picker.dispatchEvent(new Event('change',{bubbles:true}));
            ''', mode)
            wait_for(lambda: browser.execute('return !document.querySelector("[data-team-budget-mode]").disabled && document.querySelector("[data-team-budget-mode]").value === arguments[0];', mode))
            assert admin.team('list_members')['budget_mode'] == mode
        (output/'member-management-weekly.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("[data-team-weekly-reset]").click();')
        wait_for(lambda: browser.execute('return document.querySelector("[data-weekly-weekday]")?.value === "0" && document.querySelector("[data-weekly-hour]")?.value === "16";'))
        (output/'weekly-reset.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("[data-weekly-weekday]").value="3"; document.querySelector("[data-weekly-hour]").value="9"; document.querySelector("#modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-member-budget]");'))
        assert admin.team('list_members')['weekly_reset'] == {**expected_reset,'weekday':3,'hour':9}
        field = f'[data-team-member="{identity["id"]}"] [data-member-budget]'
        browser.execute('const field=document.querySelector(arguments[0]);field.value="";field.dispatchEvent(new Event("change",{bubbles:true}));',field)
        wait_for(lambda: member.team('overview')['weekly_budget']['limit'] is None)
        browser.execute('const field=document.querySelector(arguments[0]);field.value="14";field.dispatchEvent(new Event("change",{bubbles:true}));',field)
        wait_for(lambda: member.team('overview')['weekly_budget']['limit'] == 14_000_000_000)
        browser.execute('document.querySelector("[data-member-add]").click();')
        wait_for(lambda: browser.execute('return /[0-9]{7}/.test(document.querySelector("[data-member-add]").textContent);'))
        assert button_sizes()['addHeight'] == before['addHeight']
        request(browser.base+'/window/rect', {'width':640,'height':680})
        browser.navigate(member.url)
        wait_for(lambda: browser.execute('return document.querySelector("#team_workspace_open")?.hidden === false;'))
        browser.execute('document.querySelector("#team_workspace_open").click();')
        wait_for(lambda: browser.execute('return document.querySelector("[data-team-today]")?.textContent.includes(" / 14.00") && !document.querySelector("[data-team-weekly]").hidden;'))
        (output/'member-weekly-usd.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        admin.team('weekly_budget', daily_mode)
        wait_for(lambda: browser.execute('return document.querySelector("[data-team-today]")?.textContent.includes(" / 5.00");'))
        assert browser.execute('''
            const amount = document.querySelector("[data-team-today]");
            const currency = amount.parentElement.previousElementSibling?.textContent;
            const help = document.querySelector("#modal_help");
            return currency?.includes("USD") && !currency.includes("UTC") &&
                getComputedStyle(amount).whiteSpace === "nowrap" &&
                !help.hidden && help.nextElementSibling.id === "modal_close";
        ''')
        (output/'member-narrow.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        websocket_request(second.base, native_headers, {**body,'model':'gpt-6.2-sol','service_tier':'ultrafast'})
        assert upstream.requests[-1]['body']['service_tier'] == 'ultrafast'
        wait_for(lambda: browser.execute('return document.querySelector("[data-team-today-tokens]")?.textContent === "39";'), 30)
    admin.team('review_service', {**review,'shared':False})
    member.team('sync')
    calls = len(upstream.requests)
    assert request(member.base+'/v1/responses', body, local)[0] == 403
    assert len(upstream.requests) == calls
    removal = {'id':published['id'],'version':published['version']}
    member.team('remove_service', removal, 403)
    request_count = usage()['requests']
    if browser_enabled:
        request(browser.base+'/window/rect', {'width':1160,'height':760})
        browser.navigate(admin.url)
        wait_for(lambda: browser.execute('return document.querySelector("#team_workspace_open")?.hidden === false;'))
        browser.execute('document.querySelector("#team_workspace_open").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
        browser.execute('document.querySelector("[data-team-choice=manage]").click();')
        browser.execute('document.querySelector("[data-team-manage-subscriptions]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-remove-service]");'))
        (output/'subscription-management.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("[data-remove-service]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_remove_shared_service");'))
        (output/'remove-subscription.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("#modal_back").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-remove-service]");'))
        assert admin.team('shared_services')['services'][0]['id'] == published['id']
        browser.execute('document.querySelector("[data-remove-service]").click(); document.querySelector("#modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_shared_services") && !document.querySelector("[data-remove-service]");'))
    else:
        assert admin.team('remove_service', removal) == {'ok':True}
    assert not admin.team('shared_services')['services']
    assert not member.team('subscriptions')['subscriptions']
    assert usage()['requests'] == request_count
    member.team('sync')
    assert request(member.base+'/v1/responses', body, local)[0] == 403
    renewed = admin.team('publish')
    assert renewed['review_required'] and renewed['version'] == published['version']+2
    member.team('remove_member', {'id':identity['id']}, 403)
    if browser:
        browser.execute('document.querySelector("[data-team-manage-members]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-member-remove]");'))
        (output/'remove-team-member-action.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("[data-member-remove]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_remove_member");'))
        (output/'remove-team-member-confirm.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
        browser.execute('document.querySelector("#modal_back").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-member-remove]");'))
        assert member.api('/api/team/status')['joined'] and second.api('/api/team/status')['joined']
        browser.execute('document.querySelector("[data-member-remove]").click(); document.querySelector("#modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("#team_manage tbody") && !document.querySelector("[data-member-remove]");'))
    else:
        assert admin.team('remove_member', {'id':identity['id']}) == {'ok':True}
    for emp in (member, second):
        if emp.api('/api/team/status')['joined']:
            emp.team('status', expected=401)
        assert not emp.api('/api/team/status')['joined']
    assert not any(row['id']==identity['id'] for row in admin.team('list_members')['members'])
    history = next(row for row in admin.team('usage', {'period':'lifetime'})['rows'] if row['member']==identity['id'])
    assert history['requests']==request_count and history['name']=='member'
    assert any(row['kind']=='member_removed' and row['target_login']=='member' for row in admin.team('audit')['entries'])
    if browser:
        browser.navigate(member.url)
        wait_for(lambda: browser.execute('return document.querySelector("#emp_logo circle.on") && document.querySelector("#emp_logo").closest("h1").ariaLabel === "EMP";'))
        (output/'emp-wordmark.png').write_bytes(base64.b64decode(request(browser.base+'/screenshot')[1]['value']))
    assert Path(saved).read_bytes() == private_auth
    websocket_request(member.base, local, body)
    assert len(upstream.requests) == calls+1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, required=True)
    parser.add_argument('--codex', type=Path)
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument('--vps-signout-only', action='store_true')
    selection.add_argument('--team-usage-only', action='store_true')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--no-browser', action='store_true')
    parser.add_argument('--hold-demo', action='store_true', help='keep the synthetic review server open until interrupted')
    parser.add_argument('--bundled-core',type=Path,help='official Linux EMP 0.14.3 binary for the update handoff test')
    parser.add_argument('--transition-core',type=Path,help='packaged Core with adjacent transition module assets')
    parser.add_argument('--decision-update',type=Path,help='independent Decision worker with a newer beta version')
    parser.add_argument('--decision-update-next',type=Path,help='third Decision worker for artifact retention')
    enrollment = parser.add_mutually_exclusive_group()
    enrollment.add_argument('--lan', action='store_true', help='exercise discovery on a private IPv4 network')
    enrollment.add_argument('--manual-https', action='store_true', help='enter an isolated HTTPS origin by hand')
    args = parser.parse_args()
    if args.hold_demo and not args.no_browser:
        parser.error('--hold-demo requires --no-browser so the review links are unused')
    if not (args.vps_signout_only or args.team_usage_only) and args.codex is None:
        parser.error('--codex is required for the full suite')
    args.output.mkdir(parents=True, exist_ok=True)
    binaries = args.binaries.resolve()
    emppy = binaries / 'EMP'
    suffix = 'linux-x86_64' if os.uname().machine == 'x86_64' else 'linux-aarch64'
    checks = []
    with contextlib.ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-modules-')))
        downloads = None
        if not args.no_browser:
            target = Path(__file__).resolve().parents[2]/'target'
            target.mkdir(exist_ok=True)
            download_root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-team-downloads-',dir=target)))
            downloads = download_root/'downloads'
        release = http_server(stack, Release)
        release.version = subprocess.check_output([str(emppy), '--version'], text=True).strip().split()[-1]
        release.assets = {f'EMP-mod-{name}-{suffix}': (binaries / f'empmod-{name}').read_bytes() for name in KINDS}
        release.corrupt = None
        release_url = f'http://127.0.0.1:{release.server_port}'
        if args.bundled_core:
            transition = args.transition_core.resolve() if args.transition_core else emppy
            elapsed = bundled_transition(stack,root,args.bundled_core.resolve(),transition,release,release_url,suffix)
            checks.append(f'Official v0.14.3 update worker activates three verified local modules in {elapsed[0]}s with 65s-stalled downloads, and rolls back a corrupt embedded module in {elapsed[1]}s')
        upstream = http_server(stack, Upstream)
        upstream.requests, upstream.sequence = [], itertools.count(1)
        upstream.catalog = {'data': []}
        upstream.respond, upstream.response_events = lambda _: fixture.message(), fixture.response_events
        mock = f'http://127.0.0.1:{upstream.server_port}/v1'
        workspace_port = free_port()
        vps_proc, vps_url = start_vps(stack,binaries/'empmod-team',root/'vps',workspace_port)
        workspace, ca, overrides = https_workspace(stack, root, workspace_port) if args.lan or args.manual_https else (vps_url, None, None)
        vps = Emp(stack, emppy, root/'deployment-emp', {'codex_base_url':mock}, release_url, ca)
        if args.team_usage_only:
            verify_team_usage(stack, emppy, root, vps, workspace, release, mock, ca, upstream,
                              args.output, not args.no_browser)
            print(json.dumps({'team_subscription_permissions_and_usage':'passed',
                              'browser':not args.no_browser}))
            return
        if args.vps_signout_only:
            vps.install('team')
            vps.login(workspace,'deployment')
            vps.team('members',{'name':'signout-member','role':'member','sync':True,
                                'publish':False,'password':PASSWORD})
            browser = None if args.no_browser else Browser(stack, root)
            verify_vps_signout(root, vps, workspace, vps_url, 'signout-member', browser)
            print(json.dumps({'vps_signout_and_account_switch':'passed',
                              'browser':not args.no_browser}))
            return
        provider_data = {'providers': [
            {'id':name, 'name':label, 'base_url':mock, 'api_key':PROVIDER_KEY,
             'auth_mode':'api_key', 'protocol':'responses',
             'decision_api':{'protocol':'openai','model':'fixture-decision'}}
            for name,label in [('publisher','Shared fixture'), ('alternate','Alternate fixture')]
        ], 'models': [{'id':name+'/probe','provider':name,'upstream_id':'gpt-6.1-sol'}
                      for name in ('publisher','alternate')],
        'decision_provider':'publisher',
        'model_groups':[{'id':'auto/probe','name_prefix':'auto','sources':['publisher/probe','alternate/probe']}]
        }
        admin = Emp(stack,emppy,root/'admin-emp',provider_data,release_url,ca)
        member = Emp(stack,emppy,root/'member-emp',{'codex_base_url':mock,'subscription_search':{'enabled':True}},release_url,ca)

        clean_modules = admin.api('/api/modules')['modules']
        assert all(not row['active'] and row['installed_version'] is None
                   and row['version'] != release.version for row in clean_modules)
        admin.api('/api/gateway',expected=404)
        admin.api('/api/decisions/models',{},404)
        admin.api('/v1/decisions',{},404)
        admin.api('/v1/systemone',{},404)
        for kind in KINDS:
            assert request(admin.base + f'/assets/module-{kind}.js')[0] == 404
        assert not any(item['slug']=='auto/probe' for item in admin.api('/v1/models?client_version=0.169.0')['models'])
        release.corrupt = f'EMP-mod-aggregate-{suffix}'
        assert admin.install('aggregate',409)['error']['code'] == 'checksum_mismatch'
        release.corrupt = None
        aggregate_asset = f'EMP-mod-aggregate-{suffix}'
        original = release.assets[aggregate_asset]
        release.assets[aggregate_asset] = release.assets[f'EMP-mod-decision-{suffix}']
        assert admin.install('aggregate',409)['error']['code'] == 'incompatible_module'
        release.assets[aggregate_asset] = original
        local = {'Authorization':'Bearer ' + LOCAL_KEY}
        websocket_request(admin.base, local, {'model':'publisher/probe','input':'local fixture ' * 12})
        checks.append('Clean Core suppresses all four modules and rejects a tampered asset')
        if not args.no_browser:
            now = int(time.time())
            renamed = Emp(stack,emppy,root/'image-credit-browser',{
                'providers':[{'id':'openai-image-credit','name':'Existing custom API','base_url':mock,
                              'auth_mode':'api_key','protocol':'responses','api_key':'old-synthetic-key'}],
                'image_routing':{'mode':'credit','credit':{'provider':'openai-image-credit',
                                 'model':'gpt-image-2.6-flare','cycle_started_at':now-60,
                                 'expires_at':now+86400,'budget_cents':1000,'percent':95}}
            },release_url)
            browser = Browser(stack,root)
            browser.navigate(renamed.url)
            wait_for(lambda: browser.execute('return document.body.innerText.includes("Existing custom API");'))
            browser.execute("document.querySelector('[data-i18n=add_provider]').click();")
            browser.execute("document.querySelector('[data-ui-action=openai-image-credit-choose]').click();")
            assert browser.execute('return document.getElementById("credit_key").required;')
            original = json.loads(renamed.config.read_text())['providers'][0]
            browser.execute('document.getElementById("credit_key").value="new-synthetic-project-key"; document.getElementById("modal_submit").click();')
            wait_for(lambda: renamed.api('/api/config')['image_routing']['credit']['provider'] == 'openai-image-credit-2')
            providers = {provider['id']:provider for provider in json.loads(renamed.config.read_text())['providers']}
            assert providers['openai-image-credit'].get('api_key_file') == original.get('api_key_file')
            assert providers['openai-image-credit']['base_url'] == original['base_url']
            assert providers['openai-image-credit-2']['base_url'] == 'https://api.openai.com/v1'
            checks.append('The image credit shortcut sends the new key to a separate official OpenAI service after the previous service changes URL')

        module_settings = admin.root/'state/modules/modules.json'
        module_settings.write_text('{invalid manifest')
        admin.restart()
        assert all(row['error']=='invalid_module_settings' for row in admin.api('/api/modules')['modules'])
        websocket_request(admin.base,local,{'model':'publisher/probe','input':'Core works with a broken optional module manifest ' * 5})
        assert admin.install('aggregate',409)['error']['code'] == 'invalid_module_settings'
        assert module_settings.read_text() == '{invalid manifest'
        module_settings.unlink()
        admin.restart()
        checks.append('Core stays usable and refuses mutations while the module manifest is unreadable')

        gateway_store = admin.root/'state/gateway-clients.json'
        gateway_store.write_text('{invalid device database')
        admin.restart()
        gateway = next(row for row in admin.api('/api/modules')['modules'] if row['id'] == 'gateway')
        assert not gateway['active'] and gateway['error'] == 'invalid_gateway_clients'
        assert gateway_store.read_text() == '{invalid device database'
        websocket_request(admin.base,local,{'model':'publisher/probe','input':'Core works with broken device data ' * 5})
        gateway_store.unlink()
        admin.restart()
        checks.append('An invalid optional device store is preserved and does not prevent personal routing')

        core_pid = admin.process.pid
        for kind in KINDS:
            rows = admin.install(kind)['modules']
            row = next(row for row in rows if row['id']==kind)
            assert row['active'] and not row['restart_required']
            with urlopen(admin.base + f'/assets/module-{kind}.js') as panel:
                assert panel.status == 200 and panel.read()
        assert admin.process.pid == core_pid and admin.process.poll() is None
        for emp in (vps,member):
            emp.install('team')
        assert all(row['active'] and row['installed_version'] == row['version']
                   for row in admin.api('/api/modules')['modules'] if row['id'] in KINDS)
        if args.decision_update:
            replacement = args.decision_update.resolve()
            hello = json.loads(subprocess.check_output([replacement], stdin=subprocess.DEVNULL, text=True))
            version = hello["module_version"]
            assert hello['module'] == 'decision' and hello['module_version'] != '0.10-beta'
            asset = f'EMP-mod-decision-{suffix}'
            release.paginate_modules = True
            release.module_releases = {f"emp-mod-decision-api{hello['host_api']}-v{version}": {asset: replacement.read_bytes()}}
            original_core = hashlib.sha256(emppy.read_bytes()).digest()
            files = admin.root/'state/modules'
            neighbors = {path.name: hashlib.sha256(path.read_bytes()).digest()
                         for kind in KINDS if kind != 'decision' for path in files.glob(f'{kind}-*')}
            release.corrupt = asset
            admin.api('/api/modules', {'id':'decision','action':'update'}, 409)
            release.corrupt = None
            update = next(offer for offer in admin.api('/api/modules', {'action':'check_updates'})['updates']
                          if offer['id']=='decision')
            assert update['latest_version'] == version
            updated = admin.api('/api/modules', {'id':'decision','action':'update','revision':update['revision']})['modules']
            active = next(row for row in updated if row['id'] == 'decision')
            assert active['installed_version'] == hello['module_version']
            assert active['active'] and active['active_version'] == hello['module_version'] and not active['restart_required']
            old_revision = hashlib.sha256(release.assets[asset]).hexdigest()
            assert request(admin.base + f'/assets/module-decision.js?revision={old_revision}')[0] == 404
            assert admin.process.pid == core_pid
            del release.module_releases
            admin.api('/api/modules', {'id':'decision','action':'update'})
            assert next(row for row in admin.api('/api/modules')['modules'] if row['id'] == 'decision')['installed_version'] == hello['module_version']
            if args.decision_update_next:
                third = args.decision_update_next
                next_hello = json.loads(subprocess.check_output([third], stdin=subprocess.DEVNULL,text=True))
                original_digest = hashlib.sha256(release.assets[asset]).hexdigest()
                current_digest = hashlib.sha256(replacement.read_bytes()).hexdigest()
                third_digest = hashlib.sha256(third.read_bytes()).hexdigest()
                registry = files/'modules.json'
                registry.rename(files/'saved-fixture.json')
                registry.mkdir()
                release.module_releases = {f"emp-mod-decision-api{next_hello['host_api']}-v{next_hello['module_version']}":{asset:third.read_bytes()}}
                admin.api('/api/modules',{'id':'decision','action':'update'},409)
                assert (files/f'decision-{original_digest}').is_file()
                assert (files/f'decision-{current_digest}').is_file()
                registry.rmdir()
                (files/'saved-fixture.json').rename(registry)
                admin.api('/api/modules',{'id':'decision','action':'update'})
                assert not (files/f'decision-{original_digest}').exists()
                assert all((files/f'decision-{digest}').is_file() for digest in (current_digest,third_digest))
                del release.module_releases
                (files/f'decision-{third_digest}').write_bytes(b'broken selected fixture')
                admin.restart()
                core_pid = admin.process.pid
                assert next(row for row in admin.api('/api/modules')['modules'] if row['id']=='decision')['active_revision']==current_digest
                checks.append('Cleanup happens only after registry commit, preserves the active fallback, and a corrupt third version rolls back')
            release.paginate_modules = False
            assert hashlib.sha256(emppy.read_bytes()).digest() == original_core
            assert all(hashlib.sha256((files/name).read_bytes()).digest() == digest for name,digest in neighbors.items())
            assert request(admin.base+'/v1/decisions', {'state':'updated','questions':{}}, local)[0] == 200
            checks.append('Reject a corrupt independent release, hot update Decision with unchanged Core and sibling workers')
        assert admin.process.pid == core_pid and admin.process.poll() is None
        key = admin.api('/api/gateway/clients',{'name':'Fixture device'})['api_key']
        decision = {'Authorization':'Bearer '+key}
        assert request(admin.base+'/v1/decisions', {'state':'fixture','questions':{}}, decision)[1]['model']=='fixture-decision'
        admin.api('/api/decisions/models',{'provider':'publisher','protocol':'openai'})
        catalog = admin.api('/v1/models?client_version=0.169.0')['models']
        aggregate = next(item['slug'] for item in catalog if item.get('_emp_aggregate') or item['slug']=='auto/probe')
        websocket_request(admin.base,{'Authorization':'Bearer '+key,'session-id':'aggregate-fixture'},
                                     {'model':aggregate,'input':'aggregate fixture ' * 12})
        checks.append('Four assets activate on installation; Decision, device admission and aggregation complete real paths')
        upstream.decision_started = threading.Event()
        upstream.decision_release = threading.Event()
        held = []
        def run_held_decision():
            held.append(request(admin.base+'/v1/decisions', {'state':'hold-during-disable','questions':{}}, local))
        in_flight = threading.Thread(target=run_held_decision, daemon=True)
        in_flight.start()
        try:
            assert upstream.decision_started.wait(10)
            admin.api('/api/modules',{'id':'decision','action':'disable'})
            response_status, response_body = request(admin.base+'/v1/decisions', {'state':'new','questions':{}}, local)
            assert response_status == 404 and response_body['error']['code'] == 'module_disabled'
            assert request(admin.base+'/assets/module-decision.js')[0] == 404
        finally:
            upstream.decision_release.set()
            in_flight.join(15)
        assert held[0][0] == 200
        admin.api('/api/modules',{'id':'decision','action':'enable'})
        assert request(admin.base+'/v1/decisions', {'state':'restored','questions':{}}, local)[0] == 200
        assert admin.process.pid == core_pid
        checks.append('Hot disable blocks new Decision requests and lets the accepted upstream request finish; re-enable keeps the same Core PID')
        admin.api('/api/modules',{'id':'gateway','action':'disable'})
        admin.api('/api/modules',{'id':'aggregate','action':'disable'})
        assert request(admin.base+'/assets/module-aggregate.js')[0] == 404
        assert not any(item['slug'] == aggregate for item in admin.api('/v1/models?client_version=0.169.0')['models'])
        admin.api('/api/gateway',expected=404)
        assert request(admin.base+'/v1/decisions', {'state':'fixture','questions':{}},decision)[0]==401
        assert request(admin.base+'/v1/decisions', {'state':'fixture','questions':{}},local)[0]==200
        checks.append('Gateway disable rejects the old device key while Decision still accepts the local credential')
        admin.api('/api/modules',{'id':'aggregate','action':'uninstall'})
        assert not list((admin.root/'state/modules').glob('aggregate-*'))
        assert any(group['id']=='auto/probe' for group in admin.api('/api/config')['model_groups'])
        admin.install('aggregate')
        websocket_request(admin.base,{**local,'session-id':'aggregate-fixture'},
                          {'model':aggregate,'input':'same aggregate session ' * 8})
        checks.append('Aggregate hot uninstall and reinstall preserves the route and its existing session')

        assert vps.login(workspace,'deployment')['role']=='vps_admin'
        helper = {'publish':True,'sync':True,'password':PASSWORD}
        vps.team('members',{'name':'lab-admin','role':'team_admin',**helper})
        assert admin.login(workspace,'lab-admin')['role']=='team_admin'
        admin.team('members',{'name':'imposter','role':'team_admin',**helper},403)
        assert admin.team('join_code')['code'] is None
        code = admin.team('issue_invite')
        assert re.fullmatch(r'[0-9]{7}', code['code']) and code['expires_in']==202
        assert vps.team('join_code')['code'] is None
        application = {'base':workspace,'name':'researcher','nickname':'研究员一号','password':PASSWORD}
        with sqlite3.connect(root/'vps/workspace.sqlite3') as db:
            assert db.execute('UPDATE join_invites SET expires=?', (int(time.time()) - 1,)).rowcount == 1
        member.team('join', {**application,'code':code['code']}, 403)
        assert admin.team('join_code')['code'] is None
        if args.no_browser:
            code = admin.team('issue_invite')['code']
        else:
            join_browser = Browser(stack, root, download_dir=downloads)
            join_browser.navigate(admin.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.querySelector(arguments[0]).click();','#team_workspace_open')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            join_browser.execute('document.querySelector(arguments[0]).click();','[data-team-choice=manage]')
            open_member_management(join_browser)
            wait_for(lambda: join_browser.execute('return document.querySelector("[data-member-add]")?.disabled === false;'))
            assert not join_browser.execute('return !!document.querySelector(".team-invite-timer");')
            join_browser.execute('document.querySelector(arguments[0]).click();','[data-member-add]')
            wait_for(lambda: join_browser.execute('return !!document.querySelector(".team-invite-timer");'))
            code = join_browser.execute('return document.querySelector("[data-member-add] code").textContent;')
            assert re.fullmatch(r'[0-9]{7}', code)
            assert re.fullmatch(r'3:(?:2[0-2]|1[0-9])', join_browser.execute('return document.querySelector(".team-invite-timer").textContent;'))
            _, screenshot = request(join_browser.base + '/screenshot')
            (args.output/'team-invite.png').write_bytes(base64.b64decode(screenshot['value']))
            join_browser.execute("""
                Object.defineProperty(navigator, 'clipboard', {configurable:true,
                    value:{writeText:async text => window.fixtureTeamClipboard=text}});
            """)
            join_browser.execute('document.querySelector(arguments[0]).click();','[data-member-add]')
            invitation = wait_for(lambda: join_browser.execute('return window.fixtureTeamClipboard;'))
            assert code in invitation and workspace in invitation, invitation
            join_browser.execute('const now=performance.now.bind(performance); Object.defineProperty(performance,"now",{value:()=>now()+203000});')
            wait_for(lambda: join_browser.execute('return !document.querySelector(".team-invite-timer");'))
        if args.lan:
            assert member.team('discover')['workspaces'] == [{'base':workspace+'/', 'name':'Lab'}]
            checks.append('LAN discovery returns only the HTTPS workspace origin and display name')
        if args.no_browser:
            assert member.team('join',{**application,'code':code})['pending']
        else:
            join_browser.navigate(member.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.querySelector(arguments[0]).click();','#team_workspace_open')
            wait_for(lambda: join_browser.execute('return !!document.getElementById("team_join_code");'))
            assert join_browser.execute('return document.getElementById("team_nickname").required && document.getElementById("team_join_code").pattern === "[0-9]{7}";')
            _, screenshot = request(join_browser.base + '/screenshot')
            (args.output / 'team-join.png').write_bytes(base64.b64decode(screenshot['value']))
            if args.lan:
                join_browser.execute('document.getElementById("team_discover").click();')
                wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-origin]");'))
                join_browser.execute('document.querySelector("[data-team-origin]").click();')
                assert join_browser.execute('return document.getElementById("team_base").value;') == workspace+'/'
            join_browser.execute('for(const [id,value] of Object.entries(arguments[0])) document.getElementById(id).value=value;', {
                **({} if args.lan else {'team_base': workspace}), 'team_nickname': application['nickname'], 'team_join_code':code,
                'team_name':application['name'], 'team_password':PASSWORD
            })
            join_browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda: join_browser.execute('return !!document.getElementById("team_pending");'))
            _, screenshot = request(join_browser.base + '/screenshot')
            (args.output / 'team-join-pending.png').write_bytes(base64.b64decode(screenshot['value']))
        assert member.team('login', application, 403)['error']['code']=='workspace_join_pending'
        enrolled = next(row for row in admin.team('list_members')['members'] if row['name']=='researcher')
        assert enrolled['nickname'] == application['nickname'] and enrolled['pending'] and not enrolled['enabled']
        member_id = enrolled['id']
        if args.no_browser:
            assert admin.team('reject_member',{'id':member_id}) == {'ok':True}
            assert member.team('login',application,403)['error']['code']=='workspace_join_rejected'
            assert member.team('login',{**application,'password':'wrong-password'},401)['error']['code']=='workspace_login_failed'
            assert not any(row['id']==member_id for row in admin.team('list_members')['members'])
            rejected = next(row for row in admin.team('audit')['entries'] if row['kind']=='member_rejected')
            assert (rejected['actor'],rejected['target'],rejected['target_login']) == ('lab-admin',application['nickname'],application['name'])
            with sqlite3.connect(root/'vps/workspace.sqlite3') as db:
                assert db.execute('UPDATE join_invites SET expires=0 WHERE code=?',(code,)).rowcount==1
            next_code = vps.team('issue_invite')['code']
            assert member.team('join',{**application,'code':next_code})['pending']
            assert member.team('login',application,403)['error']['code']=='workspace_join_pending'
            new_id = next(row['id'] for row in admin.team('list_members')['members'] if row['name']==application['name'])
            assert new_id != member_id
            member_id = new_id
            admin.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':True,'enabled':True})
            assert admin.team('reject_member',{'id':member_id},409)['error']['code']=='workspace_member_not_pending'
            checks.append('Authenticated rejection removes the pending member, preserves its audit identity and permits a fresh application')
        else:
            join_browser.navigate(admin.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            join_browser.execute('document.querySelector("[data-team-choice=manage]").click();')
            open_member_management(join_browser)
            wait_for(lambda: join_browser.execute('return document.querySelector("[data-member-approve]")?.closest("tr").textContent.includes(arguments[0]);', application['nickname']))
            join_browser.execute('document.querySelector("[data-member-approve]").click();')
            wait_for(lambda: any(row['id']==member_id and row['enabled'] and not row['pending'] for row in admin.team('list_members')['members']))
        admin.team('member_access',{'id':member_id,'role':'team_admin','publish':True,'sync':True,'enabled':True},403)
        identity = member.login(workspace,'researcher')
        assert identity['role']=='member' and identity['nickname']==application['nickname']
        member.team('join_code',expected=403)
        member.team('issue_invite',expected=403)
        for path in ('import','export','export/confirm','lan/start','lan/receive'):
            assert member.api('/api/migration/'+path,{},403)['error']['code']=='workspace_migration_permission_required'
        assert admin.api('/api/migration/export/confirm',{})['confirmation']
        vps.team('member_access',{'id':member_id,'role':'team_admin','publish':False,'sync':True,'enabled':True})
        admin.team('budget',{'member':member_id,'daily_usd_nanos':0},403)
        member.team('status')
        assert member.api('/api/migration/export/confirm',{})['confirmation']
        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':True,'enabled':True})
        assert member.api('/api/migration/export/confirm',{},403)['error']['code']=='workspace_migration_permission_required'
        assert not member.api('/api/modules')['migration_allowed']
        checks.append('A seven-digit invitation expires after 202 seconds and requires approval; migration is administrator-only')
        if args.manual_https:
            checks.append('A manually supplied trusted HTTPS workspace origin completes enrollment without LAN discovery')
        member.team('list_members',expected=403)
        member.team('budget',{'member':member_id,'daily_usd_nanos':0},403)
        admin.team('budget',{'member':member_id,'daily_usd_nanos':-1},400)
        admin.team('budget',{'member':member_id,'daily_usd_nanos':1_000_000_000})
        admin.team('publish',{'provider':'publisher'},400)
        admin.team('publish',{'provider':'subscription:other'},400)
        admin.team('publish',expected=409)
        upstream.catalog = {'models':[{
            'slug':'gpt-6.1-sol','display_name':'GPT-6.1 Sol','description':'Synthetic team catalog',
            'visibility':'list','context_window':272000,'max_context_window':272000,
            'supported_in_api':True,'shell_type':'shell_command','priority':1,
            'base_instructions':'Synthetic fixture','support_verbosity':False,
            'truncation_policy':{'mode':'tokens','limit':10000},'experimental_supported_tools':[],
            'supports_reasoning_summaries':False,'default_reasoning_level':'medium',
            'supported_reasoning_levels':[{'effort':'medium','description':'Medium'}],
            'service_tiers':[{'id':'priority','name':'Fast','description':'Synthetic Fast'},
                             {'id':'ultrafast','name':'Ultrafast','description':'Synthetic Ultrafast'}]}]}
        expiry = int(time.time()) + (86400 if args.hold_demo else 3600)
        access = subscription_token('synthetic-subscription','pro',expiry)
        owner_auth = {'auth_mode':'chatgpt','tokens':{'account_id':'synthetic-subscription',
            'access_token':access,'refresh_token':'not-shared-refresh-token','id_token':'not-shared-id-token'}}
        admin.auth.write_text(json.dumps(owner_auth))
        published = admin.team('publish')
        assert published['version']==1 and published['review_required'] is True
        assert member.team('sync')['synced']==0
        member.team('shared_services',expected=403)
        review = {'id':published['id'],'version':published['version'],'shared':True}
        member.team('review_service',review,403)
        admin.team('review_service',{**review,'version':99},409)
        services = admin.team('shared_services')['services']
        assert services == [{'id':published['id'],'version':1,'name':'Native','models':0,
                             'shared':False,'owner':'lab-admin','available':True,'valid':True,'plan':'pro','usage':None}]
        assert access not in json.dumps(services)
        assert admin.team('review_service',review)['shared']
        checks.append('Only Native Codex can be uploaded and approval never exposes credentials')
        with sqlite3.connect(root/'vps/workspace.sqlite3') as db:
            assert access.encode() not in db.execute('select encrypted from shares').fetchone()[0]
        if not args.no_browser:
            browser_roles(join_browser,args.output,[(vps,'vps-admin',['manage']),
                (admin,'team-admin',['manage','publish','sync']),(member,'member',['publish','sync'])])
            checks.append('Firefox opens the personal workspace summary with dates and daily spend, plus distinct member and subscription management')
        stop(admin.process)
        original_auth = member.auth.read_bytes()
        assert member.team('sync')['synced']==1
        assert member.team('sync')['synced']==0
        assert member.auth.read_bytes()==original_auth
        account_id = 'team-'+published['id']
        collision = Emp(stack,emppy,root/'team-id-collision',{'providers':[
            {'id':account_id,'name':'Personal collision fixture','base_url':mock,'protocol':'responses',
             'auth_mode':'api_key','api_key':'unrelated-personal-key'}],
            'models':[{'id':'collision/model','provider':account_id,'upstream_id':'gpt-6.1-sol'}]},release_url,ca)
        collision.install('team')
        collision.restart()
        collision.login(workspace,'researcher')
        collision.team('sync',expected=409)
        collision.team('logout')
        confirmation = collision.api('/api/migration/export/confirm',{})['confirmation']
        password = 'fixture-migration-password'
        exported = collision.api('/api/migration/export',{'confirmation':confirmation,
                                 'password':password,'selections':[{'kind':'provider','id':account_id,
                                 'name':'Personal collision fixture','models':[{'id':'collision/model','selected':True}]}]})
        assert exported['summary']['providers'] == exported['summary']['models'] == 1
        restored = Emp(stack,emppy,root/'team-id-restored',{},release_url,ca)
        restored.api('/api/migration/import',{'bundle':exported['bundle'],'password':password})
        websocket_request(restored.base,local,{'model':'collision/model','input':'restored fixture '*6})
        assert {key.lower():value for key,value in upstream.requests[-1]['headers'].items()}['authorization']=='Bearer unrelated-personal-key'
        stop(restored.process)
        websocket_request(collision.base,local,{'model':'collision/model','input':'personal fixture '*6})
        assert {key.lower():value for key,value in upstream.requests[-1]['headers'].items()}['authorization']=='Bearer unrelated-personal-key'
        checks.append('An exact team share ID collision preserves the private service, model and key through sync, sign-out and export/import')
        stop(collision.process)
        member.api('/api/accounts/'+account_id+'/models/refresh',{})
        reset_request = {'idempotency_key':'e714453f-d623-4c0a-b81e-918fa0d99409'}
        assert member.api('/api/accounts/'+account_id+'/quota-reset',reset_request,403)['error']['code']=='workspace_reset_forbidden'
        reset_login = request(vps_url+'/workspace/v1/login',
            {'name':'deployment','password':PASSWORD,'device':'d'*32})[1]
        reset_headers = {'Authorization':'Bearer '+reset_login['token']}
        reset = {'share':published['id'],'version':published['version']}
        assert request(vps_url+'/workspace/v1/reset-authority',{**reset,'version':reset['version']+1},reset_headers)[0]==403
        assert vps.team('sync')['synced']==1
        assert vps.api('/api/accounts/'+account_id+'/quota-reset',{'idempotency_key':'invalid'},400)['error']['code']=='quota_reset_invalid_request'
        reset_session = json.loads((vps.root/'state/web-session.json').read_text())['token']
        status, result = request(vps.base+'/api/accounts/'+account_id+'/quota-reset',reset_request,
                                 {'X-EMP-Session':reset_session},timeout=45)
        if status == 200:
            assert result['outcome']=='nothingToReset'
        else:
            assert status == 503 and result['error']['code']=='quota_auth_required', result
        checks.append('Distinct deployment, team admin and member EMPs enforce role changes and budget writes; publisher can go offline')
        stop(vps_proc)
        before = len(upstream.requests)
        version, codex_client, codex_thread = codex_session(stack,args.codex.resolve(),member,args.output/'codex.stderr')
        fixture.turn(codex_client,codex_thread,'Public team fixture')
        codex_client.close()
        assert len(upstream.requests) == before+1
        assert {key.lower():value for key,value in upstream.requests[-1]['headers'].items()}['authorization']=='Bearer '+access
        member_ledger = member.root/'state/usage.sqlite3'

        def queued():
            if not member_ledger.exists():
                return None
            with sqlite3.connect(member_ledger) as db:
                return db.execute('select scope,event_id,payload from team_outbox').fetchone()

        pending = wait_for(queued)
        event = json.loads(pending[2])
        assert set(event) == {'id','share','model','finished_at','input_tokens','cached_input_tokens','output_tokens',
                              'actual_usd_nanos','estimated_usd_nanos'}
        vps_proc,vps_url = start_vps(stack,binaries/'empmod-team',root/'vps',workspace_port)
        personal = wait_for(lambda: (usage if (usage := member.team('usage'))['rows'][0]['requests'] else None))
        assert [(row['name'],row['requests'],row['budget_nanos']) for row in personal['rows']]==[
            (application['nickname'],1,1_000_000_000)], personal
        admin.start()
        all_rows = admin.team('usage')['rows']
        assert {row['name'] for row in all_rows}=={application['nickname'],'lab-admin','deployment'}
        with sqlite3.connect(member_ledger) as db:
            db.execute('insert into team_outbox values (?,?,?)',pending)
        assert member.team('usage')['rows'][0]['requests']==1
        store = root/'vps/workspace.sqlite3'
        with sqlite3.connect(store) as db:
            assert db.execute('select count(*) from usage').fetchone()[0]==1
        checks.append('Real Codex picker routes the shared service while VPS is offline; restart and re-sync produce exactly one VPS bill')

        fixture_login = request(vps_url+'/workspace/v1/login',
            {'name':'researcher','password':PASSWORD,'device':'0'*32})[1]
        member_bearer = {'Authorization':'Bearer '+fixture_login['token']}
        notices_endpoint = '/api/team/notices'
        billable = {**event,'id':'fixture-priced-call','actual_usd_nanos':900_000_000}
        scope,_,_ = pending
        with sqlite3.connect(member_ledger) as db:
            db.execute('insert into team_outbox values (?,?,?)',(scope,billable['id'],json.dumps(billable)))
        wait_for(lambda: any(not notice['reached'] for notice in member.api(notices_endpoint)))
        billable = {**billable,'id':'fixture-reached-call','actual_usd_nanos':1_100_000_000}
        with sqlite3.connect(member_ledger) as db:
            db.execute('insert into team_outbox values (?,?,?)',(scope,billable['id'],json.dumps(billable)))
        wait_for(lambda: any(notice['reached'] for notice in member.api(notices_endpoint)))
        assert not admin.api(notices_endpoint)
        notices = member.api(notices_endpoint)
        assert [notice['reached'] for notice in notices] == [False,True]
        if notice_log := os.environ.get('EMP_E2E_DESKTOP_NOTICE_LOG'):
            def desktop_delivered():
                file = Path(notice_log)
                entries = [json.loads(row) for row in file.read_text().splitlines()] if file.exists() else []
                return len(entries) == 2 and all(row[0] == 'EMP' for row in entries)
            wait_for(desktop_delivered)
            checks.append('The Linux desktop notification service receives both budget notices without the WebUI')
        member.restart()
        assert member.api(notices_endpoint) == notices
        checks.append('Budget warnings are recorded by the member background process when no member WebUI is open and persist through restart')
        billable = {**billable,'id':'fixture-additional-price','actual_usd_nanos':1}
        assert request(vps_url+'/workspace/v1/report',{'events':[billable]},member_bearer)[1]['inserted']==1
        assert request(vps_url+'/workspace/v1/report',{'events':[billable]},member_bearer)[1]['inserted']==0
        rows = member.team('usage')['rows']
        assert len(rows)==1 and rows[0]['budget_reached'] and rows[0]['actual_usd_nanos']==2_000_000_001
        checks.append('The USD threshold is calculated for each member; an acknowledged event replay remains one bill')
        second = Emp(stack,emppy,root/'second-member-device',{'codex_base_url':mock},release_url,ca)
        second.install('team')
        second.restart()
        assert second.login(workspace,'researcher')['id'] == member_id
        assert second.team('sync')['synced'] == 1
        assert second.team('sync')['synced'] == 0
        before_usage = member.team('usage')['rows'][0]
        websocket_request(second.base,local,{'model':'lab-admin/gpt-6.1-sol','input':'Second device fixture '*8})
        def combined_usage():
            row = member.team('usage')['rows'][0]
            return row if row['requests'] == before_usage['requests'] + 1 else None
        merged = wait_for(combined_usage)
        assert merged['budget_nanos'] == before_usage['budget_nanos']
        assert second.team('usage')['rows'][0] == merged
        assert (second.root/'state/team-device-id').read_text() != (member.root/'state/team-device-id').read_text()
        assert second.team('logout')['joined'] is False
        assert member.team('status')['joined'] is True
        member.team('sync')
        stop(second.process)
        checks.append('One member signs in and syncs on two independent EMP devices, sees combined billing and keeps the first device signed in when the second signs out')

        long_model = {**billable,'id':'long-model-fixture','model':'a'*300}
        paired_model = {**billable,'id':'short-model-fixture','model':'paired-fixture'}
        with sqlite3.connect(member_ledger) as db:
            db.executemany('insert into team_outbox values (?,?,?)',
                           ((scope,record['id'],json.dumps(record)) for record in (long_model,paired_model)))
        wait_for(lambda: {'a'*300,'paired-fixture'}.issubset({row['model'] for row in member.team('usage')['rows'][0]['models']}))
        with sqlite3.connect(member_ledger) as db:
            assert not db.execute('SELECT 1 FROM team_outbox WHERE event_id IN (?,?)',
                                  (long_model['id'],paired_model['id'])).fetchone()
        checks.append('Long model names and later queued usage both reach the ledger')
        old_call = {**billable,'id':'fixture-historical-call','model':'older-image-model','actual_usd_nanos':2_000_000_000,
                    'finished_at':int(time.time())-3*86400}
        assert request(vps_url+'/workspace/v1/report',{'events':[old_call]},member_bearer)[1]['inserted']==1
        recent = member.team('usage',{'period':'1d'})['rows'][0]
        week = member.team('usage',{'period':'7d'})['rows'][0]
        assert 'older-image-model' not in {model['model'] for model in recent['models']}
        assert 'older-image-model' in {model['model'] for model in week['models']}
        assert recent['budget_usage_nanos']==week['budget_usage_nanos']
        permissions = {'id':member_id,'fast':False,'ultrafast':True}
        member.team('member_tiers',permissions,403)
        vps.team('member_tiers',permissions)
        member.team('sync')
        assert member.team('status')['identity']['member']['ultrafast']
        slugs = [item['slug'] for item in member.api('/v1/models?client_version=0.170.0')['models']]
        model = next((slug for slug in slugs if unquote(slug).startswith('lab-admin/')), None)
        if not model:
            debug = {'slugs': slugs, 'workspace': member.team('status')['identity']['member'],
                     'accounts': [(item['id'], item.get('name'), item.get('credential_status'))
                                  for item in member.api('/api/config')['accounts']],
                     'modules': member.api('/api/modules')['modules'],
                     'models': member.api('/api/accounts/' + account_id + '/models')}
            raise AssertionError(debug)
        payload = {'model':model, 'input':'Synthetic speed fixture ' * 8}
        denied = request(member.base+'/v1/responses',{**payload,'service_tier':'priority'},local)
        assert denied[0]==403 and 'Fast is not permitted' in json.dumps(denied[1]), denied
        websocket_request(member.base,local,{**payload,'service_tier':'ultrafast'})
        assert upstream.requests[-1]['body']['service_tier']=='ultrafast'
        _, codex_client, codex_thread = codex_session(stack,args.codex.resolve(),member,args.output/'codex-speed.stderr')
        for tier, fast, ultrafast in [('ultrafast',False,True),('priority',True,False)]:
            vps.team('member_tiers',{'id':member_id,'fast':fast,'ultrafast':ultrafast})
            member.team('sync')
            fixture.turn(codex_client,codex_thread,f'Public speed fixture {tier}',serviceTier=tier)
            recorded = upstream.requests[-1]
            assert (recorded['body']['model'],recorded['body']['service_tier']) == ('gpt-6.1-sol',tier), recorded['body']
            assert {key.lower():value for key,value in recorded['headers'].items()}['authorization']=='Bearer '+access
        codex_client.close()
        assert request(member.base+'/v1/responses',{**payload,'service_tier':'ultrafast'},local)[0]==403
        vps.team('member_tiers',permissions)
        member.team('sync')
        checks.append(f'Codex {version} app-server turns through Other subscription carry Fast and Ultrafast to the authorized upstream account; disallowed tiers return 403')
        checks.append('Team per-model costs follow the selected period; UTC daily limits stay fixed and speed permissions block only the disallowed tier')
        admin_id = admin.team('status')['identity']['member']['id']
        assert next(row for row in admin.team('list_members')['members'] if row['id']==admin_id)['can_budget']
        member.team('audit',expected=403)
        events = admin.team('audit')['entries']
        assert [(event['actor'],event['target']) for event in events if event['kind']=='reset_requested']==[('deployment','lab-admin')]
        assert any(event['kind']=='member_approved' and event['actor']=='lab-admin' and event['target']==application['nickname'] for event in events)
        if args.no_browser:
            assert admin.team('budget',{'member':admin_id,'daily_usd_nanos':2_500_000_000})
        if not args.no_browser:
            join_browser.navigate(member.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: join_browser.execute(r'return /^2\.00\s*\/\s*1\.00$/.test(document.querySelector("[data-team-today]")?.textContent);'))
            wait_for(lambda: join_browser.execute('return document.querySelector("[data-team-lifetime-tokens]").textContent !== "—";'))
            assert join_browser.execute('return !document.querySelector(".team-usage-summary button") && document.querySelector("#team_usage_detail").hidden;')
            join_browser.execute('document.querySelector("[data-team-choice=sync]").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("#team_home [data-team-choice=sync]:enabled");'))
            join_browser.execute('document.querySelector("[data-team-usage-toggle]").click();')
            wait_for(lambda: join_browser.execute('return document.querySelector("[data-team-breakdown]")?.textContent.includes("paired-fixture");'))
            join_browser.execute('document.querySelector("#team_usage_detail [data-team-period=\\\"1d\\\"]").click();')
            wait_for(lambda: join_browser.execute('const detail=document.querySelector("[data-team-breakdown]");return detail?.textContent.includes("paired-fixture") && !detail.textContent.includes("older-image-model");'))
            join_browser.execute('document.querySelector("#team_usage_detail [data-team-period=\\\"7d\\\"]").click();')
            wait_for(lambda: join_browser.execute('return document.querySelector("[data-team-breakdown]")?.textContent.includes("older-image-model");'))
            _, screenshot = request(join_browser.base + '/screenshot')
            (args.output / 'member-usage-summary.png').write_bytes(base64.b64decode(screenshot['value']))
            join_browser.execute('document.querySelector("[data-team-usage-toggle]").click();')
            assert join_browser.execute('return document.querySelector("#team_usage_detail").hidden && document.querySelector("[data-team-usage-toggle]").getAttribute("aria-expanded")==="false";')
            join_browser.navigate(admin.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            join_browser.execute('document.querySelector("[data-team-choice=manage]").click();')
            open_member_management(join_browser)
            own_row = f'tr[data-team-member="{admin_id}"]'
            own_budget = own_row+' [data-member-budget]'
            wait_for(lambda: join_browser.execute('return document.querySelector(arguments[0])?.placeholder==="∞";',own_budget))
            assert join_browser.execute("""
                return !document.querySelector(arguments[0]+" [data-team-permission]:enabled");
            """,own_row)
            join_browser.execute('const field=document.querySelector(arguments[0]);field.value="2.5";field.dispatchEvent(new Event("change",{bubbles:true}));',own_budget)
            wait_for(lambda: next(row for row in vps.team('list_members')['members'] if row['id']==admin_id)['budget_nanos']==2_500_000_000)
            _, screenshot = request(join_browser.base+'/screenshot')
            (args.output/'team-member-management.png').write_bytes(base64.b64decode(screenshot['value']))
            join_browser.execute('document.querySelector("[data-team-audit]").click();')
            wait_for(lambda: join_browser.execute('return [...document.querySelectorAll("#team_audit tbody tr")].some(row=>row.textContent.includes("lab-admin") && row.textContent.includes("2.50"));'))
            _, screenshot = request(join_browser.base+'/screenshot')
            (args.output/'team-operation-log.png').write_bytes(base64.b64decode(screenshot['value']))
            join_browser.navigate(vps.url.split('/?')[0])
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            join_browser.execute('document.querySelector("[data-team-choice=manage]").click();')
            open_member_management(join_browser)
            view_row = 'document.querySelector("tr[data-team-member=\\\""+arguments[0]+"\\\"]")'
            wait_for(lambda: join_browser.execute('return !!'+view_row+'?.querySelector(".team-token-detail");',member_id))
            budget = f'tr[data-team-member="{member_id}"] [data-member-budget]'
            join_browser.execute('const field=document.querySelector(arguments[0]);field.value="1.25";field.dispatchEvent(new Event("change",{bubbles:true}));',budget)
            wait_for(lambda: next(row for row in vps.team('list_members')['members'] if row['id']==member_id)['budget_nanos']==1_250_000_000)
            wait_for(lambda: join_browser.execute('return !document.querySelector(arguments[0]).disabled;',budget))
            join_browser.execute('const field=document.querySelector(arguments[0]);field.value="1";field.dispatchEvent(new Event("change",{bubbles:true}));',budget)
            wait_for(lambda: next(row for row in vps.team('list_members')['members'] if row['id']==member_id)['budget_nanos']==1_000_000_000)
            upload = f'tr[data-team-member="{member_id}"] [data-team-permission=publish]'
            join_browser.execute('document.querySelector(arguments[0]).click();',upload)
            wait_for(lambda: next(row for row in vps.team('list_members')['members'] if row['id']==member_id)['publish'])
            wait_for(lambda: join_browser.execute('return !document.querySelector(arguments[0]).disabled;',upload))
            join_browser.execute('document.querySelector(arguments[0]).click();',upload)
            wait_for(lambda: not next(row for row in vps.team('list_members')['members'] if row['id']==member_id)['publish'])
            join_browser.execute('document.querySelector("[data-team-period=\\\"7d\\\"]").click();')
            wait_for(lambda: join_browser.execute('return !!'+view_row+'?.querySelector("template")?.content.textContent.includes("older-image-model");',member_id))
            assert join_browser.execute('return document.querySelector(\'[data-team-period="7d"]\').getAttribute("aria-pressed");') == 'true'
            token_details = f'tr[data-team-member="{member_id}"] .team-token-detail'
            join_browser.hover(token_details)
            bubble = '''const bubble=document.getElementById("shadow_feedback");
                return !bubble.hidden && bubble.textContent.includes("older-image-model") && bubble.textContent.includes("$2");'''
            wait_for(lambda: join_browser.execute(bubble))
            _, screenshot = request(join_browser.base + '/screenshot')
            (args.output/'management-model-usage.png').write_bytes(base64.b64decode(screenshot['value']))
            join_browser.hover('#modal_title')
            wait_for(lambda: not join_browser.execute(bubble))
            join_browser.execute('document.querySelector(arguments[0]).focus();',token_details)
            wait_for(lambda: join_browser.execute(bubble))
            checks.append('Firefox syncs without a dialog, toggles and filters one dashboard detail panel, edits budget and permissions in place, and opens per-model usage on hover and keyboard focus')

        events = vps.team('audit')['entries']
        assert any(event['kind']=='budget' and event['actor']==event['target']=='lab-admin'
                   and event['details']['daily_usd_nanos']==2_500_000_000 for event in events)
        assert any(event['kind']=='member_tiers' and event['actor']=='deployment' and event['target']==application['nickname']
                   and event['details']['ultrafast'] is True for event in events)
        assert events == admin.team('audit')['entries']
        checks.append('Team administrators can set their own daily limit and inspect the shared approval, budget, speed permission and authorized reset log; ordinary members cannot read it')

        if overrides is not None:
            overrides['/workspace/v1/members'] = {'members':[{'id':'ordinary "<text>'}]}
            assert vps.team('list_members',expected=502)['error']['code']=='invalid_workspace_reply'
            overrides.clear()
            overrides['/workspace/v1/usage'] = {'rows':[{'member':member_id,'requests':'<ordinary text>',
                                                         'models':[],'unknown_cost_requests':0}]}
            assert member.team('usage',{'period':'1d'},502)['error']['code']=='invalid_workspace_reply'
            overrides.clear()
            checks.append('Malformed member identifiers and non-numeric request counts from the synthetic HTTPS workspace are rejected at the HTTP boundary')
            result_id = 'unconfirmed-report-fixture'
            rejected_report = threading.Event()
            report_attempt = itertools.count(1)
            def unconfirmed_report():
                if next(report_attempt) >= 2:
                    rejected_report.set()
                return {}
            overrides['/workspace/v1/report'] = unconfirmed_report
            with sqlite3.connect(member_ledger) as db:
                db.execute('insert into team_outbox values (?,?,?)',
                           (scope,result_id,json.dumps({**event,'id':result_id})))
            member.team('usage')
            assert rejected_report.wait(25)
            with sqlite3.connect(member_ledger) as db:
                assert db.execute('select 1 from team_outbox where event_id=?',(result_id,)).fetchone()
            overrides.clear()
            def unconfirmed_usage_retried():
                with sqlite3.connect(store) as db:
                    return db.execute('select count(*) from usage where id=?',(result_id,)).fetchone()[0]==1
            wait_for(unconfirmed_usage_retried,25)
            checks.append('Unconfirmed VPS report keeps the device outbox until a valid reply acknowledges its retry')

        stop(member.process)
        team_path = member.root/'state/team-profile.json'
        team_path.write_text('invalid synthetic membership file')
        member.start()
        member.api('/api/accounts/'+account_id+'/quota-reset',reset_request,403)
        member.api('/api/migration/export/confirm',{},403)
        assert not any(unquote(row['slug']).startswith('lab-admin/')
                       for row in member.api('/v1/models?client_version=0.170.0')['models'])
        assert request(member.base+'/v1/responses',payload,local)[0] in (400,403,404)
        assert member.api('/api/team/status')['recovery_required']
        member.login(workspace,'researcher')
        assert member.team('sync')['synced'] >= 1
        websocket_request(member.base,local,payload)
        checks.append('A damaged workspace ownership file blocks shared routes and Banked Reset until signing in and syncing')

        stop(member.process)
        team_path.unlink()
        member.start()
        assert member.api('/api/team/status')['recovery_required']
        member.api('/api/migration/export/confirm',{},403)
        member.api('/api/accounts/'+account_id+'/quota-reset',reset_request,403)
        assert request(member.base+'/v1/responses',payload,local)[0] in (400,403,404)
        member.login(workspace,'researcher')
        assert member.team('sync')['synced'] >= 1
        assert member.team('sync')['synced'] == 0
        assert len([row for row in member.api('/api/config')['accounts'] if row['id']==account_id])==1
        websocket_request(member.base,local,payload)
        checks.append('A missing workspace ownership file denies team access until signing in and syncing restores the original shared route without duplicates')

        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':False,'enabled':True})
        member.team('sync',expected=403)
        member.restart()
        assert request(member.base+'/v1/responses',payload,local)[0] in (400,403,404)
        calls = len(upstream.requests)
        assert request(member.base+'/v1/alpha/search',{'query':'synthetic'},local)[0] == 503
        assert len(upstream.requests) == calls
        assert member.team('usage')['rows'][0]['requests'] > 0
        checks.append('A revoked subscription sends no search requests, and readable prior usage remains available after sync permission is removed')
        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':True,'enabled':True})
        member.team('sync')
        websocket_request(member.base,local,payload)
        checks.append('Revoking sync without disabling the member takes effect immediately and survives restart')

        member.auth.write_bytes(admin.auth.read_bytes())
        assert member.team('sync')['skipped_native']==1
        assert not any(unquote(row['slug']).startswith('lab-admin/') for row in member.api('/v1/models?client_version=0.170.0')['models'])
        personal_login = json.loads(member.auth.read_text())
        personal_access = subscription_token('personal-fixture','pro',expiry)
        personal_login['tokens'].update(access_token=personal_access,id_token=personal_access,account_id='personal-fixture')
        member.auth.write_text(json.dumps(personal_login))
        member.api('/api/native/login/use-local-key',{'account_action':'keep'})
        private_caller = {'Authorization':'Bearer '+json.loads(member.auth.read_text())['OPENAI_API_KEY']}
        personal = next(row for row in member.api('/api/config')['accounts'] if not row.get('team_shared'))
        assert personal['id'] != account_id and personal['credential_set']
        personal_auth = next(row for row in json.loads(member.config.read_text())['accounts'] if row['id']==personal['id'])['auth_file']
        personal_saved = Path(personal_auth).read_bytes()
        member.team('sync')
        assert Path(personal_auth).read_bytes() == personal_saved
        checks.append('Keeping personal Native as Other retains private credentials through subsequent team sync')
        stop(member.process)
        cfg = json.loads(member.config.read_text())
        owned = next(row for row in cfg['accounts'] if row['id']==account_id)
        private = next(row for row in cfg['accounts'] if row['id']==personal['id'])
        private['id'] = 'team-' + 'a'*32
        old_path = Path(personal_auth)
        private_store = old_path.parent.with_name(private['id'])
        old_path.parent.rename(private_store)
        personal_auth = str(private_store/old_path.name)
        private['auth_file'] = personal_auth
        cfg['accounts'] = [owned,private]
        cfg['image_routing'] = {'mode':'auto'}
        cfg['providers'].append({'id':'tier-fallback','name':'Allowed tier fixture','base_url':mock,
                                 'auth_mode':'api_key','protocol':'responses','api_key':'allowed-tier-key'})
        cfg['models'].append({'id':'tier-probe/model','provider':'tier-fallback','upstream_id':unquote(model).rsplit('/',1)[1]})
        cfg['model_groups'] = [{'id':'auto/tier-probe','name':'Policy fixture',
                                'sources':[owned['prefix']+'/'+unquote(model).rsplit('/',1)[1],'tier-probe/model']}]
        member.config.write_text(json.dumps(cfg))
        member.start()
        member.install('aggregate')
        member.restart()
        websocket_request(member.base,{**private_caller,'Session-Id':'permission-check'},{'model':'auto/tier-probe','input':'Tier selection fixture '*5,
                                               'service_tier':'priority'})
        with sqlite3.connect(member_ledger) as db:
            assert db.execute("select owner from aggregate_bindings where model='auto/tier-probe'").fetchone()[0].startswith('provider:tier-fallback:')
        checks.append('Aggregate excludes the shared source before binding a Fast request to the eligible source')
        admin.team('review_service',{**review,'shared':False})
        member.team('sync')
        calls = len(upstream.requests)
        assert request(member.base+'/v1/alpha/search',{'query':'permission fallback'},private_caller)[0] == 200
        assert len(upstream.requests) == calls+1
        assert upstream.requests[-1]['path'].endswith('/alpha/search')
        assert {key.lower():value for key,value in upstream.requests[-1]['headers'].items()}['authorization']=='Bearer '+personal_access
        turn = json.dumps({'turn_id':'after-revocation-image-turn'})
        websocket_request(member.base,private_caller,{'model':'tier-probe/model','input':'Image selection fixture '*5,
                                              'client_metadata':{'x-codex-turn-metadata':turn}})
        status, image = request(member.base+'/v1/images/generations',{'model':'gpt-image-2','prompt':'fixture'},
                                {**private_caller,'x-codex-image-turn-id':'after-revocation-image-turn'})
        assert status == 200, image
        assert {key.lower():value for key,value in upstream.requests[-1]['headers'].items()}['authorization']=='Bearer '+personal_access
        checks.append('Automatic image routing filters revoked sources before it chooses a subscription')
        admin.team('review_service',review)
        member.team('sync')
        checks.append('When search skips a revoked share it tries the next readable personal account, including an older team-shaped ID')

        member.auth.write_bytes(original_auth)
        stop(member.process)
        config = json.loads(member.config.read_text())
        next(row for row in config['accounts'] if row['id']==account_id)['credential_status'] = 'invalid'
        member.config.write_text(json.dumps(config))
        member.start()
        assert member.team('subscriptions')['subscriptions'][0]['status']=='available'
        assert member.team('sync')['synced']==1
        checks.append('Sync skips an identical Native account and restores stale local auth while the VPS snapshot is current')
        fresh_owner = Emp(stack,emppy,root/'fresh-owner',{'codex_base_url':mock},release_url,ca)
        fresh_owner.install('team')
        fresh_owner.restart()
        fresh_owner.login(workspace,'lab-admin')
        owner_expiry = expiry + 600
        owner_auth['tokens']['access_token'] = subscription_token('synthetic-subscription','pro',owner_expiry)
        fresh_owner.auth.write_text(json.dumps(owner_auth))
        renewed = fresh_owner.team('publish')
        assert renewed == {'id':published['id'],'version':2,'review_required':False}
        assert member.team('sync')['synced'] == 1
        assert next(row for row in vps.team('audit')['entries'] if row['kind']=='share_refreshed')['details'] == {
            'share':published['id'],'version':2,'review_required':False}
        checks.append('A second device renews the same Native subscription and members sync it without another approval')

        stop(admin.process)
        legacy_id = 'team-' + '9'*32
        admin_settings = json.loads(admin.config.read_text())
        admin_settings['providers'].append({'id':legacy_id,'name':'Legacy team Key','base_url':mock,
                                            'protocol':'responses','auth_mode':'api_key','api_key':PROVIDER_KEY})
        admin.config.write_text(json.dumps(admin_settings))
        admin.start()
        decision_local = {'Authorization':'Bearer '+admin.api('/api/decisions/connection')['api_key']}
        admin.team('sync')
        assert any(provider['id']==legacy_id and provider['api_key_set'] for provider in admin.api('/api/config')['providers'])
        assert request(admin.base+'/v1/decisions',{'model':'fixture'}, {**decision_local,'X-EMP-Provider':legacy_id})[0] == 200
        checks.append('Sync preserves a preexisting personal API service with a team-shaped ID and allows it in Decision')
        ten_member_demo(admin,vps,member,vps_url,None if args.no_browser else join_browser,args.output,args.hold_demo)
        if args.hold_demo:
            admin.api('/api/modules',{'id':'gateway','action':'enable'})
            admin.restart()
            newcomer = Emp(stack,emppy,root/'newcomer-emp',{},release_url,ca)
            newcomer.install('team')
            newcomer.restart()
            web = Path(__file__).resolve().parents[2]/'crates/emp-app/web'
            views = {}
            for role,emp in [('member',member),('team_admin',admin),('vps_admin',vps),('newcomer',newcomer)]:
                token = json.loads((emp.root/'state/web-session.json').read_text())['token']
                head = '<script>localStorage.setItem("emp_management_session_v1",'+json.dumps(token)+')</script>'
                views[role],_ = candidate_frontend(stack,emp.base,web,index_head=head)
            print(json.dumps({'demo_only':True,'workspace_url':workspace,**views}),flush=True)
            try:
                while all(emp.process.poll() is None for emp in (vps,admin,member)):
                    time.sleep(2)
            except KeyboardInterrupt:
                pass
            return
        checks.append('Ten-person workspace exposes three Pro and one Pro Lite, controls profile visibility and records only online PST days; members can remind the owner and sync renewed credentials without another approval')

        team_role = vps.team('list_members')['members']
        row = next(row for row in team_role if row['id']==member_id)
        vps.team('member_access',{'id':member_id,'role':row['role'],'publish':False,'sync':False,'enabled':False})
        member.team('sync',expected=401)
        member.team('usage',expected=401)
        assert not member.api('/api/team/status')['joined']
        assert not any(unquote(item['slug']).startswith('lab-admin/')
                       for item in member.api('/v1/models?client_version=0.169.0')['models'])
        checks.append('Revoked member loses sync, server usage and team models in the local Codex catalog')
        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':True,'enabled':True})
        member.login(workspace,'researcher')
        assert any(unquote(row['slug']).startswith('lab-admin/')
                   for row in member.api('/v1/models?client_version=0.170.0')['models'])
        member.team('sync')
        member.auth.write_text(json.dumps({'auth_mode':'chatgpt','tokens':{
            'access_token':'free-synthetic-bearer','account_id':'free-fixture',
            'id_token':subscription_token('free-fixture','free',expiry)}}))
        assert not any(row['active'] for row in member.api('/api/modules')['modules'] if row['id']=='gateway')
        turn = json.dumps({'turn_id':'free-fixture-image-turn'})
        websocket_request(member.base, {'Authorization':'Bearer free-synthetic-bearer'},
                          {**payload, 'client_metadata':{'x-codex-turn-metadata':turn}})
        key_file = member.root/'state/gateway-key'
        with subprocess.Popen([str(emppy),'--emp-image-mcp',member.base+'/v1',str(key_file)],env=member.env,
                              cwd=member.root,stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True) as mcp:
            mcp.stdin.write(json.dumps({'jsonrpc':'2.0','id':1,'method':'tools/call',
                'params':{'name':'imagegen','arguments':{'prompt':'fixture image'},
                          '_meta':{'x-codex-turn-metadata':turn}}})+'\n')
            mcp.stdin.flush()
            result = json.loads(mcp.stdout.readline())
            assert not result['result'].get('isError'), result
            image_path = Path(json.loads(result['result']['content'][0]['text'])['path'])
            assert image_path.read_bytes() == b'fake'
            mcp.stdin.close()
        assert upstream.requests[-1]['path'].endswith('/images/generations')
        assert request(member.base+'/v1/images/generations',{'model':'gpt-image-2'},local)[0] == 401
        assert request(member.base+'/v1/alpha/search',{'query':'fixture'},
                       {'Authorization':'Bearer '+key_file.read_text().strip()})[0] == 401
        member.auth.write_bytes(original_auth)
        checks.append('With Free Native and Gateway absent the actual local image MCP produces a file; its credential cannot access search')
        def team_credentials():
            return [Path(row['auth_file']) for row in json.loads(member.config.read_text())['accounts']
                    if row.get('workspace_shared')]
        imported = team_credentials()
        assert imported and all(path.is_file() for path in imported)
        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':False,'enabled':True})
        old_model = 'a' * 511 + '星'
        queued_events = [{**event,'id':'oversized-legacy-fixture','model':old_model},
                         {**event,'id':'following-fixture','model':'gpt-6.1-sol'}]
        with sqlite3.connect(member_ledger) as db:
            db.executemany('insert into team_outbox values (?,?,?)',
                           [(scope,item['id'],json.dumps(item)) for item in queued_events])
        assert member.team('usage')['rows'][0]['requests'] > 0
        device = (member.root/'state/team-device-id').read_text()
        assert scope.endswith(':'+device)
        assert not member.team('logout')['joined']
        assert (member.root/'state/team-device-id').read_text() == device
        member.restart()
        member.login(workspace,'mei')
        with sqlite3.connect(member_ledger) as db:
            assert db.execute('select count(*) from team_outbox where event_id like ?',('%-fixture',)).fetchone()[0]==2
        member.team('logout')
        vps.team('member_access',{'id':member_id,'role':'member','publish':False,'sync':True,'enabled':True})
        member.login(workspace,'researcher')
        def reported_models():
            with sqlite3.connect(store) as db:
                rows = db.execute('select id,model from usage where id in (?,?)',
                                  tuple(item['id'] for item in queued_events)).fetchall()
                return dict(rows) if len(rows)==2 else None
        assert wait_for(reported_models,25) == {'oversized-legacy-fixture':'a'*511,'following-fixture':'gpt-6.1-sol'}
        assert member.api('/api/modules')['migration_allowed'] is False
        checks.append('Unreadable-to-VPS outbox row is normalized without losing its event ID, survives sign-out and restart, and reports only for its original member')
        member.team('logout')
        assert member.api('/api/modules')['migration_allowed']
        assert all(not path.exists() for path in imported)
        assert Path(personal_auth).read_bytes()==personal_saved
        member.login(workspace,'researcher')
        member.team('sync')
        stop(member.process)
        configuration = json.loads(member.config.read_text())
        account = next(row for row in configuration['accounts'] if row['id']==account_id)
        account['name'] = 'lab-admin (Lab)'
        member.config.write_text(json.dumps(configuration))
        member.start()
        assert member.team('sync')['synced']==0
        account = next(row for row in json.loads(member.config.read_text())['accounts'] if row['id']==account_id)
        assert account['name']=='lab-admin' and 'lab-admin (Lab)' in account['route_names']
        websocket_request(member.base,local,{'model':'lab-admin (Lab)/gpt-6.1-sol',
                          'input':'Fixture historic shared naming ' * 8})
        checks.append('A synced subscription uses the owner name, and an existing conversation can still send the previous workspace-suffixed route')
        imported = team_credentials()
        member.api('/api/modules',{'id':'team','action':'disable'})
        member.team('status',expected=404)
        assert not member.api('/api/modules')['migration_allowed']
        member.api('/api/modules',{'id':'team','action':'enable'})
        assert member.team('status')['joined']
        member.api('/api/modules',{'id':'team','action':'disable'})
        stop(member.process)
        team_path.unlink()
        member.start()
        assert not member.api('/api/modules')['migration_allowed']
        member.api('/api/modules',{'id':'team','action':'uninstall'})
        assert member.api('/api/modules')['migration_allowed']
        assert all(not path.exists() for path in imported)
        assert Path(personal_auth).read_bytes()==personal_saved and member.auth.read_bytes()==original_auth
        checks.append('Signing out and uninstalling each delete team credential files and preserve personal credentials, even when the ownership file is missing during uninstall')
        verify_workspace_backup(stack,root,binaries/'empmod-team',vps,admin,join_browser if not args.no_browser else None,downloads)
        checks.append('Only the VPS maintainer can download the encrypted backup; a fresh VPS preserves identity, ledger and subscriptions and rejects a wrong password or existing destination')
        successor = next(row for row in vps.team('list_members')['members'] if row['name']=='mei')
        prior = admin.team('status')['identity']['member']['id']
        other_admin_login = request(vps_url+'/workspace/v1/login',
            {'name':'lab-admin','password':PASSWORD,'device':'f'*32})[1]
        other_admin_bearer = {'Authorization':'Bearer '+other_admin_login['token']}
        assert other_admin_login['member']['id'] == prior
        verify_vps_signout(root, vps, workspace, vps_url, 'mei', join_browser if not args.no_browser else None)
        checks.append('A VPS maintainer can switch accounts on one device without changing the VPS role or other sessions')
        assert admin.team('logout')['joined'] is False
        status, other_member = request(vps_url+'/workspace/v1/status',headers=other_admin_bearer)
        assert status == 200 and other_member['member']['role'] == 'team_admin'
        admin.login(workspace,'lab-admin')
        checks.append('A team administrator signs out on one device while another device keeps its session and administrator role')
        admin.team('handover',{'successor':prior},409)
        vps.team('handover',{'successor':successor['id']},403)
        if args.no_browser:
            assert not admin.team('handover',{'successor':successor['id']})['joined']
        else:
            join_browser.navigate(admin.url)
            wait_for(lambda: join_browser.execute('return document.getElementById("team_workspace_open")?.hidden === false;'))
            join_browser.execute('document.getElementById("team_workspace_open").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-choice=manage]");'))
            join_browser.execute('document.querySelector("[data-team-choice=manage]").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("[data-team-handover]");'))
            join_browser.execute('document.querySelector("[data-team-handover]").click();')
            wait_for(lambda: join_browser.execute('return !!document.querySelector("#team_successor:enabled");'))
            assert join_browser.execute('return document.getElementById("modal_submit").disabled;')
            join_browser.execute('const input=document.getElementById("team_successor");input.value=arguments[0];input.dispatchEvent(new Event("change"));document.getElementById("modal_submit").click();',successor['id'])
            wait_for(lambda: join_browser.execute('return !!document.getElementById("team_join_code");'))
        roles = {row['id']:row['role'] for row in vps.team('list_members')['members']}
        assert roles[prior]=='member' and roles[successor['id']]=='team_admin'
        status, successor_auth = request(vps_url+'/workspace/v1/login',
            {'name':'mei','password':PASSWORD,'device':'d'*32})
        assert status==200 and successor_auth['member']['role']=='team_admin'
        status, other_member = request(vps_url+'/workspace/v1/status',headers=other_admin_bearer)
        assert status == 200 and other_member['member']['role'] == 'member'
        checks.append('Role handover gives an active member the administrator role across devices and signs out the initiating device')
        report = {'checks':checks,'codex_version':version,'real_provider_calls':0,'browser':not args.no_browser}
        (args.output/'result.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps(report))


if __name__ == '__main__':
    main()
