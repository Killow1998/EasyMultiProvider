"""Disposable gateway/accounting/aggregation E2E. All upstream replies are synthetic."""
import argparse
import contextlib
import json
import socket
import sqlite3
import struct
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from http.client import HTTPConnection
from pathlib import Path
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).parent / 'shadow_frontend'))
from local_backend import request, start_backend


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    observed = []
    held, cancelled = threading.Event(), threading.Event()

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            source = self.headers['Authorization'].removeprefix('Bearer fixture-')
            observed.append((self.path, source, body))
            if body.get('state',{}).get('hold'):
                self.send_response(200)
                self.send_header('Content-Length','100')
                self.end_headers()
                held.set()
                if not self.rfile.read(1):
                    cancelled.set()
                return
            status = body.get('state', {}).get('status', 200)
            if self.path.endswith(('/decisions', '/systemone')):
                result = {'id': 'decision-' + str(len(observed)), 'model': body['model'], 'answers': {'effort': 'low'}}
                if 'usage' in body.get('state', {}):
                    result['usage'] = body['state']['usage']
                if status != 200:
                    result = {'error': {'code': 'rate_limit', 'message': 'synthetic quota'}}
            else:
                result = {'id': 'chat-' + str(len(observed)), 'object': 'chat.completion', 'model': 'fixture',
                    'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'source-' + source}, 'finish_reason': 'stop'}],
                    'usage': {'prompt_tokens': 10, 'completion_tokens': 2, 'total_tokens': 12}}
            raw = json.dumps(result).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(raw)))
            if status == 429:
                self.send_header('Retry-After', '7')
            self.end_headers()
            self.wfile.write(raw)

    checks = []
    with contextlib.ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-gateway-e2e-')))
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        sources = [f's{n}/fixture' for n in range(1, 4)]
        config = {'providers': [{'id': f's{n}', 'name': f'Fixture {n}', 'base_url': f'http://127.0.0.1:{upstream.server_port}/v1',
            'protocol': 'chat_completions', 'auth_mode': 'api_key', 'api_key': f'fixture-{n}'} for n in range(1, 4)],
            'models': [{'id': source, 'provider': f's{n}', 'upstream_id': 'fixture', 'enabled': True} for n, source in enumerate(sources, 1)],
            'model_groups': [{'id': 'auto/' + policy, 'name': policy, 'policy': policy, 'sources': sources} for policy in ['priority', 'round_robin']]}
        base, _, state, management = start_backend(stack, args.binary.resolve(), root, config)
        management['Origin'] = base
        clients = []
        for name in ['Desktop fixture', 'Forwarded laptop fixture']:
            status, client = request(base + '/api/gateway/clients', {'name': name}, management)
            assert status == 200, client
            clients.append(client)
        listed = request(base + '/api/gateway/clients', headers=management)[1]
        disk = (state / 'gateway-clients.json').read_text()
        assert all(client['api_key'] not in disk and client['api_key'] not in json.dumps(listed) for client in clients)
        checks.append('named keys stored as digests and disclosed only at creation')
        headers = [{'Authorization': 'Bearer ' + client['api_key'], 'Host': '192.0.2.20:14200',
            'X-EMP-Caller-ID': 'forged', 'X-EMP-Peer-IP': '203.0.113.99'} for client in clients]
        for header in headers:
            assert request(base + '/v1/models', headers=header)[0] == 200
            assert request(base + '/api/config', headers=header)[0] in (401, 403)
        checks.append('forwarded callers can infer but cannot open management')
        decisions = [(0, {'input_tokens': 5, 'output_tokens': 2}), (0, {'input_tokens': 0, 'output_tokens': 0}),
            (1, {'input_tokens': 3}), (1, None), (1, None)]
        for index, (client, usage) in enumerate(decisions):
            supplied = {} if usage is None else {'usage': usage}
            if index == 4:
                supplied['status'] = 429
            path = '/v1/systemone' if index == 2 else '/v1/decisions'
            payload = {'model': 'fixture-jev', 'state': supplied}
            auth = {**headers[client], 'X-EMP-Provider': 's1','session-id':'public-parent-thread'}
            if index == 4:
                target = urlsplit(base)
                connection = stack.enter_context(contextlib.closing(HTTPConnection(target.hostname,target.port,timeout=10)))
                connection.request('POST',path,json.dumps(payload),{'Content-Type':'application/json',**auth})
                response = connection.getresponse()
                status, result = response.status, json.loads(response.read())
                assert response.getheader('Retry-After') == '7'
                assert result == {'error': {'code': 'rate_limit', 'message': 'synthetic quota'}}
            else:
                status, result = request(base + path,payload,auth)
            assert status == (429 if index == 4 else 200), result
            if usage is not None:
                assert result['usage'] == usage
        assert len(observed) == len(decisions), observed
        checks.append('Decision body, missing usage, zero usage and HTTP failure preserved without retry')
        with sqlite3.connect(state / 'usage.sqlite3') as database:
            assert database.execute("SELECT COUNT(*) FROM call_records WHERE operation='decisions' AND session_id<>''").fetchone()[0] == 0, 'Decision leaked into its parent conversation bill'
            deliveries = [json.loads(value) if value else None for value, in database.execute(
                "SELECT delivery FROM call_records WHERE operation='decisions' ORDER BY started_at")]
            assert len(deliveries) == len(decisions) and all(
                item and item['delivery']=='delivered' and item['writes_completed']==1
                for item in deliveries), ('Decision delivery receipts missing',deliveries)
            assert [item['response_status'] for item in deliveries] == [200,200,200,200,429]
        checks.append('Decision calls stay out of the parent conversation bill')
        checks.append('Decision delivery receipts persist both success and upstream error responses')
        status, report = request(base + '/api/gateway/usage', headers=management)
        assert status == 200, report
        assert sum(row['calls'] for row in report['rows']) == 5, report
        by_id = {row['caller_id']: row for row in report['rows']}
        a, b = (by_id[client['id']] for client in clients)
        assert (a['calls'], a['input_tokens'], a['output_tokens'], a['reported_calls']) == (2, 5, 2, 2), a
        assert (b['calls'], b['input_tokens'], b['output_tokens'], b['reported_calls']) == (3, 3, None, 0), b
        assert a['cost_nanos'] is None and b['cost_nanos'] is None, report
        checks.append('same socket peer split by authenticated key; unknown usage and cost remain unknown')
        for policy, sessions, expected in [('priority', ['a', 'b'], [1, 1]),
                ('round_robin', ['a', 'a', 'b', 'c', 'd'], [1, 1, 2, 3, 1])]:
            for session, source in zip(sessions, expected):
                status, reply = request(base + '/v1/responses', {'model': 'auto/' + policy,
                    'input': [{'role': 'user', 'content': 'public fixture'}]}, {**headers[0], 'session-id': session})
                assert status == 200, reply
                assert reply['output'][0]['content'][0]['text'] == 'source-' + str(source), reply
        checks.append('priority and new-conversation round robin keep existing conversation affinity')
        current = request(base + '/api/config', headers=management)[1]
        current['model_groups'][0]['sources'] = list(reversed(sources))
        status, saved = request(base + '/api/config', current, management)
        assert status == 200, saved
        for session, expected in [('a', 1), ('new-after-reorder', 3)]:
            status, reply = request(base + '/v1/responses', {'model': 'auto/priority',
                'input': [{'role': 'user', 'content': 'public fixture'}]}, {**headers[0], 'session-id': session})
            assert status == 200 and reply['output'][0]['content'][0]['text'] == f'source-{expected}', reply
        checks.append('editing priority order affects new conversations only')
        target = urlsplit(base)
        connection = HTTPConnection(target.hostname,target.port,timeout=10)
        connection.request('POST','/v1/decisions',json.dumps({'model':'fixture-jev','state':{'hold':True}}),
            {'Content-Type':'application/json',**headers[0],'X-EMP-Provider':'s1'})
        assert held.wait(10),'Decision did not reach the synthetic upstream'
        connection.close()
        assert cancelled.wait(5),'Decision kept reading after its caller disconnected'
        checks.append('Decision disconnect drops the in-flight upstream without retry')
        address = urlsplit(base)
        websocket = stack.enter_context(socket.create_connection((address.hostname,address.port),timeout=10))
        reader = stack.enter_context(websocket.makefile('rb'))
        websocket.sendall(('GET /v1/responses HTTP/1.1\r\nHost: '+address.netloc+'\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
            'Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: MDEyMzQ1Njc4OWFiY2RlZg==\r\nAuthorization: Bearer '+clients[1]['api_key']+'\r\n\r\n').encode())
        assert reader.readline().startswith(b'HTTP/1.1 101')
        while reader.readline() != b'\r\n':
            pass
        status, removed = request(base + '/api/gateway/clients', {'id': clients[1]['id']}, management, method='DELETE')
        assert status == 200, removed
        assert request(base + '/v1/models', headers=headers[1])[0] in (401, 403)
        message = json.dumps({'type':'response.create','model':'auto/priority','input':'public fixture'}).encode()
        mask = b'abcd'
        websocket.sendall(bytes([0x81,0x80|len(message)])+mask+bytes(value^mask[index%4] for index,value in enumerate(message)))
        frame = reader.read(2)
        assert frame[0]&15 == 8, frame
        closed = reader.read(frame[1]&127)
        assert struct.unpack('>H',closed[:2])[0] == 1008, closed
        checks.append('revocation rejects subsequent HTTP and already-connected WebSocket requests')
    result = {'passed': True, 'real_provider_calls': 0, 'checks': checks, 'upstream_requests': len(observed)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == '__main__':
    main()
