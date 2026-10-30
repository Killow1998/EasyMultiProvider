"""Synthetic accounts, loopback providers and a disposable Codex/EMP lifecycle."""
import base64
import contextlib
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import itertools
import hashlib
import json
import os
import re
import subprocess
import threading

from scripts.code_mode_collaboration_acceptance import response_events
from scripts.codex_0165_acceptance import LOOPBACK_ENV
from scripts.native_restore_acceptance.app_server import AppServer
from scripts.shadow_frontend.local_backend import request, start_backend, stop, wait_for


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.headers.get('Upgrade', '').lower() == 'websocket':
            key = self.headers['Sec-WebSocket-Key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', base64.b64encode(hashlib.sha1(key.encode()).digest()).decode())
            self.end_headers()
            self.close_connection = True
            while prefix := self.rfile.read(2):
                opcode, length = prefix[0] & 15, prefix[1] & 127
                if opcode == 8:
                    return
                assert opcode == 1 and prefix[1] & 128, (opcode, prefix)
                if length in (126, 127):
                    length = int.from_bytes(self.rfile.read(2 if length == 126 else 8), 'big')
                mask = self.rfile.read(4)
                raw = self.rfile.read(length)
                body = json.loads(bytes(byte ^ mask[index % 4] for index, byte in enumerate(raw)))
                if body.get('type') == 'codex.client.metadata':
                    continue
                assert body.get('type') == 'response.create', body
                record = {'path':self.path, 'body':body, 'headers':dict(self.headers), 'transport':'websocket'}
                record['response_id'] = 'response-'+str(next(self.server.sequence))
                self.server.requests.append(record)
                item = self.server.respond(record)
                if item['type'] == 'message':
                    item['id'] = 'message-'+record['response_id']
                events = response_events(item, record['response_id']) if body.get('generate') is not False else [
                    {'type':'response.completed', 'response':{'id':record['response_id'], 'status':'completed',
                        'output':[], 'usage':{'input_tokens':10, 'output_tokens':0, 'total_tokens':10}}}]
                for event in events:
                    payload = json.dumps(event).encode()
                    size = len(payload)
                    prefix = bytes([129, size]) if size < 126 else bytes([129, 126])+size.to_bytes(2, 'big')
                    self.wfile.write(prefix+payload)
                    self.wfile.flush()
            return
        assert self.path.split('?')[0].endswith('/models'), self.path
        encoded = json.dumps(self.server.catalog).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.send_header('ETag', '"fixture-models-v1"')
        self.end_headers()
        self.wfile.write(encoded)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            from compression import zstd
            raw = zstd.decompress(raw)
        body = json.loads(raw)
        record = {'path':self.path, 'body':body, 'headers':dict(self.headers)}
        self.server.requests.append(record)
        identity = 'response-'+str(next(self.server.sequence))
        record['response_id'] = identity
        item = self.server.respond(record)
        if item['type'] == 'message':
            item['id'] = 'message-'+identity
        if self.path.endswith('/chat/completions'):
            events = [{'id':identity, 'choices':[{'index':0, 'delta':{'content':item['content'][0]['text']}}]},
                      {'id':identity, 'choices':[{'index':0, 'delta':{}, 'finish_reason':'stop'}],
                       'usage':{'prompt_tokens':10, 'completion_tokens':3, 'total_tokens':13}}]
        elif self.path.endswith('/messages'):
            events = [{'type':'message_start', 'message':{'id':identity, 'role':'assistant', 'content':[],
                        'usage':{'input_tokens':10, 'output_tokens':0}}},
                      {'type':'content_block_start', 'index':0, 'content_block':{'type':'text', 'text':''}},
                      {'type':'content_block_delta', 'index':0, 'delta':{'type':'text_delta', 'text':item['content'][0]['text']}},
                      {'type':'content_block_stop', 'index':0},
                      {'type':'message_delta', 'delta':{'stop_reason':'end_turn'}, 'usage':{'output_tokens':3}},
                      {'type':'message_stop'}]
        else:
            events = response_events(item, identity)
        encoded = b''.join(b'data: '+json.dumps(event).encode()+b'\n\n' for event in events)
        if self.path.endswith('/chat/completions'):
            encoded += b'data: [DONE]\n\n'
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


