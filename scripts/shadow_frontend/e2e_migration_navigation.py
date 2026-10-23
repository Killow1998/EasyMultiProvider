"""Migration navigation against production assets and disposable real EMP backends."""
import argparse
import base64
import contextlib
import json
import tempfile
import threading
from pathlib import Path
from candidate_web import candidate_frontend
from e2e import Browser
from e2e_environment import request, start_backend, wait_for



def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--web-root', type=Path, default=Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-migration-navigation-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        source, target = root / 'source', root / 'target'
        source.mkdir(); target.mkdir()
        base, _, _, headers, upstream = start_backend(stack, args.emp.resolve(), source)
        target_base, target_url, _, _, _ = start_backend(stack, args.emp.resolve(), target,
            {'codex_base_url': base + '/fixture-subscription'})
        headers['Origin'] = base
        status, confirmation = request(base + '/api/migration/export/confirm', {}, headers)
        assert status == 200
        selection = [{'kind': 'provider', 'id': 'demo', 'name': 'Imported fixture', 'models': [{'id': 'demo/model', 'selected': True}]}]
        status, exported = request(base + '/api/migration/export', {'confirmation': confirmation['confirmation'],
            'password': 'fixture-migration-password', 'selections': selection}, headers, timeout=90)
        assert status == 200
        assert base64.b64decode(exported['bundle']).startswith(b'EMP-MIGRATION')
        frontend, server = candidate_frontend(stack, target_base, args.web_root.resolve())
        browser = Browser(stack, root)
        browser.navigate(target_url.replace(target_base, frontend))
        before = (target / 'config.json').read_bytes()
        fill = '''const input=document.getElementById('shadow_migration_file'),transfer=new DataTransfer();
          transfer.items.add(new File([Uint8Array.from(atob(arguments[0]),c=>c.charCodeAt(0))],'fixture.emp'));
          input.files=transfer.files;input.dispatchEvent(new Event('input',{bubbles:true}));
          const password=document.getElementById('shadow_import_password');password.value='fixture-migration-password';
          password.dispatchEvent(new Event('input',{bubbles:true}));'''
        for language in ['zh-CN', 'en']:
            for theme in ['light', 'dark']:
                request(browser.base + '/window/rect', {'width': 640 if theme == 'dark' else 1280, 'height': 900})
                browser.execute('for(const [id,value] of [["language_select",arguments[0]],["theme_select",arguments[1]]]){const select=document.getElementById(id);select.value=value;select.dispatchEvent(new Event("change",{bubbles:true}));}document.getElementById("shadow_migration_open").click();', language, theme)
                # Language selection intentionally saves the error-language preference.
                wait_for(lambda: json.loads((target / 'config.json').read_text()).get('language') == language)
                before = (target / 'config.json').read_bytes()
                browser.execute('document.querySelector("[data-migration-mode=import]").click();')
                browser.execute(fill, exported['bundle'])
                browser.execute('document.getElementById("modal_back").click();document.querySelector("[data-migration-mode=export]").click();document.getElementById("shadow_export_password").value="export-draft-password";document.getElementById("shadow_export_password_confirm").value="export-draft-password";document.getElementById("migration_include_context").click();document.getElementById("modal_back").click();document.querySelector("[data-migration-mode=tunnel]").click();document.getElementById("lan_code").value="x".repeat(49);')
                browser.escape()
                browser.execute('document.querySelector("[data-migration-mode=export]").click();')
                assert browser.execute('return document.getElementById("shadow_export_password").value==="export-draft-password" && document.getElementById("shadow_export_password_confirm").value==="export-draft-password" && !document.getElementById("migration_include_context").checked;')
                browser.escape()
                browser.execute('document.querySelector("[data-migration-mode=tunnel]").click();')
                assert browser.execute('return document.getElementById("lan_code").value==="x".repeat(49);')
                browser.escape()
                browser.execute('document.querySelector("[data-migration-mode=import]").click();')
                assert browser.execute('return document.getElementById("shadow_migration_file").files[0].name==="fixture.emp" && document.getElementById("shadow_import_password").value==="fixture-migration-password";')
                assert browser.execute('return document.documentElement.scrollWidth<=document.documentElement.clientWidth && document.getElementById("shadow_migration").scrollWidth<=document.getElementById("shadow_migration").clientWidth;')
                if language == 'zh-CN':
                    status, screenshot = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name(f'migration-guidance-{theme}-20261014.png').write_bytes(base64.b64decode(screenshot['value']))
                browser.execute('document.querySelector("[data-migration-demo]").click();')
                wait_for(lambda: browser.execute('return !!document.querySelector(".migration-demo-note") && !document.getElementById("modal_submit").disabled;'))
                assert browser.execute('return !document.getElementById("modal_back").hidden;')
                browser.execute('document.getElementById("modal_back").click();')
                assert browser.execute('return !document.querySelector(".migration-demo-note") && document.getElementById("shadow_migration_file").files[0].name==="fixture.emp" && document.getElementById("shadow_import_password").value==="fixture-migration-password";')
                assert (target / 'config.json').read_bytes() == before
                browser.execute('document.querySelector("[data-migration-demo]").click();')
                wait_for(lambda: browser.execute('return !!document.querySelector(".migration-demo-note") && !document.getElementById("modal_submit").disabled;'))
                assert browser.execute('return document.getElementById("modal_close").hidden;')
                browser.escape()
                assert browser.execute('return !document.querySelector(".migration-demo-note") && document.getElementById("shadow_migration_file").files[0].name==="fixture.emp" && document.getElementById("shadow_import_password").value==="fixture-migration-password";')
                browser.escape()
                wait_for(lambda:browser.execute('return !!document.getElementById("migration_chooser");'))
                browser.execute('document.getElementById("modal_close").click();')
                assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");')
                assert (target / 'config.json').read_bytes() == before
        checks.append('Both languages/themes: chooser, forms and preview return to their immediate parent; import file/password, export password/options and tunnel code survive switching; root X closes without writes')
        browser.execute('document.getElementById("shadow_migration_open").click();document.querySelector("[data-migration-mode=import]").click();')
        browser.execute(fill, exported['bundle'])
        server.preview_gate = threading.Event()
        browser.execute('document.querySelector("[data-migration-decode]").click();')
        wait_for(server.preview_started.is_set, seconds=90)
        assert browser.execute('return document.getElementById("modal_submit").disabled;')
        browser.execute('document.getElementById("modal_back").click();')
        server.preview_gate.set()
        browser.execute('document.querySelector("[data-migration-decode]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-migration-name]") && !document.getElementById("modal_submit").disabled;'), seconds=90)
        assert (target / 'config.json').read_bytes() == before
        checks.append('Back cancels pending preview; reopened encrypted package becomes importable without stale state or writes')
        # A response that never arrives must leave a usable retry, not a permanent wait.
        gate = threading.Event()
        stack.callback(gate.set)
        server.preview_gate = gate
        server.preview_started.clear()
        checkbox = 'document.getElementById("migration_include_rename")'
        browser.execute(checkbox + '.click();')
        wait_for(server.preview_started.is_set)
        assert browser.execute('return document.getElementById("modal_status").textContent;') == 'Preparing the import preview. Please wait…'
        wait_for(lambda: browser.execute('return document.getElementById("modal_submit").textContent === "Retry preview" && !document.getElementById("modal_submit").disabled;'), seconds=35)
        assert browser.execute('return document.getElementById("modal_status").textContent.includes("timed out");')
        assert browser.execute('return document.getElementById("shadow_migration").getAttribute("aria-busy")==="false";')
        assert browser.execute('return !document.getElementById("migration_include_rename").checked;')
        assert (target / 'config.json').read_bytes() == before
        status, screenshot = request(browser.base + '/screenshot')
        assert status == 200
        args.output.with_name('migration-preview-timeout.png').write_bytes(base64.b64decode(screenshot['value']))
        # Keep the old request delayed until a new preview has completed.
        server.preview_gate = None
        browser.execute('document.getElementById("modal_submit").click();')
        assert browser.execute('return document.getElementById("modal_submit").disabled;')
        wait_for(lambda: browser.execute('return document.getElementById("modal_submit").textContent === "Import" && !document.getElementById("modal_submit").disabled;'))
        gate.set()
        assert browser.execute('return !document.getElementById("migration_include_rename").checked && document.getElementById("modal_status").textContent === "";')
        assert (target / 'config.json').read_bytes() == before
        checks.append('Stalled preview times out; Retry preserves selections and succeeds in the same backend without importing or restarting')
        browser.execute('document.querySelector("[data-migration-all=false]").click();')
        assert browser.execute('return document.getElementById("modal_submit").disabled;')
        browser.execute('document.querySelector("[data-migration-all=true]").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("modal_submit").disabled;'))
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        stored = json.loads((target / 'config.json').read_text())
        assert any(provider['name'] == 'Imported fixture' for provider in stored['providers'])
        assert upstream.calls == 0
        checks.append('Selection changes gate Import; one explicit Import saves the selected service through real backend')
        # Same-account roles share one card, with explicit choices only for differing preferences.
        native_path = target / 'codex_home/auth.json'
        identity_token = 'fixture.' + base64.urlsafe_b64encode(json.dumps({
            'https://api.openai.com/auth': {'chatgpt_account_id': 'native-fixture', 'chatgpt_user_id': 'fixture-user'}
        }).encode()).decode().rstrip('=') + '.signature'
        native_auth = {'tokens': {'account_id': 'native-fixture', 'access_token': 'native-fixture-token', 'id_token': identity_token}}
        native_path.write_text(json.dumps(native_auth))
        native_bytes = native_path.read_bytes()
        target_headers = {'X-EMP-Session': json.loads((target / 'state/web-session.json').read_text())['token'], 'Origin': target_base}
        status, configured = request(target_base + '/api/config', headers=target_headers)
        assert status == 200
        catalog = Path(configured['native_catalog_path'])
        assert catalog.is_relative_to(target)
        catalog.parent.mkdir(parents=True, exist_ok=True)
        catalog.write_text(json.dumps({'models': [{'slug': 'gpt-fixture', 'display_name': 'Fixture model',
            'context_window': 272000, 'max_context_window': 872000, 'visibility': 'list', 'base_instructions': 'fixture'}]}))
        configured['native_model_context_windows'] = {'gpt-fixture': 400000}
        status, result = request(target_base + '/api/config', configured, target_headers)
        assert status == 200, result
        status, imported = request(target_base + '/api/accounts/import', {'id': 'merged-fixture', 'prefix': 'merged-fixture', 'name': 'Imported same account',
            'auth_json': {'tokens': {'account_id': 'native-fixture', 'access_token': 'rotated-fixture-token', 'id_token': identity_token}},
            'hidden_models': ['gpt-fixture'], 'model_context_windows': {'gpt-fixture': 467368}}, target_headers)
        assert status == 200, imported
        source_id, source_auth_path = imported['account']['id'], json.loads((target / 'config.json').read_text())['accounts'][0]['auth_file']
        browser.navigate(frontend + '/')
        assert browser.execute('return document.querySelectorAll(".service-account").length===1 && !!document.querySelector(`[data-service="account:@native"]`);')
        browser.execute('document.querySelector(`[data-service="account:@native"] [data-ui-action="account-details"]`).click();')
        browser.execute('document.querySelector(`[data-ui-action="account-edit"]`).click();')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-preference-kind]").length===2;'))
        assert browser.execute('return document.getElementById("modal_submit").disabled;')
        before = (target / 'config.json').read_bytes()
        status, screenshot = request(browser.base + '/screenshot')
        assert status == 200
        args.output.with_name('subscription-preference-conflicts-20261014.png').write_bytes(base64.b64decode(screenshot['value']))
        for kind, value in [('visibility', '1'), ('context', '0')]:
            browser.execute('const select=document.querySelector("[data-preference-kind="+arguments[0]+"]");select.value=arguments[1];select.dispatchEvent(new Event("change",{bubbles:true}));', kind, value)
        assert (target / 'config.json').read_bytes() == before
        assert browser.execute('return !document.getElementById("modal_submit").disabled;')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        merged = json.loads((target / 'config.json').read_text())
        peer = next(account for account in merged['accounts'] if account['id'] == source_id)
        assert merged['native_hidden_models'] == peer['hidden_models'] == ['gpt-fixture']
        assert merged['native_model_context_windows'] == peer['model_context_windows'] == {'gpt-fixture': 400000}
        assert peer['auth_file'] == source_auth_path and native_path.read_bytes() == native_bytes
        assert upstream.calls == 0
        checks.append('Verified native/imported identity shares one service; only visibility/context conflicts need choices; Save preserves credentials and references')
        for identifier in ['group-fixture', 'group-backup']:
            status, result = request(target_base + '/api/accounts/import', {'id': identifier, 'prefix': identifier,
                'name': identifier, 'auth_json': {'tokens': {'account_id': 'other-fixture-owner', 'access_token': identifier + '-token'}}}, target_headers)
            assert status == 200, result
        browser.navigate(frontend + '/')
        assert browser.execute('return document.querySelectorAll(".service-account").length===2 && !document.querySelector(`[data-service="account:group-backup"]`);')
        browser.execute('document.querySelector(`[data-service="account:group-fixture"] [data-ui-action="account-details"]`).click();')
        status, element = request(browser.base + '/element', {'using': 'css selector', 'value': '[data-ui-action="account-remove"]'})
        assert status == 200, element
        identifier = element['value']['element-6066-11e4-a52e-4f735466cecf']
        status, result = request(browser.base + '/element/' + identifier + '/click', {})
        assert status == 200, result
        status, result = request(browser.base + '/alert/accept', {})
        assert status == 200, result
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && document.querySelectorAll(".service-account").length===1;'))
        remaining = json.loads((target / 'config.json').read_text())['accounts']
        assert [account['id'] for account in remaining] == [source_id]
        assert native_path.read_bytes() == native_bytes and upstream.calls == 0
        checks.append('Removing a merged imported service through the browser removes all its verified aliases and retains Native credentials')
        records = [json.loads(line) for log in (target / 'state/logs').glob('*.jsonl') for line in log.read_text().splitlines()]
        previews = [row['fields'] for row in records if row['event'] == 'operation_finished' and row['fields'].get('operation') == 'migration_preview']
        assert previews and all(row['outcome'] == 'completed' for row in previews), previews
        assert all([stage['stage'] for stage in row['stages']] == ['read_configuration', 'resolve_preferences', 'build_preview'] for row in previews)
        assert 'fixture-migration-password' not in json.dumps(records) and exported['bundle'] not in json.dumps(records)
        checks.append('Backend preview receipts identify processing stages and completion without the migration password or encrypted package')
        args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'real_model_requests': 0,
            'temporary_backend_only': True, 'production_assets': True}, indent=2) + '\n')
        print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
