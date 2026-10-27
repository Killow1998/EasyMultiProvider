"""Check App Server timestamps, billing-source isolation and negotiated compression.

Requires Python 3.14 and the websockets test client. All model/backend traffic is
synthetic and loopback-only; the supplied Codex and EMP installations are unchanged.
"""
import argparse
import base64
import contextlib
from compression import zstd
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time

from scripts.code_mode_collaboration_acceptance import response_events
from scripts.native_restore_acceptance.app_server import AppServer
from scripts.native_restore_acceptance.common import bounded_argv, sha256_file
from scripts.shadow_frontend.local_backend import free_port, request, start_backend, stop, wait_for


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            raw = zstd.decompress(raw)
        body = json.loads(raw)
        self.server.paths.append(self.path)
        if self.path.endswith('/usage/thread_usage/query'):
            row = {'thread_id':body['thread_ids'][0], 'estimated_usage_credits_micros':123000000,
                   'estimated_usage_usd_micros':2000000, 'groups':[]}
            if self.server.native_usd != 'absent':
                row['native_usage_usd_micros'] = self.server.native_usd
            result = json.dumps({'threads':[row]}).encode()
            self.send_response(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(result)))
            self.end_headers()
            self.wfile.write(result)
            return
        if self.path != '/v1/responses':
            self.send_error(404)
            return
        assert body['model'] == 'upstream-fixture', body['model']
        self.server.inputs.append(body['input'])
        index = len(self.server.inputs)
        if index > 1:
            assert 'FIXTURE_REPLY_1' in json.dumps(body['input']), 'Resumed history lost its first reply'
        item = {'id':f'message-{index}', 'type':'message', 'role':'assistant', 'status':'completed',
                'content':[{'type':'output_text','text':f'FIXTURE_REPLY_{index}','annotations':[]}]}
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Connection','close')
        self.end_headers()
        for event in response_events(item,f'resp-{index}'):
            if 'response' in event:
                event['response']['usage']['input_tokens_details'] = {'cached_tokens':0}
            self.wfile.write(('data: '+json.dumps(event)+'\n\n').encode())
        self.wfile.flush()


