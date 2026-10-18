"""One Shadow workspace with production assets and disposable synthetic state."""
import contextlib
import json
import signal
import sqlite3
import tempfile
import threading
import time
from pathlib import Path
from candidate_web import candidate_frontend
from e2e_environment import request, start_backend

HERE = Path(__file__).resolve().parent
PASSWORD = 'fixture-import-password'


def seed_performance(base, state, headers):
    assert request(base + '/api/calls', headers=headers)[0] == 200
    with sqlite3.connect(state / 'usage.sqlite3') as database:
        for index, legacy in enumerate((False, True)):
            started = time.time() - 20 - index * 20
            row = {'request_id': 'metrics-' + str(index), 'route': 'responses',
                'thread_id': 'legacy-example' if legacy else 'stream-example',
                'model_id': 'preview-speed', 'client_model': 'preview-speed',
                'state': 'completed', 'success': True, 'status': 200, 'error_class': 'none',
                'duration_ms': 10000, 'output_tokens': 200, 'reasoning_tokens': 40,
                'tokens_per_second': 20, 'performance_schema': 4 if legacy else 5,
                'started_at': started, 'finished_at': started + 10}
            if not legacy:
                row.update(first_content_ms=2000, generation_ms=2000, generation_tokens_per_second=80)
            database.execute('INSERT OR IGNORE INTO call_records '
                '(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts) '
                'VALUES (?,?,?,?,?,?,?,?,?,?)', (row['request_id'], 'responses', started, started + 10,
                    '', '@native', row['model_id'], row['thread_id'], 'completed', json.dumps(row)))
        for index, suffix in enumerate(('c','k','s')):
            owner = 'account:' + suffix
            database.execute('INSERT OR IGNORE INTO usage_account_labels VALUES (?,?,?,?,?)',
                ('subscription',owner,'sample-'+suffix,'Sample '+chr(65+index),''))
            database.execute('INSERT OR IGNORE INTO usage_events '
                '(id,observed_at,category,owner,model,service_tier,operation,input_tokens,output_tokens,'
                'cached_input_tokens,cost_nanos,rates) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)',
                ('service-color-'+suffix,time.time()-30,'subscription',owner,'preview-speed',
                 'default','responses',100*(index+1),20*(index+1),0,10000*(index+1),'{}'))


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
    base, state, headers, _ = endpoints[1]
    seed_performance(base, state, headers)
    return endpoints[1], target, exported['bundle']


def preview(stack, backend, bundle, session, port=0):
    def preview_script():
        script = (HERE / 'preview.js').read_text().replace('__FIXTURE_BUNDLE__',json.dumps(bundle)).replace('__FIXTURE_PASSWORD__',json.dumps(PASSWORD))
        script += '\n' + (HERE / 'preview-review.js').read_text()
        script += '\n' + (HERE / 'preview-actions.js').read_text()
        return '<script>' + script + '</script>'
    head='<script>'+ (HERE / 'shadow-service-demo.js').read_text() + '</script><script>localStorage.setItem("emp_management_session_v1",'+json.dumps(session)+');</script>'
    web = HERE.parents[1] / 'crates/emp-app/web'
    def recovery_page():
        shim = '<script>' + (HERE / 'preview-recovery.js').read_text() + '</script>'
        return (web / 'recovery.html').read_text().replace('<script>', shim + '<script>', 1)
    return candidate_frontend(stack, backend, web, preview_script,port=port,index_head=head,
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
