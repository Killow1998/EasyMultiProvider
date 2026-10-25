"""Device-code destination choices against production assets and synthetic accounts."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import shutil
import tempfile

from e2e_refinements import Browser
from local_backend import request, start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-login-destination-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        bin_dir = root / 'bin'; bin_dir.mkdir()
        shutil.copy(Path(__file__).with_name('fixtures') / 'device-codex.py', bin_dir / 'codex')
        (bin_dir / 'codex').chmod(0o700)
        control = bin_dir / 'login-control'
        browser = Browser(stack, root)
        for mode, language in [('local_key', 'zh-CN'), ('subscription', 'en')]:
            control.write_text('pending')
            target = root / mode; target.mkdir()
            home = target / 'native'; home.mkdir()
            if mode == 'subscription':
                (home / 'auth.json').write_text(json.dumps({'tokens': {
                    'access_token': 'existing-native-token', 'account_id': 'existing-native'}}))
            base, url, state_dir, _ = start_backend(stack, args.emp.resolve(), target,
                {'auto_enable_on_start': False,
                    'providers': [{'id': 'fixture', 'name': 'Fixture API', 'protocol': 'responses',
                        'base_url': 'http://127.0.0.1:9/v1', 'auth_mode': 'api_key', 'api_key': 'synthetic-only'}],
                    'models': [{'id': 'fixture/model', 'provider': 'fixture', 'upstream_id': 'fixture-model'}]},
                {'CODEX_HOME': str(home),
                    'PATH': str(bin_dir)+':/usr/bin:/bin', 'HTTP_PROXY': 'http://127.0.0.1:9',
                    'HTTPS_PROXY': 'http://127.0.0.1:9', 'ALL_PROXY': 'http://127.0.0.1:9', 'NO_PROXY': 'localhost,127.0.0.1'})

            def call(path, body=None):
                headers = {'X-EMP-Session': json.loads((state_dir / 'web-session.json').read_text())['token']}
                status, value = request(base+path, body, headers)
                assert status == 200, (path, status, value)
                return value

            if mode == 'local_key':
                call('/api/integration/enable', {'confirm_reload': True})
            original = (home / 'auth.json').read_bytes()
            browser.navigate(url)
            browser.execute('setLanguage(arguments[0]);managementEvents.stop();clearInterval(quotaStateTimer);', language)

            def open_device(destination):
                browser.execute('''closeModal();if(arguments[0]==='local_key')nativeLogin.open();
                    else {openServiceChooser();document.querySelector('[data-ui-action=account-import-device]').click();}''', mode)
                wait_for(lambda:browser.execute('return !document.getElementById("device_login_target").disabled;'))
                browser.execute('''const select=document.getElementById('device_login_target');select.value=arguments[0];select.dispatchEvent(new Event('change',{bubbles:true}));''', destination)
                wait_for(lambda:browser.execute('return !document.querySelector("[data-native-login-start]").disabled;'))
                assert browser.execute('''const remove=document.querySelector('[data-native-key-remove]');
                    return !remove || remove.hidden===(arguments[0]==='other');''', destination)

            def start_login(destination):
                path = '/api/accounts/login/' if destination == 'other' else '/api/native/login/'
                browser.execute('document.querySelector("[data-native-login-start]").click();')
                try:
                    wait_for(lambda:browser.execute('return !!document.querySelector(".device-code");'), seconds=50)
                except TimeoutError:
                    raise AssertionError({'status': browser.execute('return document.getElementById("modal_status").textContent;'),
                        'login': call(path+'status', {})}) from None
                assert browser.execute('return document.getElementById("device_login_target").disabled;')
                assert browser.execute('const remove=document.querySelector("[data-native-key-remove]");return !remove || remove.disabled;')
                assert call(path+'status', {})['status'] == 'pending'
                return path

            open_device('other')
            browser.execute('document.getElementById("device_account_name").value="Other fixture";')
            path = start_login('other')
            assert (home / 'auth.json').read_bytes() == original
            control.write_text('complete')
            wait_for(lambda:len(call('/api/config')['accounts']) == 1, seconds=30)
            wait_for(lambda:browser.execute('return state.accounts.length===1;'))
            assert (home / 'auth.json').read_bytes() == original
            assert not call(path+'status', {})['restart_required']
            checks.append(f'{mode}: Other device login adds a subscription and preserves Native bytes')

            control.write_text('pending')
            open_device('native')
            request(browser.base+'/window/rect', {'width': 480, 'height': 900})
            status, screenshot = request(browser.base+'/screenshot'); assert status == 200
            (args.output / f'device-{mode}.png').write_bytes(base64.b64decode(screenshot['value']))
            path = start_login('native')
            browser.execute('document.querySelector("[data-login-cancel]").click();')
            wait_for(lambda:call(path+'status', {})['status'] == 'cancelled')
            assert (home / 'auth.json').read_bytes() == original
            wait_for(lambda:browser.execute('return !document.querySelector("[data-native-login-start]").disabled;'))
            start_login('native')
            control.write_text('complete')
            wait_for(lambda:call(path+'status', {})['status'] == 'completed', seconds=30)
            saved = (home / 'auth.json').read_bytes()
            assert json.loads(saved)['tokens']['access_token'] == 'fixture-access'
            assert call(path+'status', {})['restart_required']
            assert len(call('/api/config')['accounts']) == 1
            checks.append(f'{mode}: Cancel preserves Native; explicit replacement changes Native and requires restart')

            imported = {'tokens': {'account_id': 'auth-file-fixture', 'access_token': 'file-token'}}
            browser.execute('''closeModal();openServiceChooser();document.querySelector('[data-ui-action=account-import-auth]').click();
                document.getElementById('modal_account_name').value='File account';
                const input=document.getElementById('a_auth_json'),transfer=new DataTransfer();
                transfer.items.add(new File([JSON.stringify(arguments[0])],'auth.json',{type:'application/json'}));
                input.files=transfer.files;input.dispatchEvent(new Event('change',{bubbles:true}));''', imported)
            wait_for(lambda:browser.execute('return !!document.getElementById("modal_account_auth").value;'))
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda:len(call('/api/config')['accounts']) == 2)
            wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
            browser.execute('''openServiceChooser();document.querySelector('[data-ui-action=account-import-auth]').click();
                document.getElementById('modal_account_name').value='Pasted account';
                document.getElementById('modal_account_auth').value=JSON.stringify(arguments[0]);
                document.getElementById('modal_submit').click();''', imported)
            wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
            assert len(call('/api/config')['accounts']) == 2
            assert (home / 'auth.json').read_bytes() == saved
            checks.append(f'{mode}: auth.json file and paste use Other, deduplicate and preserve Native')
    report = {'real_model_calls': 0, 'checks': checks}
    (args.output / 'result.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
