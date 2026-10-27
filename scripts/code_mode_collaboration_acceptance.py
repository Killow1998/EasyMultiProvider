"""Real standalone Codex collaboration through EMP, with loopback-only inference."""
import argparse
import contextlib
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import threading
import time

from scripts.native_restore_acceptance.app_server import AppServer
from scripts.shadow_frontend.local_backend import request, start_backend


PARENT = 'native/gpt-parent'
CHILD = 'external/worker'
TASK = 'EMP_CODE_MODE_TASK_中文'
MESSAGE = 'EMP_CODE_MODE_MESSAGE'
FOLLOWUP = 'EMP_CODE_MODE_FOLLOWUP'
RESULT = 'EMP_CODE_MODE_CHILD_RESULT'
# V8 reserves a large virtual sandbox; this is not resident memory allocation.
CODE_MODE_ADDRESS_SPACE = 2 * 1024 ** 4


def response_events(item, identity):
    response = {'id':identity, 'object':'response', 'status':'completed', 'output':[item],
                'usage':{'input_tokens':10, 'output_tokens':3, 'total_tokens':13}}
    yield {'type':'response.created', 'response':{**response, 'status':'in_progress', 'output':[]}}
    yield {'type':'response.output_item.added', 'output_index':0, 'item':item}
    if item['type'] == 'message':
        yield {'type':'response.output_text.delta', 'item_id':item['id'], 'output_index':0,
               'content_index':0, 'delta':item['content'][0]['text']}
    yield {'type':'response.output_item.done', 'output_index':0, 'item':item}
    yield {'type':'response.completed', 'response':response}


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            raw = self.rfile.read(int(self.headers['Content-Length']))
            if self.headers.get('Content-Encoding') == 'zstd':
                from compression import zstd
                raw = zstd.decompress(raw)
            body = json.loads(raw)
            assert body['model'] in ('gpt-parent', 'worker'), 'Unexpected recovery/model request'
            with self.server.lock:
                self.server.requests.append({'path':self.path, 'body':body,
                                             'subagent':self.headers.get('x-openai-subagent')})
                identity = f'resp-{len(self.server.requests)}'
            child = body['model'] == 'worker'
            text = json.dumps(body, ensure_ascii=False)
            if child:
                assert TASK in text, 'Child lost the original task'
                answer = RESULT + (' FOLLOWUP_COMPLETE' if FOLLOWUP in text else ' INITIAL_COMPLETE')
            else:
                operation = self.server.operation
                if operation not in self.server.sent and operation in ('spawn','message','followup','verify'):
                    assert any(tool.get('name') == 'exec' and tool['type'] == 'custom'
                               for tool in body['tools']), 'Code Mode was not enabled'
                    self.server.sent.add(operation)
                    method, arguments = {
                        'spawn':('spawn_agent', {'task_name':'worker','model':CHILD,'fork_turns':'none','message':TASK}),
                        'message':('send_message', {'target':'/root/worker','message':MESSAGE}),
                        'followup':('followup_task', {'target':'/root/worker','message':FOLLOWUP}),
                        'verify':('wait_agent', {'timeout_ms':10000}),
                    }[operation]
                    script = f'text(await tools.collaboration__{method}({json.dumps(arguments,ensure_ascii=False)}));'
                    item = {'id':'item-'+identity,'type':'custom_tool_call','call_id':'call-'+identity,
                            'name':'exec','namespace':'functions','input':script,'status':'completed'}
                    self.stream(response_events(item,identity))
                    return
                if operation in ('verify','resume'):
                    assert RESULT+' FOLLOWUP_COMPLETE' in text, 'Parent lost the latest child result'
                answer = 'PARENT_' + operation.upper()
            if child and self.server.protocol == 'chat_completions':
                events = [
                    {'id':identity,'object':'chat.completion.chunk','model':'worker','choices':[{'index':0,'delta':{'role':'assistant','content':answer},'finish_reason':None}]},
                    {'id':identity,'object':'chat.completion.chunk','model':'worker','choices':[{'index':0,'delta':{},'finish_reason':'stop'}],'usage':{'prompt_tokens':10,'completion_tokens':3,'total_tokens':13}},
                ]
                self.stream(events, done=True)
            elif child and self.server.protocol == 'anthropic_messages':
                self.stream([
                    {'type':'message_start','message':{'id':identity,'type':'message','role':'assistant','model':'worker','content':[],'stop_reason':None,'usage':{'input_tokens':10,'output_tokens':0}}},
                    {'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},
                    {'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':answer}},
                    {'type':'content_block_stop','index':0},
                    {'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':3}},
                    {'type':'message_stop'},
                ], named=True)
            else:
                item = {'id':'item-'+identity,'type':'message','role':'assistant','status':'completed',
                        'content':[{'type':'output_text','text':answer,'annotations':[]}]}
                self.stream(response_events(item,identity))
        except Exception as error:
            self.server.errors.append(str(error))
            self.send_error(500, 'Fixture failed')

    def stream(self, events, done=False, named=False):
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Connection','close')
        self.end_headers()
        for event in events:
            if named:
                self.wfile.write(f'event: {event["type"]}\n'.encode())
            self.wfile.write(('data: '+json.dumps(event,ensure_ascii=False)+'\n\n').encode())
        if done:
            self.wfile.write(b'data: [DONE]\n\n')
        self.wfile.flush()


