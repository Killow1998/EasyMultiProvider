"""One Shadow workspace with production assets and disposable synthetic state."""
import contextlib
import json
import signal
import tempfile
import threading
from pathlib import Path
from candidate_web import candidate_frontend
from e2e_environment import request, start_backend

HERE = Path(__file__).resolve().parent
PASSWORD = 'fixture-import-password'


def fixture(stack, binary, root):
    source, target = root / 'source', root / 'target'
    source.mkdir(); target.mkdir()
    endpoints = []
    for directory, token in [(source, 'source-fixture-token'), (target, 'target-fixture-token')]:
        base, _, state, headers, upstream = start_backend(stack, binary, directory,
            {'codex_base_url': 'http://127.0.0.1:9/fixture-only','providers':[],'models':[],
             'codex_runtime_sources':['vscode']}, {'PATH':'/usr/bin:/bin',
              'HTTP_PROXY':'http://127.0.0.1:9','HTTPS_PROXY':'http://127.0.0.1:9',
              'ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'127.0.0.1,localhost'})
        headers['Origin'] = base
        auth = directory / 'codex_home/auth.json'
        auth.write_text(json.dumps({'tokens':{'account_id':'demo-fixture-account','access_token':token}}))
        status, config = request(base + '/api/config', headers=headers)
        assert status == 200
        catalog = Path(config['native_catalog_path']); assert catalog.is_relative_to(directory)
        catalog.parent.mkdir(parents=True, exist_ok=True)
        catalog.write_text(json.dumps({'models':[{'slug':slug,'display_name':name,
            'context_window':272000,'max_context_window':872000,'visibility':'list','base_instructions':'fixture'} for slug,name in [('demo-model','Sample model A'),('shared-model','Sample model B')]]}))
        config['native_hidden_models'] = ['demo-model'] if directory == source else []
        config['native_model_context_windows'] = {'demo-model':467368 if directory == source else 400000}
        status, result = request(base + '/api/config', config, headers)
        assert status == 200, result
        endpoints.append((base, state, headers, upstream))
    base, _, headers, _ = endpoints[0]
    status, confirmation = request(base + '/api/migration/export/confirm', {}, headers)
    assert status == 200
    status, exported = request(base + '/api/migration/export', {'confirmation':confirmation['confirmation'],
        'password':PASSWORD,'selections':[{'kind':'account','id':'@native','export_id':'demo','models':[{
            'id':'demo-model','selected':False,'name':'Sample model A','context_window':467368,'default_context':272000,'maximum':872000},
            {'id':'shared-model','selected':True,'name':'Sample model B','default_context':272000,'maximum':872000}]}]}, headers)
    assert status == 200, exported
    return endpoints[1], target, exported['bundle']


def preview(stack, backend, bundle, session, port=0):
    script = (HERE / 'preview.js').read_text().replace('__FIXTURE_BUNDLE__',json.dumps(bundle)).replace('__FIXTURE_PASSWORD__',json.dumps(PASSWORD))
    script += '\n' + (HERE / 'preview-review.js').read_text()
    head='<script>'+ (HERE / 'shadow-service-demo.js').read_text() + '</script><script>localStorage.setItem("emp_management_session_v1",'+json.dumps(session)+');</script>'
    web = HERE.parents[1] / 'crates/emp-app/web'
    def recovery_page():
        shim = '<script>' + (HERE / 'preview-recovery.js').read_text() + '</script>'
        return (web / 'recovery.html').read_text().replace('<script>', shim + '<script>', 1)
    return candidate_frontend(stack, backend, web, '<script>'+script+'</script>',port=port,index_head=head,
                              pages={'/recovery': recovery_page})


def serve(binary, port=4201, open_browser=False):
    with tempfile.TemporaryDirectory(prefix='emp-shadow-') as temp, contextlib.ExitStack() as stack:
        endpoint, _, bundle = fixture(stack, binary.resolve(), Path(temp))
        url, _ = preview(stack, endpoint[0], bundle, endpoint[2]['X-EMP-Session'], port)
        done = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_:done.set())
        signal.signal(signal.SIGINT, lambda *_:done.set())
        if open_browser:
            import webbrowser
            webbrowser.open(url)
        print(f'EMP unified Shadow: {url}/',flush=True)
        done.wait()
