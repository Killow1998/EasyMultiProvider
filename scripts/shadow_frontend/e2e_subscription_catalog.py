"""Subscription picker visibility and context settings with disposable state."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-catalog-origin-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        home = root / 'native'
        home.mkdir()
        (home / 'auth.json').write_text(json.dumps({'tokens': {
            'account_id': 'fixture-native', 'access_token': 'fixture-only'}}))
        catalog = root / 'models.json'
        catalog.write_text(json.dumps({'models': [
            {'slug': 'gpt-6-luna', 'display_name': 'GPT-6-Luna', 'visibility': 'list',
             'context_window': 272000, 'supported_in_api': True},
            {'slug': 'gpt-6.1-sol', 'display_name': 'GPT-6.1-Sol', 'visibility': 'list',
             'context_window': 272000, 'supported_in_api': True, '_emp_catalog_retained': True}]}))
        base, _, _, headers, _ = start_backend(stack, args.emp.resolve(), root,
            {'auto_enable_on_start': False, 'native_catalog_path': str(catalog),
             'native_model_context_windows': {'gpt-6.1-sol': 180000},
             'codex_base_url': 'http://127.0.0.1:9/v1'}, {'CODEX_HOME': str(home)})
        status, options = request(base + '/api/accounts/%40native/models', headers=headers)
        assert status == 200, options
        assert [model['id'] for model in options['models']] == ['gpt-6-luna']
        assert all('catalog_origin' not in model for model in options['models'])
        status, picker = request(base + '/v1/models?client_version=0.169.1', headers=headers)
        assert status == 200, picker
        legacy = next(model for model in picker['models'] if model['slug'] == 'gpt-6.1-sol')
        assert legacy['visibility'] == 'hide' and legacy['supported_in_api'] is True
        assert '_emp_catalog_retained' not in legacy
        head = '<script>localStorage.setItem("emp_management_session_v1",' + json.dumps(headers['X-EMP-Session']) + ')</script>'
        url, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web', index_head=head)
        browser = Browser(stack, root)
        assert request(browser.base + '/window/rect', {'width': 1280, 'height': 900})[0] == 200
        browser.navigate(url)
        browser.execute('editAccount(state.native_account.id);')
        wait_for(lambda: browser.execute("return !!document.querySelector('[data-model=\"gpt-6-luna\"]');"))
        status, capture = request(browser.base + '/screenshot')
        assert status == 200, capture
        (args.output / 'catalog.png').write_bytes(base64.b64decode(capture['value']))
        browser.execute('''
            document.querySelector('[data-model="gpt-6-luna"]').value='100000';
            document.getElementById('modal_submit').click();
        ''')
        wait_for(lambda: browser.execute('return state.native_model_context_windows?.["gpt-6-luna"]===100000;'))
        status, config = request(base + '/api/config', headers=headers)
        assert status == 200, config
        assert config['native_model_context_windows'] == {'gpt-6-luna': 100000, 'gpt-6.1-sol': 180000}
        checks = ['account_models_api', 'historical_route_hidden_from_picker', 'context_settings_save']
        (args.output / 'report.json').write_text(json.dumps({'checks': checks, 'real_model_requests': 0}, indent=2))
        print(json.dumps({'checks': checks, 'real_model_requests': 0}))


if __name__ == '__main__':
    main()