def wait_child(client, child_id, completed=True):
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        value = client.rpc('thread/read',{'threadId':child_id,'includeTurns':True})['thread']
        turns = value['turns']
        if turns:
            terminal = next((event['params']['turn'] for event in reversed(client.events)
                             if event.get('method') == 'turn/completed'
                             and event['params']['threadId'] == child_id
                             and event['params']['turn']['id'] == turns[-1]['id']), None)
            if terminal:
                if completed:
                    assert terminal['status'] == 'completed', terminal
                # A history read can precede the persisted turn-completion item.
                if turns[-1]['status'] == terminal['status']:
                    return value
        time.sleep(.1)
    raise AssertionError('Child did not finish')


def turn(client, upstream, thread_id, operation):
    upstream.operation = operation
    client.rpc('turn/start', {'threadId':thread_id,'input':[{'type':'text','text':'Fixture '+operation}]})
    status = client.wait_turn(thread_id, timeout=35)
    assert status == 'completed', (operation,status,upstream.errors)
    return client.rpc('thread/read',{'threadId':thread_id,'includeTurns':True})['thread']


def agent_messages(home):
    messages = []
    for rollout in home.glob('sessions/**/*.jsonl'):
        for line in rollout.read_text().splitlines():
            event = json.loads(line)
            if event.get('payload',{}).get('type') == 'agent_message':
                messages.append(event['payload'])
    return messages


def assert_plaintext(messages, mode):
    expected = [(TASK, '/root', '/root/worker')]
    if mode != 'off':
        expected += [(MESSAGE, '/root', '/root/worker'),
                     (FOLLOWUP, '/root', '/root/worker'),
                     (RESULT+' FOLLOWUP_COMPLETE', '/root/worker', '/root')]
    for marker, author, recipient in expected:
        matches = [item for item in messages if item['author'] == author
                   and item['recipient'] == recipient and marker in json.dumps(item, ensure_ascii=False)]
        assert matches, f'Missing typed agent message: {marker}'
        for item in matches:
            assert 'encrypted_content' not in item
            assert all(part['type'] == 'input_text' for part in item['content'])


