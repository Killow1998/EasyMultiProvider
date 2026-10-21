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
METRICS_THREAD = '11111111-1111-4111-8111-111111111111'
METRICS_TURN = '22222222-2222-4222-8222-222222222222'


def seed_performance(base, state, headers):
    assert request(base + '/api/calls', headers=headers)[0] == 200
    with sqlite3.connect(state / 'usage.sqlite3') as database:
        for index, legacy in enumerate((False, True)):
            started = time.time() - 20 - index * 20
            row = {'request_id': 'metrics-' + str(index), 'route': 'responses',
                'thread_id': 'legacy-example' if legacy else METRICS_THREAD,
                'turn_id': METRICS_TURN if not legacy else '',
                'model_id': 'preview-speed', 'client_model': 'preview-speed',
                'state': 'completed', 'success': True, 'status': 200, 'error_class': 'none',
                'duration_ms': 10000, 'output_tokens': 200, 'reasoning_tokens': 40,
                'tokens_per_second': 20, 'performance_schema': 4 if legacy else 5,
                'started_at': started, 'finished_at': started + 10}
            if not legacy:
                row.update(first_content_ms=2000, generation_ms=2000, generation_tokens_per_second=80)
            row['session_id'] = row['thread_id']
            database.execute('INSERT OR IGNORE INTO call_records '
                '(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts) '
                'VALUES (?,?,?,?,?,?,?,?,?,?)', (row['request_id'], 'responses', started, started + 10,
                    '', '@native', row['model_id'], row['thread_id'], 'completed', json.dumps(row)))
        started = time.time() - 60
        row = {'request_id':'metrics-burst','route':'responses','model_id':'preview-speed',
            'client_model':'preview-speed','state':'completed','success':True,'status':200,
            'error_class':'none','duration_ms':11966,'first_content_ms':11916,
            'performance_schema':5,'generation_ms':6,'generation_tokens_per_second':11500,
            'output_tokens':100,'reasoning_tokens':31,'tokens_per_second':8.36,
            'started_at':started,'finished_at':started+11.966}
        database.execute('INSERT OR IGNORE INTO call_records VALUES (?,?,?,?,?,?,?,?,?,?,?)',
            ('metrics-burst','responses',started,started+11.966,'','@native','preview-speed','',
             'completed',json.dumps(row),None))
        for index, suffix in enumerate(('c','k','s')):
            owner = 'account:' + suffix
            database.execute('INSERT OR IGNORE INTO usage_account_labels VALUES (?,?,?,?,?)',
                ('subscription',owner,'sample-'+suffix,'Sample '+chr(65+index),''))
            database.execute('INSERT OR IGNORE INTO usage_events '
                '(id,observed_at,category,owner,model,service_tier,operation,input_tokens,output_tokens,'
                'cached_input_tokens,cost_nanos,rates) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)',
                ('service-color-'+suffix,time.time()-30,'subscription',owner,'preview-speed',
                 'default','responses',100*(index+1),20*(index+1),0,10000*(index+1),'{}'))
    home = state.parent / 'codex_home'
    rollout = home / 'sessions' / 'metrics-tools.jsonl'
    rollout.parent.mkdir(parents=True,exist_ok=True)
    events = [{'type':'event_msg','payload':{'type':'task_started','turn_id':METRICS_TURN}}]
    for identifier,begin,end in [('one',1000,4000),('two',2000,6000)]:
        events.append({'type':'event_msg','payload':{'type':'item_completed','thread_id':METRICS_THREAD,
            'turn_id':METRICS_TURN,'started_at_ms':begin,'completed_at_ms':end,
            'item':{'type':'CommandExecution','id':identifier}}})
    rollout.write_text(''.join(json.dumps(event)+'\n' for event in events))
    with sqlite3.connect(home / 'state_5.sqlite') as database:
        database.execute('CREATE TABLE IF NOT EXISTS threads(id TEXT PRIMARY KEY,title TEXT,rollout_path TEXT)')
        database.execute('INSERT OR REPLACE INTO threads VALUES (?,?,?)',
            (METRICS_THREAD,'Synthetic stream',str(rollout)))


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
            {'id':'shared-model','selected':True,'name':'Sample model B','default_context':272000,'maximum':872000}]}]}, headers, timeout=90)
    assert status == 200, exported
    base, state, headers, _ = endpoints[1]
    seed_performance(base, state, headers)
    seed_retry_examples(state)
    return endpoints[1], target, exported['bundle']