def compression_case(codex, env, root, enabled, requested):
    from websockets.sync.client import connect

    port = free_port()
    label = f'compression-{enabled}-{requested}'
    with contextlib.ExitStack() as stack:
        log = stack.enter_context((root/(label+'.stderr')).open('wb'))
        process = subprocess.Popen(bounded_argv([
            str(codex),'--disable','plugins','app-server','--listen',f'ws://127.0.0.1:{port}',
            '-c',f'features.app_server_websocket_compression={str(enabled).lower()}',
        ],1536*1024*1024),env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log)
        stack.callback(stop,process)
        def listening():
            assert process.poll() is None, (label,'App Server exited')
            with socket.socket() as probe:
                return probe.connect_ex(('127.0.0.1',port)) == 0
        wait_for(listening)
        compressed = enabled and requested
        frames = []
        # Reconnect with fresh codecs: stream history belongs to one connection.
        for _ in range(2):
            headers = {'x-codex-app-server-compression':'zstd-stream-v1'} if requested else {}
            with connect(f'ws://127.0.0.1:{port}',additional_headers=headers,proxy=None,
                         compression=None,max_size=16*1024*1024) as connection:
                assert connection.response.headers.get('x-codex-app-server-compression') == (
                    'zstd-stream-v1' if compressed else None)
                encoder, decoder = zstd.ZstdCompressor(level=-7), zstd.ZstdDecompressor()
                for identity,method,params in [
                    (1,'initialize',{'clientInfo':{'name':'emp-upstream-test','version':'1'},
                                     'capabilities':{'experimentalApi':True}}),
                    (2,'config/read',{}),
                ]:
                    text = json.dumps({'id':identity,'method':method,'params':params})
                    connection.send(encoder.compress(text.encode())+encoder.flush(zstd.ZstdCompressor.FLUSH_BLOCK)
                                    if compressed else text)
                    while True:
                        frame = connection.recv(timeout=15)
                        assert isinstance(frame,bytes) == compressed, (label,type(frame).__name__)
                        frames.append('binary' if compressed else 'text')
                        value = json.loads(decoder.decompress(frame) if compressed else frame)
                        if value.get('id') == identity:
                            assert 'error' not in value, value
                            break
                    if identity == 1:
                        text = json.dumps({'method':'initialized','params':{}})
                        connection.send(encoder.compress(text.encode())+encoder.flush(zstd.ZstdCompressor.FLUSH_BLOCK)
                                        if compressed else text)
                if compressed:
                    assert not decoder.eof, 'A message must flush, not finish, its zstd stream'
        return {'server_enabled':enabled,'client_requested':requested,
                'negotiated':compressed,'reconnections':1,'frames':frames}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex',type=Path,required=True)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    codex, binary = args.codex.resolve(), args.emp.resolve()
    args.output.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-upstream-case-') as temporary, contextlib.ExitStack() as stack:
        root, checks = Path(temporary), []
        upstream = ThreadingHTTPServer(('127.0.0.1',0),Upstream)
        upstream.daemon_threads = True
        upstream.native_usd, upstream.paths, upstream.inputs = 'absent', [], []
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever,daemon=True).start()
        stack.callback(upstream.shutdown)
        endpoint = f'http://127.0.0.1:{upstream.server_port}'
        emp_root, home = root/'emp', root/'codex'
        emp_root.mkdir()
        home.mkdir()
        (emp_root/'state').mkdir()
        (emp_root/'state/api_prices.json').write_text(json.dumps({'fetched_at':time.time(),
            'prices':{'upstream-fixture':{'input_cost_per_token':'0.001','output_cost_per_token':'0.002'}}}))
        proxies = {name:'http://127.0.0.1:9' for name in
                   ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy')}
        proxies.update(NO_PROXY='localhost,127.0.0.1',no_proxy='localhost,127.0.0.1')
        fixture = {'auto_enable_on_start':False,'codex_base_url':endpoint,
            'providers':[{'id':'fixture','protocol':'responses','auth_mode':'api_key',
                          'api_key':'fixture-only','base_url':endpoint+'/v1'}],
            'models':[{'id':'fixture/model','provider':'fixture','upstream_id':'upstream-fixture','enabled':True}]}
        base,_,_,headers = start_backend(stack,binary,emp_root,fixture,{'CODEX_HOME':str(home),**proxies})
        headers['Origin'] = base
        status,catalog = request(base+'/v1/models?client_version=0.166.0-alpha.2',headers=headers)
        assert status == 200
        catalog['models'] = [entry for entry in catalog['models'] if entry['slug'] == 'fixture/model']
        assert len(catalog['models']) == 1
        catalog['models'][0].update(tool_mode='function',base_instructions='Reply to the fixture.',use_responses_lite=False)
        (home/'models.json').write_text(json.dumps(catalog))
        (home/'config.toml').write_text(
            'model="fixture/model"\nmodel_provider="fixture"\ncli_auth_credentials_store="file"\n'
            f'chatgpt_base_url={json.dumps(endpoint)}\nmodel_catalog_json={json.dumps(str(home/"models.json"))}\n'
            'approval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
            '[features]\nplugins=false\nmemories=false\nasync_compaction=false\ncode_mode=false\n'
            'app_server_websocket_compression=true\n'
            '[model_providers.fixture]\nname="Fixture"\nwire_api="responses"\n'
            f'base_url={json.dumps(base+"/v1")}\nrequires_openai_auth=false\nenv_key="EMP_TEST_KEY"\n'
            'supports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n'
            f'http_headers={{"X-EMP-Session"={json.dumps(headers["X-EMP-Session"])}}}\n')
        env = {'PATH':os.environ['PATH'],'HOME':str(home),'CODEX_HOME':str(home),
               'XDG_CONFIG_HOME':str(home),'EMP_TEST_KEY':'fixture-only','RUST_LOG':'error',**proxies}
        client = AppServer(codex,env,args.output/'stdio.stderr')
        stack.callback(client.close)
        thread = client.rpc('thread/start',{'cwd':str(home),'model':'fixture/model',
                            'approvalPolicy':'never','sandbox':'read-only','ephemeral':False})['thread']['id']
        for index in range(1,4):
            if index == 3:
                client.close()
                client = AppServer(codex,env,args.output/'resumed.stderr')
                stack.callback(client.close)
                client.rpc('thread/resume',{'threadId':thread})
            client.rpc('turn/start',{'threadId':thread,'input':[{'type':'text','text':f'Fixture {index}'}]})
            assert client.wait_turn(thread,timeout=30) == 'completed'
            history = client.rpc('thread/read',{'threadId':thread,'includeTurns':True})['thread']
            assert f'FIXTURE_REPLY_{index}' in json.dumps(history)
        items = [item for turn in history['turns'] for item in turn['items']]
        messages = [item for item in items if item['type'] == 'agentMessage']
        assert len(messages) == 3 and all('payload' not in item for item in messages)
        assert all(isinstance(item.get('startedAtMs'),int) and
                   item['completedAtMs'] >= item['startedAtMs'] for item in messages), messages
        checks.append('Three turns and process resume retain flat ThreadItems with lifecycle timestamps while stdio compression is enabled')
        before = request(base+'/api/usage?start=0&end='+str(time.time()+60),headers=headers)[1]['totals']
        claims = {'email':'fixture@example.invalid','exp':int(time.time())+3600,
                  'https://api.openai.com/auth':{'chatgpt_account_id':'fixture-account','chatgpt_plan_type':'pro'}}
        token = 'e30.'+base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=')+'.fixture'
        client.rpc('account/login/start',{'type':'chatgptAuthTokens','accessToken':token,
                   'chatgptAccountId':'fixture-account','chatgptPlanType':'pro'})
        usd_cases = []
        for amount in ['absent',None,0,12345678]:
            upstream.native_usd = amount
            usage = client.rpc('account/usage/read',{'threadId':thread})['threadUsage']
            assert usage['nativeUsageUsdMicros'] == (None if amount == 'absent' else amount), usage
            assert usage['estimatedUsageUsdMicros'] == 2000000, usage
            assert usage['estimatedUsageCreditsMicros'] == 123000000, usage
            usd_cases.append({'backend_native_usd_micros':amount,'app_server_native_usd_micros':usage['nativeUsageUsdMicros']})
        after = request(base+'/api/usage?start=0&end='+str(time.time()+60),headers=headers)[1]['totals']
        assert before == after and after['requests'] == 3 and after['cost_nanos'] == 48000000, (before,after)
        checks.append('Absent/null/zero/nonzero native USD stay separate from credit estimates and do not add or reprice EMP usage')
        client.close()
        compression = [compression_case(codex,env,args.output,enabled,requested)
                       for enabled,requested in [(False,True),(True,False),(True,True)]]
        checks.append('Unacknowledged/legacy connections stay text; negotiated zstd uses binary frames, persistent stream history and fresh reconnect codecs')
        result = {'codex_version':subprocess.check_output([str(codex),'--version'],env=env,text=True).strip(),
                  'codex_sha256':sha256_file(codex),'emp_sha256':sha256_file(binary),'checks':checks,
                  'completed_turns':len(upstream.inputs),'native_usd_cases':usd_cases,'emp_usage_totals':after,
                  'compression':compression,'backend_paths':sorted(set(upstream.paths)),
                  'real_model_calls':0,'live_instances_changed':False}
        (args.output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps({'passed':len(checks),'completed_turns':len(upstream.inputs)}))


if __name__ == '__main__':
    main()