def case(binary, codex, root, output, protocol, mode):
    label = protocol+'-'+mode
    root.mkdir()
    evidence = output/label
    evidence.mkdir(exist_ok=True)
    with contextlib.ExitStack() as stack:
        upstream = ThreadingHTTPServer(('127.0.0.1',0),Upstream)
        upstream.daemon_threads = True
        upstream.protocol, upstream.operation = protocol, 'spawn'
        upstream.requests, upstream.errors, upstream.sent = [], [], set()
        upstream.lock = threading.Lock()
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever,daemon=True).start()
        stack.callback(upstream.shutdown)
        endpoint = f'http://127.0.0.1:{upstream.server_port}/v1'
        fixture = {'auto_enable_on_start':False,'external_collaboration':mode,
            'providers':[{'id':'native','protocol':'responses','auth_mode':'forward','base_url':endpoint},
                         {'id':'external','protocol':protocol,'auth_mode':'api_key','api_key':'fixture-only','base_url':endpoint}],
            'models':[{'id':PARENT,'provider':'native','upstream_id':'gpt-parent','enabled':True},
                      {'id':CHILD,'provider':'external','upstream_id':'worker','enabled':True}]}
        proxies = {name:'http://127.0.0.1:9' for name in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy')}
        proxies.update(NO_PROXY='localhost,127.0.0.1',no_proxy='localhost,127.0.0.1')
        emp_root = root/'emp'
        emp_root.mkdir()
        base,_,state,headers = start_backend(stack,binary,emp_root,fixture,proxies)
        status,catalog = request(base+'/v1/models?client_version=0.164.0',headers=headers)
        assert status == 200, catalog
        catalog['models'] = [entry for entry in catalog['models'] if entry['slug'] in (PARENT,CHILD)]
        assert len(catalog['models']) == 2, catalog
        for entry in catalog['models']:
            entry.update(tool_mode='code_mode',multi_agent_version='v2',use_responses_lite=False,
                         supports_parallel_tool_calls=True,base_instructions='Follow the fixture task.')
        home = root/'codex'
        home.mkdir()
        (home/'models.json').write_text(json.dumps(catalog))
        (home/'config.toml').write_text(
            f'model={json.dumps(PARENT)}\nmodel_provider="fixture"\n'
            f'model_catalog_json={json.dumps(str(home/"models.json"))}\n'
            'approval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n'
            '[features]\nplugins=false\nmemories=false\nasync_compaction=false\n'
            'multi_agent=true\ncode_mode=true\ncode_mode_host_grpc=true\n'
            'code_mode_host={enabled=true,disable_in_process_fallback=true}\n'
            '[features.multi_agent_v2]\nenabled=true\nnon_code_mode_only=false\nexpose_spawn_agent_model_overrides=true\n'
            '[model_providers.fixture]\nname="OpenAI"\nwire_api="responses"\n'
            f'base_url={json.dumps(base+"/v1")}\nrequires_openai_auth=false\nenv_key="EMP_TEST_KEY"\n'
            'supports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n'
            f'http_headers={{"X-EMP-Session"={json.dumps(headers["X-EMP-Session"])}}}\n')
        env = {'PATH':os.environ['PATH'],'HOME':str(home),'CODEX_HOME':str(home),
               'XDG_CONFIG_HOME':str(home),'EMP_TEST_KEY':'fixture-only','RUST_LOG':'error',**proxies}
        client = AppServer(codex,env,evidence/'codex.stderr',address_space_limit_bytes=CODE_MODE_ADDRESS_SPACE)
        clients = [client]
        stack.callback(client.close)
        try:
            started = client.rpc('thread/start',{'cwd':str(home),'model':PARENT,'approvalPolicy':'never','sandbox':'read-only','ephemeral':False})
            thread_id = started['thread']['id']
            parent = turn(client,upstream,thread_id,'spawn')
            children = [item['agentThreadId'] for parent_turn in parent['turns']
                        for item in parent_turn['items'] if item.get('type') == 'subAgentActivity'
                        and item.get('kind') == 'started' and item.get('model') == CHILD]
            assert len(children) == 1, f'Expected one actual child, found {len(children)}; see events.json'
            child_id = children[0]
            first = wait_child(client,child_id,completed=mode!='off')
            if mode == 'off':
                assert first['turns'][-1]['status'] == 'failed', first
                assert not any(row['body']['model']=='worker' for row in upstream.requests)
                assert 'external_subagents_disabled' in json.dumps(client.events), client.events
            else:
                assert RESULT in json.dumps(first,ensure_ascii=False), first
                turn(client,upstream,thread_id,'message')
                turn(client,upstream,thread_id,'followup')
                deadline = time.monotonic()+25
                while True:
                    child = wait_child(client,child_id)
                    if 'FOLLOWUP_COMPLETE' in json.dumps(child):
                        break
                    assert time.monotonic()<deadline, child
                    time.sleep(.1)
                turn(client,upstream,thread_id,'verify')
                client.close()
                client = AppServer(codex,env,evidence/'resumed.stderr',address_space_limit_bytes=CODE_MODE_ADDRESS_SPACE)
                clients.append(client)
                stack.callback(client.close)
                resumed = client.rpc('thread/resume',{'threadId':thread_id})
                assert resumed['model'] == PARENT, resumed
                turn(client,upstream,thread_id,'resume')
                child_requests=[row['body'] for row in upstream.requests if row['body']['model']=='worker']
                assert any(MESSAGE in json.dumps(row) and FOLLOWUP in json.dumps(row) for row in child_requests)
            assert not upstream.errors, upstream.errors
            assert_plaintext(agent_messages(home), mode)
            with contextlib.closing(sqlite3.connect((state/'usage.sqlite3').as_uri()+'?mode=ro',uri=True)) as ledger:
                calls = [json.loads(row[0]) for row in ledger.execute('SELECT facts FROM call_records')]
            assert not any(row.get('route')=='agent_task_recovery' for row in calls), calls
            report = {'protocol':protocol,'mode':mode,'passed':True,
                      'child_completed':mode!='off','resume_completed':mode!='off',
                      'plaintext_messages_verified':True,'recovery_calls':0,
                      'child_requests':sum(row['body']['model']=='worker' for row in upstream.requests)}
            (evidence/'result.json').write_text(json.dumps(report,indent=2))
            return report
        finally:
            (evidence/'requests.json').write_text(json.dumps(upstream.requests,ensure_ascii=False,indent=2))
            (evidence/'events.json').write_text(json.dumps([event for server in clients for event in server.events],ensure_ascii=False,indent=2))
            (evidence/'agent-messages.json').write_text(json.dumps(agent_messages(home),ensure_ascii=False,indent=2))


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex',type=Path,required=True)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--protocol',choices=['responses','chat_completions','anthropic_messages'],action='append')
    parser.add_argument('--mode',choices=['off','on_demand','on'],action='append')
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='emp-code-mode-') as temp:
        version_env={'PATH':os.environ['PATH'],'HOME':temp,'CODEX_HOME':temp}
        report={'real_model_calls':0,'codex_version':subprocess.check_output([str(args.codex),'--version'],env=version_env,text=True,stderr=subprocess.DEVNULL).strip(),
                'binary_sha256':{str(path):hashlib.file_digest(path.open('rb'),'sha256').hexdigest()
                                 for path in (args.codex,args.codex.with_name('codex-code-mode-host'),args.emp)},
                'cases':[]}
        for protocol in args.protocol or ['responses','chat_completions','anthropic_messages']:
            for mode in args.mode or ['on_demand','off','on']:
                result=case(args.emp.resolve(),args.codex.resolve(),Path(temp)/(protocol+'-'+mode),args.output,protocol,mode)
                report['cases'].append(result)
                (args.output/'result.json').write_text(json.dumps(report,indent=2))
                print(json.dumps(result),flush=True)


if __name__ == '__main__':
    main()
