"""Exercise both Native-to-local-key choices with isolated credentials."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
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
    with tempfile.TemporaryDirectory(prefix='emp-native-key-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        browser = Browser(stack, root)
        for scenario in ('keep-new', 'keep-existing', 'remove'):
            target = root / scenario; target.mkdir()
            home = target / 'native'; home.mkdir()
            original = {'tokens': {'access_token': 'fixture-native-token', 'account_id': 'fixture-native'}}
            (home / 'auth.json').write_text(json.dumps(original))
            fixture = {'auto_enable_on_start': False,
                       'native_hidden_models': ['fixture-hidden'],
                       'native_model_context_windows': {'fixture-native': 64000},
                       'providers': [{'id': 'fixture', 'name': 'Fixture API', 'protocol': 'responses',
                           'base_url': 'http://127.0.0.1:9/v1', 'auth_mode': 'api_key', 'api_key': 'fixture'}],
                       'models': [{'id': 'fixture/model', 'provider': 'fixture', 'upstream_id': 'fixture'}]}
            env = {'CODEX_HOME': str(home), 'HTTP_PROXY': 'http://127.0.0.1:9',
                   'HTTPS_PROXY': 'http://127.0.0.1:9', 'ALL_PROXY': 'http://127.0.0.1:9',
                   'NO_PROXY': 'localhost,127.0.0.1'}
            base, url, state_dir, _ = start_backend(stack, args.emp.resolve(), target, fixture, env)

            def call(path, body=None):
                headers = {'X-EMP-Session': json.loads((state_dir / 'web-session.json').read_text())['token']}
                status, value = request(base + path, body, headers)
                assert status == 200, (path, status, value)
                return value

            if scenario != 'keep-new':
                other = original if scenario == 'keep-existing' else {'tokens': {'access_token': 'other-token', 'account_id': 'other'}}
                account = call('/api/accounts/import', {'name': 'Existing Other', 'auth_json': other})['account']
                call('/api/accounts/import', {**account, 'auth_json': other, 'hidden_models': ['other-hidden'],
                     'model_context_windows': {'other-model': 96000}, 'enabled': False})
            before = call('/api/config')['accounts']
            call('/api/integration/enable', {'confirm_reload': True})
            browser.navigate(url)
            browser.execute('setLanguage(arguments[0]);managementEvents.stop();clearInterval(quotaStateTimer);',
                            'en' if scenario == 'keep-existing' else 'zh-CN')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=account-details][data-id=\\"@native\\"]");'))
            browser.execute('document.querySelector("[data-ui-action=account-details][data-id=\\"@native\\"]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=native-use-local-key]");'))
            if scenario == 'keep-new':
                status, screenshot = request(browser.base + '/screenshot'); assert status == 200
                (args.output / 'account-details.png').write_bytes(base64.b64decode(screenshot['value']))
            browser.execute('document.querySelector("[data-ui-action=native-use-local-key]").click();')
            assert browser.execute('return document.querySelectorAll("[name=native_key_action]").length;') == 2
            if scenario == 'keep-new':
                browser.execute('document.getElementById("modal_back").click();')
                wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=native-use-local-key]");'))
                browser.execute('document.querySelector("[data-ui-action=native-use-local-key]").click();')
            if scenario in ('keep-new', 'keep-existing'):
                status, screenshot = request(browser.base + '/screenshot'); assert status == 200
                (args.output / ('switch-options-en.png' if scenario == 'keep-existing' else 'switch-options.png')).write_bytes(base64.b64decode(screenshot['value']))
                request(browser.base + '/window/rect', {'width': 480, 'height': 800})
                status, screenshot = request(browser.base + '/screenshot'); assert status == 200
                (args.output / ('switch-options-en-narrow.png' if scenario == 'keep-existing' else 'switch-options-narrow.png')).write_bytes(base64.b64decode(screenshot['value']))
                request(browser.base + '/window/rect', {'width': 1366, 'height': 900})
            choice = 'remove' if scenario == 'remove' else 'keep'
            browser.execute('document.querySelector("[name=native_key_action][value="+arguments[0]+"]").click();', choice)
            browser.execute('''const fetchOriginal=window.fetch;window.fetch=async function(...args){
                const response=await fetchOriginal(...args);
                if(String(args[0]).endsWith('/api/native/login/use-local-key')) window.fixtureSwitchResult=await response.clone().json();
                return response;};''')
            browser.execute('document.querySelector("[data-native-key-switch]").click();')
            wait_for(lambda: call('/api/config')['native_account']['auth_mode'] == 'local_key')
            assert json.loads((home / 'auth.json').read_text())['OPENAI_API_KEY'].startswith('emp-local-')
            after = call('/api/config')['accounts']
            assert len(after) == (1 if scenario == 'keep-new' else len(before)), (scenario, before, after)
            if before:
                for field in ('id', 'name', 'prefix', 'enabled', 'hidden_models', 'model_context_windows'):
                    assert after[0][field] == before[0][field], (scenario, field)
            else:
                assert after[0]['hidden_models'] == fixture['native_hidden_models']
                assert after[0]['model_context_windows'] == fixture['native_model_context_windows']
            assert call('/api/integration')['configuration']['state'] == 'emp_applied'
            wait_for(lambda: browser.execute('return !!window.fixtureSwitchResult;'))
            result = browser.execute('return window.fixtureSwitchResult;')
            assert result['restart_required'] and result['integration_updated'], result
            wait_for(lambda: browser.execute('return document.body.textContent.includes(tr("login.switch_done"));'))
            assert 'model_catalog_json' in (home / 'config.toml').read_text()
            checks.append(scenario + ': account choice, local key, active integration and restart notice')
    (args.output / 'result.json').write_text(json.dumps({'real_model_calls': 0, 'checks': checks}, indent=2) + '\n')
    print(json.dumps(checks))


if __name__ == '__main__':
    main()