def seed_session_bills(state):
    examples = [
        ('33333333-3333-4333-8333-333333333333','修复模型切换 / Fix model switching',[
            ('gpt-6.1-sol',12000,2000,9000,0,40000000),
            ('claude-opus-5-5',3000,600,2000,0,50000000)]),
        ('44444444-4444-4444-8444-444444444444','未计价模型 / Unpriced model',[
            ('custom-model',2500,500,0,0,None)]),
        ('55555555-5555-4555-8555-555555555555','用量不完整 / Incomplete usage',[
            ('sample-model',1000,None,None,None,None)]),
        ('66666666-6666-4666-8666-666666666666','零价模型 / Zero-rate model',[
            ('local-model',300,40,0,0,0)]),
    ]
    with sqlite3.connect(state/'usage.sqlite3') as database:
        for index,(session,title,models) in enumerate(examples):
            for part,(model,inputs,outputs,cached,written,cost) in enumerate(models):
                identifier=f'bill-example-{index}-{part}'
                database.execute('INSERT OR IGNORE INTO usage_events '
                    '(id,observed_at,category,owner,model,service_tier,operation,input_tokens,'
                    'output_tokens,cached_input_tokens,cache_write_tokens,cost_nanos,rates) '
                    'VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)',
                    (identifier,time.time()-index-2,'external','bill-fixture',model,'default',
                     'responses',inputs,outputs,cached,written,cost,'{}'))
                database.execute('INSERT OR IGNORE INTO usage_sessions VALUES (?,?)',(identifier,session))
    home=state.parent/'codex_home'
    with (home/'session_index.jsonl').open('a',encoding='utf-8') as index:
        for session,title,_ in examples:
            index.write(json.dumps({'id':session,'thread_name':title},ensure_ascii=False)+'\n')


def seed_retry_examples(state):
    with sqlite3.connect(state / 'usage.sqlite3') as database:
        for index, (identifier, status, extra) in enumerate([
            ('guidance-retry', 'completed', {'status':200, 'success':True, 'attempts':2,
                'retries':[{'at':time.time()-10, 'reason':'rate_limit', 'status':429,
                    'delay_ms':1000, 'retry_after_seconds':1}]}),
            ('guidance-limit', 'failed', {'status':429, 'error_class':'rate_limit',
                'attempts':1, 'retry_after_seconds':301}),
            ('guidance-output', 'failed', {'status':502, 'error_class':'stream_incomplete',
                'attempts':1, 'output_emitted':True, 'tool_activity':True}),
        ]):
            started = time.time() - 10 - index
            row = {'request_id':identifier, 'route':'responses', 'model_id':'preview-retry',
                'client_model':'preview-retry', 'upstream_model':'sample-model',
                'response_model':'sample-model', 'state':status, 'duration_ms':2000,
                'started_at':started, 'finished_at':started+2, **extra}
            delivery = {'delivery':'terminal_written', 'downstream_terminal':
                'completed' if status == 'completed' else 'failed'}
            database.execute('INSERT OR IGNORE INTO call_records VALUES (?,?,?,?,?,?,?,?,?,?,?)',
                (identifier,'responses',started,started+2,'','@native','preview-retry','',
                 status,json.dumps(row),json.dumps(delivery)))


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
        seed_session_bills(endpoint[1])
        url, _ = preview(stack, endpoint[0], bundle, endpoint[2]['X-EMP-Session'], port)
        done = threading.Event()
        signal.signal(signal.SIGTERM, lambda *_:done.set())
        signal.signal(signal.SIGINT, lambda *_:done.set())
        if open_browser:
            import webbrowser
            webbrowser.open(url)
        print(f'EMP unified Shadow: {url}/',flush=True)
        done.wait()
