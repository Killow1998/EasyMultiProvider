"""Exercise both Native-to-local-key choices with isolated credentials."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from e2e_refinements import Browser
from local_backend import request, start_backend, wait_for


def delayed_login_save(browser, path, action, expected_mode):
    browser.execute('''window.auditNativeFetch=window.fetch;
      window.fetch=(path,options)=>String(path)===arguments[0]?
        new Promise(resolve=>window.releaseNativeSave=()=>window.auditNativeFetch(path,options).then(resolve)):
        window.auditNativeFetch(path,options);''', path)
    browser.execute(action)
    wait_for(lambda: browser.execute('return typeof window.releaseNativeSave==="function";'))
    browser.execute('openModal("Replacement","<div id=native_save_replacement></div>","",null);window.releaseNativeSave();')
    wait_for(lambda: browser.execute('return state.native_account.auth_mode===arguments[0];', expected_mode))
    assert browser.execute('return !!document.getElementById("native_save_replacement") && !document.getElementById("modal_backdrop").classList.contains("hidden");')
    browser.execute('window.fetch=window.auditNativeFetch;delete window.releaseNativeSave;')


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
            native_catalog = target / 'native-models.json'
            native_catalog.write_text(json.dumps({'models': [{
                'slug': 'gpt-6.1-sol', 'display_name': 'Sol', 'visibility': 'list',
                'supported_in_api': True, 'context_window': 128000,
            }]}))
            fixture = {'auto_enable_on_start': False,
                       'native_catalog_path': str(native_catalog),
                       'native_hidden_models': ['fixture-hidden'],
                       'native_model_context_windows': {'fixture-native': 64000},
                       'providers': [{'id': 'fixture', 'name': 'Fixture API', 'protocol': 'responses',
                           'base_url': 'http://127.0.0.1:9/v1', 'auth_mode': 'api_key', 'api_key': 'fixture'}],
                       'models': [{'id': 'fixture/model', 'provider': 'fixture', 'upstream_id': 'fixture'}]}
            if scenario == 'keep-new':
                fixture['providers'][0]['name'] = 'Native'
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
            assert any(model['id'] == 'gpt-6.1-sol' for model in call('/api/config')['catalog_models'])
            browser.navigate(url)
            browser.execute('setLanguage(arguments[0]);managementEvents.stop();clearInterval(quotaStateTimer);',
                            'en' if scenario == 'keep-existing' else 'zh-CN')
            if scenario == 'keep-new':
                browser.execute('''openServiceChooser();document.querySelector('[data-ui-action=account-import-auth]').click();
                    document.getElementById('modal_account_name').value='native';
                    document.getElementById('modal_account_auth').value=JSON.stringify({tokens:{account_id:'different',access_token:'fixture'}});
                    document.getElementById('modal_submit').click();''')
                wait_for(lambda: browser.execute('return document.getElementById("modal_status").textContent.includes("名称已被其他服务或旧会话使用");'))
                assert browser.execute('return document.getElementById("modal_account_name").value === "native";')
                assert call('/api/config')['accounts'] == []
                browser.execute('closeModal();')
                checks.append('Duplicate subscription names show a readable error and retain the import form')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-ui-action=account-details][data-id=\\"@native\\"]");'))
            browser.execute('document.querySelector("[data-ui-action=account-details][data-id=\\"@native\\"]").click();')
            wait_for(lambda: browser.execute('return !!document.getElementById("account_usage");'))
            if scenario == 'keep-new':
                status, screenshot = request(browser.base + '/screenshot'); assert status == 200
                (args.output / 'account-details.png').write_bytes(base64.b64decode(screenshot['value']))
            browser.execute('closeModal();document.querySelector("[data-ui-action=native-login]").click();document.querySelector("[data-native-method=local_key]").click();')
            assert browser.execute('return document.querySelectorAll("[name=native_key_action]").length;') == 2
            if scenario == 'keep-new':
                browser.execute('document.getElementById("modal_back").click();')
                wait_for(lambda: browser.execute('return !!document.querySelector("[data-native-method=local_key]");'))
                browser.execute('document.querySelector("[data-native-method=local_key]").click();')
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
                assert after[0]['name'] == 'Native 2'
            assert call('/api/integration')['configuration']['state'] == 'emp_applied'
            local = call('/api/config')
            assert all(model['id'] != 'gpt-6.1-sol' for model in local['catalog_models'])
            assert any(model['id'] == 'fixture/model' for model in local['catalog_models'])
            key = json.loads((home / 'auth.json').read_text())['OPENAI_API_KEY']
            status, catalog = request(base + '/v1/models?client_version=0.167.0',
                                      headers={'Authorization': 'Bearer ' + key})
            assert status == 200, catalog
            native = next(model for model in catalog['models'] if model['slug'] == 'gpt-6.1-sol')
            assert native['visibility'] == 'hide', native
            if scenario == 'keep-new':
                other_route = after[0]['prefix'] + '/gpt-6.1-sol'
                assert any(model['id'] == other_route for model in local['catalog_models'])
                group = {'id': 'auto/local-key-fixture', 'name_prefix': 'auto', 'policy': 'priority',
                         'sources': ['gpt-6.1-sol', other_route]}
                grouped = call('/api/config', {**local, 'model_groups': [group]})
                assert any(model['id'] == group['id'] for model in grouped['catalog_models'])
                assert all(model['id'] != 'gpt-6.1-sol' for model in grouped['catalog_models'])
                status, catalog = request(base + '/v1/models?client_version=0.167.0',
                                          headers={'Authorization': 'Bearer ' + key})
                assert status == 200
                assert any(model.get('_emp_aggregate') and model.get('visibility', 'list') == 'list'
                           for model in catalog['models'])
            checks.append(scenario + ': local-key catalog hides unusable Native while retaining other routes')
            wait_for(lambda: browser.execute('return !!window.fixtureSwitchResult;'))
            result = browser.execute('return window.fixtureSwitchResult;')
            assert result['restart_required'] and result['integration_updated'], result
            wait_for(lambda: browser.execute('return document.body.textContent.includes(tr("login.switch_done"));'))
            assert 'model_catalog_json' in (home / 'config.toml').read_text()
            checks.append(scenario + ': account choice, local key, active integration and restart notice')
            browser.execute('nativeLogin.manage();document.querySelector("[data-native-method=api_key]").click();document.getElementById("native_api_key").value="sk-fixture-delayed-save";')
            delayed_login_save(browser, '/api/native/login/use-api-key',
                               'document.getElementById("modal_submit").click();', 'api_key')
            assert json.loads((home / 'auth.json').read_text())['OPENAI_API_KEY'] == 'sk-fixture-delayed-save'
            assert any(model['id'] == 'gpt-6.1-sol' for model in call('/api/config')['catalog_models'])
            if scenario == 'keep-existing':
                browser.execute('nativeLogin.manage();document.querySelector("[data-native-method=other]").click();')
                delayed_login_save(browser, '/api/native/login/use-account',
                                   'document.getElementById("modal_submit").click();', 'subscription')
                assert any(model['id'] == 'gpt-6.1-sol' for model in call('/api/config')['catalog_models'])
            browser.execute('nativeLogin.openLocalKey();')
            delayed_login_save(browser, '/api/native/login/use-local-key',
                               'document.querySelector("[data-native-key-switch]").click();', 'local_key')
            assert json.loads((home / 'auth.json').read_text())['OPENAI_API_KEY'].startswith('emp-local-')
            checks.append(scenario + ': delayed credential and local-key saves commit without closing a replacement dialog')
        target = root / 'signed-out'; target.mkdir()
        home = target / 'native'; home.mkdir()
        native_catalog = target / 'native-models.json'
        native_catalog.write_text(json.dumps({'models': [{'slug': 'gpt-6.1-sol', 'visibility': 'list',
            'supported_in_api': True, 'context_window': 128000}]}))
        fixture = {'auto_enable_on_start': False, 'native_catalog_path': str(native_catalog),
                   'providers': [{'id': 'fixture', 'name': 'Fixture API', 'protocol': 'responses',
                       'base_url': 'http://127.0.0.1:9/v1', 'auth_mode': 'api_key', 'api_key': 'fixture'}],
                   'models': [{'id': 'fixture/model', 'provider': 'fixture', 'upstream_id': 'fixture'}]}
        base, _, _, headers = start_backend(stack, args.emp.resolve(), target, fixture, {
            'CODEX_HOME': str(home), 'HTTP_PROXY': 'http://127.0.0.1:9',
            'HTTPS_PROXY': 'http://127.0.0.1:9', 'ALL_PROXY': 'http://127.0.0.1:9',
            'NO_PROXY': 'localhost,127.0.0.1'})
        status, result = request(base + '/api/integration/enable', {'confirm_reload': True}, headers)
        assert status == 200, result
        assert json.loads((home / 'auth.json').read_text())['emp_local_connection'] is True
        catalog = json.loads((home / 'easy-multi-provider/catalog.json').read_text())
        assert next(model for model in catalog['models'] if model['slug'] == 'gpt-6.1-sol')['visibility'] == 'hide'
        checks.append('First activation without a login writes a local-key catalog with Native hidden')
    (args.output / 'result.json').write_text(json.dumps({'real_model_calls': 0, 'checks': checks}, indent=2) + '\n')
    print(json.dumps(checks))


if __name__ == '__main__':
    main()