def message(text='FIXTURE_COMPLETE'):
    return {'id':'message', 'type':'message', 'role':'assistant', 'status':'completed',
            'content':[{'type':'output_text', 'text':text}]}


class Fixture:
    def __init__(self, codex, emp, root, evidence, *, configuration=None, native_catalog=None):
        self.codex, self.root, self.evidence = codex, root, evidence
        self.stack = contextlib.ExitStack()
        self.home = root/'codex'
        self.home.mkdir(parents=True)
        self.evidence.mkdir(parents=True, exist_ok=True)
        self.upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        self.upstream.daemon_threads = True
        self.upstream.requests, self.upstream.sequence = [], itertools.count(1)
        self.upstream.respond = lambda _record: message()
        self.stack.callback(self.upstream.server_close)
        threading.Thread(target=self.upstream.serve_forever, daemon=True).start()
        self.stack.callback(self.upstream.shutdown)
        endpoint = f'http://127.0.0.1:{self.upstream.server_port}/v1'
        claims = {'email':'fixture@example.test', 'exp':2100000000,
                  'https://api.openai.com/auth':{'chatgpt_plan_type':'plus',
                  'chatgpt_user_id':'fixture-user', 'chatgpt_account_id':'fixture-account'}}
        token = 'eyJhbGciOiJub25lIn0.'+base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=')+'.c2ln'
        auth = {'auth_mode':'chatgpt', 'tokens':{'id_token':token, 'access_token':'fixture-only',
                'refresh_token':'', 'account_id':'fixture-account'}, 'last_refresh':datetime.now(timezone.utc).isoformat()}
        (self.home/'auth.json').write_text(json.dumps(auth))
        native = ['gpt-6.1-sol', 'gpt-6-luna', 'gpt-5.6-luna', 'gpt-6.1-terra']
        native_path = root/'native.json'
        self.native_catalog = native_catalog or {'models':[{'slug':name, 'supported_in_api':True,
            'supports_reasoning_effort_updates':True,
            'context_window':100000, 'shell_type':'shell_command', 'base_instructions':'Synthetic acceptance fixture.',
            'default_reasoning_level':'high', 'supported_reasoning_levels':[
                {'effort':effort, 'description':effort} for effort in ('low', 'medium', 'high', 'xhigh')]}
            for name in native]}
        native_path.write_text(json.dumps(self.native_catalog))
        self.upstream.catalog = self.native_catalog
        providers = [('responses', 'responses', 'api_key'),
                     ('chat', 'chat_completions', 'api_key'), ('anthropic', 'anthropic_messages', 'api_key')]
        config = {'auto_enable_on_start':False, 'codex_base_url':endpoint,
                  'native_catalog_path':str(native_path),
                  'providers':[{'id':name, 'protocol':protocol, 'auth_mode':auth_mode,
                                'base_url':endpoint, 'api_key':'fixture-only'}
                               for name, protocol, auth_mode in providers],
                  'models':[{'id':name+'/probe', 'provider':name, 'upstream_id':'probe',
                             'enabled':True, 'supports_reasoning':True,
                             'reasoning_levels':['low', 'medium', 'high', 'xhigh']}
                            for name, _, _ in providers]}
        config.update(configuration or {})
        backend = root/'emp'
        backend.mkdir()
        self.base, _, _, self.headers = start_backend(self.stack, emp, backend, config,
                                      {**LOOPBACK_ENV, 'CODEX_HOME':str(self.home)})
        status, self.catalog = request(self.base+'/v1/models?client_version=0.165.0', headers=self.headers)
        assert status == 200, self.catalog
        template = next(entry for entry in self.catalog['models'] if entry['slug'] == 'responses/probe')
        external = [entry for entry in self.catalog['models'] if '/' in entry['slug']]
        self.catalog['models'] = external + [{**template, 'slug':name, 'display_name':name,
            'apply_patch_tool_type':'freeform', 'use_responses_lite':True,
            'supports_reasoning_effort_updates':True} for name in native]
        (self.home/'models.json').write_text(json.dumps(self.catalog))
        self.config = (
            'model="gpt-6.1-sol"\nmodel_provider="fixture"\n'
            f'model_catalog_json={json.dumps(str(self.home/"models.json"))}\n'
            'model_reasoning_effort="high"\napproval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
            '[features]\nplugins=false\napps=false\nmemories=false\nasync_compaction=false\ncode_mode=false\n'
            '[model_providers.fixture]\nname="OpenAI"\nwire_api="responses"\n'
            f'base_url={json.dumps(self.base+"/v1")}\nrequires_openai_auth=false\nenv_key="EMP_TEST_KEY"\n'
            'supports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n'
            f'http_headers={{"X-EMP-Session"={json.dumps(self.headers["X-EMP-Session"])}}}\n')
        (self.home/'config.toml').write_text(self.config)
        self.env = {'PATH':os.environ['PATH'], 'HOME':str(self.home), 'CODEX_HOME':str(self.home),
                    'XDG_CONFIG_HOME':str(self.home), 'EMP_TEST_KEY':'fixture-only',
                    'RUST_LOG':'error,codex_memories_write=info', **LOOPBACK_ENV}
        self.client_count = itertools.count(1)

    def import_account(self):
        auth = json.loads((self.home/'auth.json').read_text())
        auth['tokens']['account_id'] = 'imported-fixture-account'
        auth['tokens']['access_token'] = 'imported-fixture-only'
        claims = json.loads(base64.urlsafe_b64decode(auth['tokens']['id_token'].split('.')[1]+'=='))
        claims['email'] = 'imported@example.test'
        claims['https://api.openai.com/auth']['chatgpt_account_id'] = 'imported-fixture-account'
        encoded = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=')
        auth['tokens']['id_token'] = 'eyJhbGciOiJub25lIn0.'+encoded+'.c2ln'
        payload = {'id':'fixture-imported', 'prefix':'imported', 'name':'Imported fixture',
                   'auth_json':auth, 'enabled':True}
        status, result = request(self.base+'/api/accounts/import', payload, self.headers)
        assert status == 200, result
        status, refreshed = request(self.base+'/api/accounts/fixture-imported/models/refresh', {}, self.headers)
        assert status == 200, refreshed
        return result['account']

    def client(self):
        client = AppServer(self.codex, self.env, self.evidence/f'codex-{next(self.client_count)}.stderr',
                           address_space_limit_bytes=2 * 1024**4)
        self.stack.callback(client.close)
        return client

    def executor(self, client):
        log_path = self.evidence/'executor.log'
        log = self.stack.enter_context(log_path.open('w'))
        process = subprocess.Popen([str(self.codex), 'exec-server', '--listen', 'ws://127.0.0.1:0'],
                                   env=self.env, cwd=self.home, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        self.stack.callback(stop, process)
        url = wait_for(lambda: re.search(r'ws://127\.0\.0\.1:\d+', log_path.read_text()))[0]
        client.rpc('environment/add', {'environmentId':'memory-fixture', 'execServerUrl':url})
        return {'environmentId':'memory-fixture', 'cwd':str(self.home)}

    def close(self):
        self.stack.close()
        (self.evidence/'requests.json').write_text(json.dumps(self.upstream.requests, indent=2))

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


def turn(client, thread, text, **settings):
    client.rpc('turn/start', {'threadId':thread, 'input':[{'type':'text', 'text':text}], **settings})
    status = client.wait_turn(thread, timeout=30)
    assert status == 'completed', (status, client.events)
