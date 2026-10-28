"""Real Codex 0.165 memory selection and terminal errors through isolated EMP."""

import argparse
import base64
import contextlib
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading

from scripts.code_mode_collaboration_acceptance import response_events
from scripts.native_restore_acceptance.app_server import AppServer
from scripts.shadow_frontend.local_backend import request, start_backend

LOOPBACK_ENV = {key:'http://127.0.0.1:9' for key in
                ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy')}
LOOPBACK_ENV.update(NO_PROXY='localhost,127.0.0.1', no_proxy='localhost,127.0.0.1')


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            from compression import zstd
            raw = zstd.decompress(raw)
        self.server.requests.append(json.loads(raw))
        failure = self.server.failure
        error = {'code':'model_not_found', 'message':'synthetic unavailable model'}
        if isinstance(failure, int):
            self.send_response(failure)
            self.send_header('Content-Type', 'application/json')
            body = json.dumps({'error':error}).encode()
        else:
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            if failure == 'error':
                events = [{'type':'error', 'error':error}]
            elif failure == 'response.failed':
                events = [{'type':'response.failed', 'response':{'id':'failed', 'status':'failed', 'error':error}}]
            else:
                item = {'id':'message', 'type':'message', 'role':'assistant', 'status':'completed',
                        'content':[{'type':'output_text', 'text':'OK'}]}
                events = response_events(item, 'response')
            body = b''.join(b'data: '+json.dumps(event).encode()+b'\n\n' for event in events)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def run_case(codex, root, base, headers, catalog, upstream, name, evidence, legacy=False,
             version=None, enabled=True, failure=None, model='native/probe'):
    home = root/name
    home.mkdir()
    evidence.mkdir(exist_ok=True)
    (home/'models.json').write_text(json.dumps(catalog))
    for folder, marker in [('memories', 'LEGACY_MEMORY_MARKER'), ('memories_v2', 'V2_MEMORY_MARKER')]:
        if folder == 'memories' and not legacy:
            continue
        memory = home/folder
        memory.mkdir()
        (memory/'memory_summary.md').write_text('v1\n\n## User profile\n\n'+marker+'\n')
    claims = {'email':'memory@example.test', 'exp':2100000000,
              'https://api.openai.com/auth':{'chatgpt_plan_type':'plus',
              'chatgpt_user_id':'fixture-user', 'chatgpt_account_id':'fixture-account'}}
    token = 'eyJhbGciOiJub25lIn0.'+base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=')+'.c2ln'
    (home/'auth.json').write_text(json.dumps({'auth_mode':'chatgpt', 'tokens':{
        'id_token':token, 'access_token':'fixture-only', 'refresh_token':'', 'account_id':'fixture-account'},
        'last_refresh':datetime.now(timezone.utc).isoformat()}))
    config = (
        f'model={json.dumps(model)}\nmodel_provider="fixture"\n'
        f'model_catalog_json={json.dumps(str(home/"models.json"))}\n'
        'approval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
        '[features]\nplugins=false\nasync_compaction=false\ncode_mode=false\n'
        f'memories={str(enabled).lower()}\n'
        '[memories]\ngenerate_memories=false\n'
        +(f'version={json.dumps(version)}\n' if version else '')+
        '[model_providers.fixture]\nname="OpenAI"\nwire_api="responses"\n'
        f'base_url={json.dumps(base+"/v1")}\nrequires_openai_auth=false\nenv_key="EMP_TEST_KEY"\n'
        'supports_websockets=false\nrequest_max_retries=2\nstream_max_retries=2\n'
        f'http_headers={{"X-EMP-Session"={json.dumps(headers["X-EMP-Session"])}}}\n'
    )
    (home/'config.toml').write_text(config)
    env = {'PATH':os.environ['PATH'], 'HOME':str(home), 'CODEX_HOME':str(home),
           'XDG_CONFIG_HOME':str(home), 'EMP_TEST_KEY':'fixture-only', 'RUST_LOG':'error',
           **LOOPBACK_ENV}
    upstream.failure, upstream.requests = failure, []
    client = AppServer(codex, env, evidence/'codex.stderr')
    try:
        started = client.rpc('thread/start', {'cwd':str(home), 'model':model,
                             'approvalPolicy':'never', 'sandbox':'read-only', 'ephemeral':True})
        client.rpc('turn/start', {'threadId':started['thread']['id'],
                                 'input':[{'type':'text', 'text':'Reply OK.'}]})
        while True:
            event = client.read_message(30)
            client.events.append(event)
            if event.get('method') == 'turn/completed':
                turn = event['params']['turn']
                break
        assert len(upstream.requests) == 1, (name, 'inference was replayed', len(upstream.requests))
        if failure is not None:
            assert turn['status'] == 'failed', (name, turn)
            assert 'modelNotFound' in json.dumps(turn), (name, turn)
        else:
            assert turn['status'] == 'completed', (name, turn)
            wire = json.dumps(upstream.requests[0])
            expected = None if not enabled else ('v2' if version == 'v2' or (not legacy and version is None) else 'v1')
            assert ('LEGACY_MEMORY_MARKER' in wire) == (expected == 'v1'), (name, 'v1 selection')
            assert ('V2_MEMORY_MARKER' in wire) == (expected == 'v2'), (name, 'v2 selection')
        assert (home/'config.toml').read_text() == config, (name, 'Codex config changed')
        return {'case':name, 'passed':True, 'requests':1, 'turn_status':turn['status'],
                'memory_version':expected if failure is None else None,
                'codex_error_info':turn.get('error', {}).get('codexErrorInfo') if turn.get('error') else None}
    finally:
        client.close()
        (evidence/'events.json').write_text(json.dumps(client.events, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-codex165-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        upstream.daemon_threads = True
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        endpoint = f'http://127.0.0.1:{upstream.server_port}/v1'
        fixture = {'auto_enable_on_start':False,
            'providers':[{'id':name, 'protocol':'responses', 'base_url':endpoint,
                          'auth_mode':auth, 'api_key':'fixture-only'}
                         for name, auth in [('native', 'forward'), ('external', 'api_key')]],
            'models':[{'id':name+'/probe', 'provider':name, 'upstream_id':'probe', 'enabled':True}
                      for name in ('native', 'external')]}
        emp_root = root/'emp'
        emp_root.mkdir()
        base, _, _, headers = start_backend(stack, args.emp.resolve(), emp_root, fixture, LOOPBACK_ENV)
        version = subprocess.check_output([str(args.codex), '--version'], text=True,
                  env={'PATH':os.environ['PATH'], 'HOME':str(root), 'CODEX_HOME':str(root)}).strip()
        status, catalog = request(base+'/v1/models?client_version='+version.split()[-1], headers=headers)
        assert status == 200
        catalog['models'] = [entry for entry in catalog['models'] if entry['slug'] in ('native/probe', 'external/probe')]
        report = {'codex_version':version, 'real_model_calls':0, 'cases':[],
                  'binary_sha256':{str(path):hashlib.file_digest(path.open('rb'), 'sha256').hexdigest()
                                   for path in (args.codex, args.emp)}}
        cases = [('fresh-v2', {}), ('existing-v1', {'legacy':True}),
                 ('explicit-v2', {'legacy':True, 'version':'v2'}), ('disabled', {'enabled':False})]
        cases += [(source+'-'+str(failure), {'failure':failure, 'enabled':False, 'model':source+'/probe'})
                  for source in ('native', 'external') for failure in (400, 403, 404, 'error', 'response.failed')]
        for name, options in cases:
            result = run_case(args.codex.resolve(), root, base, headers, catalog, upstream, name, args.output/name, **options)
            report['cases'].append(result)
            (args.output/'result.json').write_text(json.dumps(report, indent=2))
            print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
