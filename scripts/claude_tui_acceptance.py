"""Real installed Claude TUI, fake Messages upstream, and a Codex-shaped tool loop."""
import argparse
import base64
import contextlib
import json
import os
import random
import socket
from pathlib import Path
import sys
import tempfile
import threading
import struct
import zlib
from http.client import HTTPConnection
from urllib.parse import urlsplit
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).parent / 'shadow_frontend'))
from local_backend import request, start_backend


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--large-image', action='store_true')
    parser.add_argument('--large-context', action='store_true')
    parser.add_argument('--case',choices=['tools','http401','http429','http503','http504','sse-error','stream-error','stream-cancel','cancel','budget'],default='tools')
    parser.add_argument('--codex',type=Path)
    parser.add_argument('--transport', choices=['json','sse','websocket'], default='json')
    args = parser.parse_args()
    observed = []
    held,cancelled = threading.Event(),threading.Event()
    visible = threading.Event()
    image = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC'
    if args.large_image:
        def chunk(kind, data):
            return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
        pixels = random.Random(5).randbytes(2048*1024*3)
        raw = b''.join(b'\0'+pixels[row*6144:(row+1)*6144] for row in range(1024))
        image = base64.b64encode(b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',2048,1024,8,2,0,0,0))
            +chunk(b'IDAT',zlib.compress(raw))+chunk(b'IEND',b'')).decode()

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            observed.append(body)
            if args.case=='sse-error':
                raw=b'event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"PUBLIC_FIXTURE_ERROR"}}\n\n'
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(raw)))
                self.end_headers();self.wfile.write(raw);return
            if args.case.startswith('http'):
                code=int(args.case[4:]);raw=json.dumps({'type':'error','error':{'type':'authentication_error' if code==401 else 'rate_limit_error','message':'PUBLIC_FIXTURE_ERROR'}}).encode()
                self.send_response(code);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)))
                if code==429:self.send_header('Retry-After','7')
                self.end_headers();self.wfile.write(raw);return
            if args.case == 'cancel':
                self.send_response(200);self.send_header('Content-Length','100');self.end_headers();held.set()
                if not self.rfile.read(1):cancelled.set()
                return
            tools = [tool['name'] for tool in body.get('tools', []) if tool['name'].endswith('__codex')]
            assert tools, body.get('tools')
            has_context = 'PUBLIC_TASK_MARKER' in json.dumps(body.get('messages'))
            has_result = 'PUBLIC_TOOL_RESULT' in json.dumps(body.get('messages'))
            assert has_context,'MCP prompt context did not reach the upstream request'
            if not has_result:
                name, arguments = tools[0], {'answer': '', 'tool_calls': [{'type': 'function_call',
                    'name': 'read_public', 'namespace':'', 'input':'', 'arguments': {'path': 'fixture.txt'}}]}
            else:
                name, arguments = None, None
            block = {'type': 'tool_use', 'id': f'toolu_{len(observed)}', 'name': name, 'input': {}} if name else {'type': 'text', 'text': ''}
            delta = {'type': 'input_json_delta', 'partial_json': json.dumps(arguments)} if name else {'type': 'text_delta', 'text': 'PUBLIC_FINAL_ANSWER'}
            events = [
                {'type': 'message_start', 'message': {'id': f'msg_{len(observed)}', 'type': 'message',
                    'role': 'assistant', 'model': body['model'], 'content': [], 'stop_reason': None,
                    'stop_sequence': None, 'usage': {'input_tokens': 17, 'cache_read_input_tokens': 0, 'cache_creation_input_tokens': 3,
                        'cache_creation':{'ephemeral_5m_input_tokens':1,'ephemeral_1h_input_tokens':2},'output_tokens': 1}}},
                {'type': 'content_block_start', 'index': 0, 'content_block': block},
                {'type': 'content_block_delta', 'index': 0, 'delta': delta},
                {'type': 'content_block_stop', 'index': 0},
                {'type': 'message_delta', 'delta': {'stop_reason': 'max_tokens' if args.case=='budget' else 'tool_use' if name else 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 5}},
                {'type': 'message_stop'},
            ]
            if args.transport != 'json':
                visible.clear()
                prefix = [
                    {'type':'content_block_start','index':0,'content_block':{'type':'thinking','thinking':''}},
                    {'type':'content_block_delta','index':0,'delta':{'type':'thinking_delta','thinking':'PRIVATE_THINKING_MARKER'}},
                    {'type':'content_block_stop','index':0},
                ]
                if name:
                    prefix += [
                        {'type':'content_block_start','index':1,'content_block':{'type':'text','text':''}},
                        {'type':'content_block_delta','index':1,'delta':{'type':'text_delta','text':'PUBLIC_PROGRESS'}},
                        {'type':'content_block_stop','index':1},
                    ]
                for event in events[1:]:
                    if 'index' in event: event['index'] += 2 if name else 1
                events[1:1] = prefix
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Connection','close');self.end_headers()
                for event in events:
                    wire=f'event: {event["type"]}\ndata: {json.dumps(event)}\n\n'.encode()
                    self.wfile.write(wire[:17]);self.wfile.flush();self.wfile.write(wire[17:]);self.wfile.flush()
                    if event.get('delta',{}).get('type') == 'text_delta':
                        assert visible.wait(10), 'Public text was buffered until the Claude turn finished'
                        if args.case == 'stream-error':
                            self.wfile.write(b'event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"PUBLIC_FIXTURE_ERROR"}}\n\n');self.wfile.flush()
                            break
                        if args.case == 'stream-cancel':
                            if not self.rfile.read(1):cancelled.set()
                            break
                self.close_connection=True
                return
            raw = ''.join(f'event: {event["type"]}\ndata: {json.dumps(event)}\n\n' for event in events).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    report = {'scope': 'real installed Claude TUI; synthetic upstream and Codex-shaped caller', 'real_provider_calls': 0}
    with contextlib.ExitStack() as stack:
        directory = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-tui-acceptance-')))
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        config = {'providers': [{'id': 'cpa', 'name': 'Fixture CPA',
            'base_url': f'http://127.0.0.1:{upstream.server_port}/v1', 'protocol': 'anthropic_messages',
            'auth_mode': 'api_key', 'api_key': 'fixture-only', 'execution_backend': 'claude_cli'}],
            'models': [{'id': 'cpa/sonnet', 'provider': 'cpa', 'upstream_id': 'sonnet', 'enabled': True,
                'context_window': 16_000_000}]}
        base, _, state, management = start_backend(stack, args.binary.resolve(), directory, config,
            {'PATH': os.environ['PATH']})
        key = json.loads((state / 'gateway-clients.json').read_text()) if (state / 'gateway-clients.json').exists() else []
        assert not key
        gateway = (state / 'gateway-key').read_text()
        headers = {'Authorization': 'Bearer ' + gateway, 'session-id': '11111111-1111-4111-8111-111111111111'}
        document=base64.b64encode(b'%PDF-1.4\n% public fixture\n%%EOF').decode()
        body = {'model': 'cpa/sonnet','reasoning':{'effort':'high'}, 'input': [{'role': 'user', 'content': [
            {'type':'input_text','text':'PUBLIC_TASK_MARKER: read the fixture and answer.'},
            {'type':'input_image','image_url':'data:image/png;base64,'+image},
            {'type':'input_file','filename':'public.pdf','file_data':'data:application/pdf;base64,'+document}]}],
            'tools': [{'type': 'function', 'name': 'read_public', 'parameters': {'type': 'object', 'properties': {'path': {'type': 'string'}}}}]}
        if args.large_context:
            body['input'][0]['content'].insert(1,{'type':'input_text','text':'public-padding '.ljust(11*1024*1024,'x')})
        if args.case == 'cancel':
            address=urlsplit(base);connection=HTTPConnection(address.hostname,address.port,timeout=10)
            connection.request('POST','/v1/responses',json.dumps(body),{'Content-Type':'application/json',**headers})
            assert held.wait(30),'Claude request did not reach its synthetic upstream'
            connection.close()
            assert cancelled.wait(5),'Disconnected caller left its Claude request running'
            report.update(passed=True,case='cancel',upstream_requests=len(observed),upstream_cancelled=True)
            args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2));print(json.dumps(report));return
        def complete(body):
            if args.transport == 'json': return request(base+'/v1/responses',body,headers,timeout=65)
            address=urlsplit(base);events=[]
            def accept(event):
                events.append(event)
                if args.case=='stream-error' and event['type']=='error':
                    assert any(item['type']=='response.output_text.delta' for item in events),events
                    assert event['error']['code']=='upstream_capacity',event
                    return event
                assert event['type'] not in ('error','response.failed'),event
                if event['type']=='response.output_text.delta':
                    visible.set()
                    if args.case=='stream-cancel':return {'cancelled':True}
                if event['type']=='response.completed':
                    combined=json.dumps(events)
                    assert 'PRIVATE_THINKING_MARKER' not in combined,combined
                    deltas=''.join(item.get('delta','') for item in events if item['type']=='response.output_text.delta')
                    expected='PUBLIC_PROGRESS' if 'PUBLIC_TOOL_RESULT' not in json.dumps(body['input']) else 'PUBLIC_FINAL_ANSWER'
                    assert deltas==expected,(deltas,expected)
                    return event['response']
            if args.transport=='sse':
                connection=HTTPConnection(address.hostname,address.port,timeout=65)
                connection.request('POST','/v1/responses',json.dumps({**body,'stream':True}),{'Content-Type':'application/json',**headers})
                response=connection.getresponse()
                assert response.status==200,response.read() if response.status!=200 else ''
                while line:=response.readline():
                    if line.startswith(b'data: '):
                        final=accept(json.loads(line[6:]))
                        if final is not None:
                            if args.case=='stream-cancel':response.close()
                            connection.close();return 200,final
            else:
                with socket.create_connection((address.hostname,address.port),timeout=65) as sock:
                    upgrade={**headers,'Host':address.netloc,'Upgrade':'websocket','Connection':'Upgrade','Sec-WebSocket-Version':'13','Sec-WebSocket-Key':base64.b64encode(b'fixture-only-key').decode()}
                    sock.sendall(('GET /v1/responses HTTP/1.1\r\n'+''.join(f'{key}: {value}\r\n' for key,value in upgrade.items())+'\r\n').encode())
                    with sock.makefile('rb') as source:
                        assert b' 101 ' in source.readline()
                        while source.readline()!=b'\r\n': pass
                        payload=json.dumps({'type':'response.create',**body}).encode();mask=b'test'
                        sock.sendall(bytes([129,254])+len(payload).to_bytes(2,'big')+mask+bytes(byte^mask[index%4] for index,byte in enumerate(payload)))
                        while True:
                            prefix=source.read(2);assert prefix and prefix[0]&15==1,prefix
                            size=prefix[1]&127
                            if size in (126,127): size=int.from_bytes(source.read(2 if size==126 else 8),'big')
                            final=accept(json.loads(source.read(size)))
                            if final is not None:return 200,final
            raise AssertionError('Missing terminal response')
        try:
            if args.case.startswith('http'):
                address=urlsplit(base);connection=HTTPConnection(address.hostname,address.port,timeout=65)
                connection.request('POST','/v1/responses',json.dumps(body),{'Content-Type':'application/json',**headers})
                response=connection.getresponse();status=response.status;first=json.loads(response.read())
                if args.case=='http429':assert response.getheader('Retry-After')=='7',dict(response.getheaders())
                connection.close()
            else:
                status, first = complete(body)
        except Exception as error:
            report.update(passed=False, failure=type(error).__name__, upstream_requests=len(observed))
            for name in ['backend']:
                path = directory/(name+'.log')
                if path.is_file():
                    with path.open('rb') as source:
                        source.seek(max(0,path.stat().st_size-8192))
                        report[name] = source.read().decode('utf-8',errors='replace')
            args.output.parent.mkdir(parents=True,exist_ok=True)
            args.output.write_text(json.dumps(report,indent=2))
            raise
        report['first_status'] = status
        if args.case in ('stream-error','stream-cancel'):
            if args.case=='stream-cancel':assert cancelled.wait(5),'Upstream continued after disconnect'
            assert len(observed)==1,len(observed)
            report.update(passed=True,case=args.case,transport=args.transport,early_text_verified=True,upstream_requests=1)
            args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2));print(json.dumps(report));return
        if args.case != 'tools':
            report.update(passed=False,case=args.case,upstream_requests=len(observed))
            args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2))
            expected=int(args.case[4:]) if args.case.startswith('http') else 502
            assert status==expected and len(observed)==1,(status,first,len(observed))
            if args.case=='budget':assert first['error']['code']=='claude_cli_output_budget_exhausted',first
            if args.case=='sse-error':assert first['error']['code']=='upstream_capacity',first
            report.update(passed=True,case=args.case,upstream_requests=1)
            args.output.parent.mkdir(parents=True,exist_ok=True);args.output.write_text(json.dumps(report,indent=2));print(json.dumps(report));return
        if status != 200:
            report['failure'] = first
            report['upstream_requests'] = len(observed)
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2))
            raise AssertionError(report)
        call = next(item for item in first['output'] if item['type'] == 'function_call')
        body['input'] += first['output'] + [{'type': 'function_call_output', 'call_id': call['call_id'], 'output': 'PUBLIC_TOOL_RESULT'}]
        status, final = complete(body)
        assert status == 200, final
        assert final['output'][0]['content'][0]['text'] == 'PUBLIC_FINAL_ANSWER', final
        assert len(observed) == 2, len(observed)
        assert first['usage']['input_tokens'] == 20, first
        assert final['usage']['input_tokens'] == 20, final
        assert first['usage']['input_tokens_details']['cache_creation_tokens']==3,first
        assert first['usage']['input_tokens_details']['cache_creation_1h_tokens']==2,first
        assert any(item['role']=='assistant' for item in observed[1]['messages']), 'Tool continuation reset the CLI'
        body['input'] += final['output'] + [{'role':'user','content':'PUBLIC_NEXT_USER_TURN'}]
        status,next_turn = complete(body)
        assert status == 200 and next_turn['output'][0]['content'][0]['text'] == 'PUBLIC_FINAL_ANSWER',next_turn
        assert len(observed) == 3
        assert all(item.get('output_config',{}).get('effort')=='high' for item in observed),[item.get('output_config') for item in observed]
        assert not any(item['role']=='assistant' for item in observed[2]['messages']), 'A new user turn retained Claude session history'
        system_text = [' '.join(part.get('text', '') for part in body['system']) if isinstance(body['system'], list) else body['system'] for body in observed]
        assert all('cc_entrypoint=cli' in value for value in system_text)
        report.update(passed=True, transport=args.transport, early_text_verified=args.transport!='json', upstream_requests=len(observed), tool_call_count=1,
            selected_effort='high',cache_creation_1h_tokens=first['usage']['input_tokens_details']['cache_creation_1h_tokens'],
            user_turn_reset=True, tool_round_reuse=True,
            per_request_input_tokens=[first['usage']['input_tokens'], final['usage']['input_tokens'],next_turn['usage']['input_tokens']],
            entrypoint='cli', final_answer='PUBLIC_FINAL_ANSWER')
        def media(value,kind,expected,path=''):
            if isinstance(value,dict):
                if value.get('type') == kind:
                    yield {'path':path,'unchanged':value.get('source',{}).get('data') == expected}
                for key,item in value.items():
                    yield from media(item,kind,expected,path+'/'+key)
            elif isinstance(value,list):
                for index,item in enumerate(value):
                    yield from media(item,kind,expected,path+'/'+str(index))
        report['images'] = [list(media(body['messages'],'image',image)) for body in observed]
        report['documents'] = [list(media(body['messages'],'document',document)) for body in observed]
        report['media_exact'] = all(len(parts) == 1 and parts[0]['unchanged'] for parts in report['images']+report['documents'])
        report['passed'] = report['media_exact']
        report['large_image'] = args.large_image
        report['large_context'] = args.large_context
        if args.codex:
            from claude_codex_acceptance import run
            shaped_calls=len(observed)
            report['real_codex']=run(args.codex.resolve(),base,headers,directory,args.output.parent/'real-codex',
                visible.set if args.transport!='json' else None)
            actual=observed[shaped_calls:]
            assert len(actual)==4,len(actual)
            assert any(item['role']=='assistant' for item in actual[1]['messages']), 'Real Codex tool continuation lost CLI affinity'
            assert all(not any(item['role']=='assistant' for item in record['messages']) for record in [actual[0],actual[2],actual[3]])
            report['real_codex']['upstream_requests']=len(actual)
            report['shaped_caller_requests']=shaped_calls
            report['upstream_requests']=len(observed)
            report['tool_call_count']+=report['real_codex']['tool_calls']
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2))
        assert report['passed'], report
    print(json.dumps(report))


if __name__ == '__main__':
    main()
