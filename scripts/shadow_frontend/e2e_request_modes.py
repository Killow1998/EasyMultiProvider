"""Verify selected effort and confirmed tier through real EMP requests and reports."""
import argparse
import base64
import contextlib
import hashlib
import json
import socket
import sys
import tempfile
import urllib.request
from collections import Counter
from pathlib import Path
from unittest.mock import patch
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.codex_feature_acceptance import fixture as fixtures
from candidate_web import candidate_frontend
from e2e_refinements import Browser
from local_backend import request, wait_for


def websocket_request(base, headers, body):
    address = urlsplit(base)
    with socket.create_connection((address.hostname, address.port), timeout=15) as sock:
        headers = {**headers, 'Host':address.netloc, 'Upgrade':'websocket', 'Connection':'Upgrade',
                   'Sec-WebSocket-Version':'13', 'Sec-WebSocket-Key':base64.b64encode(b'fixture-only-key').decode()}
        sock.sendall(('GET /v1/responses HTTP/1.1\r\n'+''.join(f'{key}: {value}\r\n' for key,value in headers.items())+'\r\n').encode())
        with sock.makefile('rb') as source:
            status = source.readline()
            assert b' 101 ' in status, status
            while source.readline() != b'\r\n':
                pass
            payload = json.dumps({'type':'response.create', **body}).encode()
            mask = b'test'
            sock.sendall(bytes([129,254])+len(payload).to_bytes(2,'big')+mask+
                         bytes(byte ^ mask[index % 4] for index,byte in enumerate(payload)))
            while True:
                prefix = source.read(2)
                assert prefix and prefix[0] & 15 == 1, prefix
                size = prefix[1] & 127
                if size in (126,127):
                    size = int.from_bytes(source.read(2 if size == 126 else 8),'big')
                event = json.loads(source.read(size))
                assert event['type'] not in ('error','response.failed'), event
                if event['type'] == 'response.completed':
                    return


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    cases, checks = [], []
    original_events, original_post = fixtures.response_events, fixtures.Upstream.do_POST
    selected, returned, with_usage, reported_effort = None, None, True, 'xhigh'

    def events(*arguments):
        for event in original_events(*arguments):
            if 'response' in event:
                response = event['response']
                response['reasoning'] = {'effort':reported_effort}
                if returned is not None:
                    response['service_tier'] = returned
                if not with_usage:
                    response.pop('usage', None)
            yield event

    def post(handler):
        if handler.path.endswith('/messages'):
            return original_post(handler)
        raw = handler.rfile.read(int(handler.headers['Content-Length']))
        if handler.headers.get('Content-Encoding') == 'zstd':
            from compression import zstd
            raw = zstd.decompress(raw)
        body = json.loads(raw)
        identity = 'response-'+str(next(handler.server.sequence))
        handler.server.requests.append({'body':body,'path':handler.path,'response_id':identity})
        stream = list(events(fixtures.message(), identity))
        response = stream[-1]['response']
        chat = handler.path.endswith('/chat/completions')
        if chat:
            response = {'id':identity,'model':'probe','choices':[{'index':0,
                'message':{'role':'assistant','content':'FIXTURE_COMPLETE'},'finish_reason':'stop'}]}
            if returned is not None:
                response['service_tier'] = returned
            if with_usage:
                response['usage'] = {'prompt_tokens':10,'completion_tokens':3,'total_tokens':13}
            stream = [{**response, 'choices':[{'index':0,'delta':{'content':'FIXTURE_COMPLETE'},'finish_reason':'stop'}]}]
        payload = (b''.join(b'data: '+json.dumps(event).encode()+b'\n\n' for event in stream)
                   if body.get('stream') else json.dumps(response).encode())
        if chat and body.get('stream'):
            payload += b'data: [DONE]\n\n'
        handler.send_response(200)
        handler.send_header('Content-Type','text/event-stream' if body.get('stream') else 'application/json')
        handler.send_header('Content-Length',str(len(payload)))
        handler.end_headers()
        handler.wfile.write(payload)

    with tempfile.TemporaryDirectory(prefix='emp-request-modes-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        stack.enter_context(patch.object(fixtures, 'response_events', events))
        stack.enter_context(patch.object(fixtures.Upstream, 'do_POST', post))
        fixture = stack.enter_context(fixtures.Fixture(None, args.emp.resolve(), root/'fixture', root/'evidence'))
        fixture.import_account()
        headers = {**fixture.headers,'Content-Type':'application/json','Authorization':'Bearer fixture-only',
                   'chatgpt-account-id':'fixture-account'}
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for model in ['gpt-6.1-sol','imported/gpt-6.1-sol','responses/probe','chat/probe']:
            for transport in ['http','sse'] + ([] if model.startswith('chat/') else ['websocket']):
                for index,(selected,returned,with_usage) in enumerate([
                    ('default','default',True),('priority','priority',True),('priority','default',True),
                    ('default','priority',True),(None,'priority',True),('priority',None,True),
                    ('priority','default',False),
                ]):
                    choice = 'ultra' if index % 2 == 0 else 'low'
                    reported_effort = 'xhigh' if choice == 'ultra' else 'low'
                    body = {'model':model,'stream':transport!='http', 'reasoning':{'effort':'high'},
                            'input':[{'role':'user','content':'FIXTURE_REQUEST_MODES'}],
                            'client_metadata':{'x-codex-turn-metadata':json.dumps({'reasoning_effort':choice})}}
                    if selected is not None:
                        body['service_tier'] = selected
                    if transport == 'websocket':
                        websocket_request(fixture.base, headers, body)
                    else:
                        call = urllib.request.Request(fixture.base+'/v1/responses',json.dumps(body).encode(),headers)
                        with opener.open(call, timeout=15) as response:
                            payload = response.read()
                            assert response.status == 200 and b'FIXTURE_COMPLETE' in payload, payload
                    report = wait_for(lambda: (result if (result := request(fixture.base+'/api/calls?limit=100', headers=fixture.headers)[1])['total'] == len(cases)+1 else None))
                    row = report['records'][0]
                    mode = 'unknown' if returned is None else 'fast' if returned == 'priority' else 'standard'
                    expected = {'requested_service_tier':selected,'reported_service_tier':returned,
                                'speed_mode':mode,'requested_effort':choice,
                                'reported_effort':None if model.startswith('chat/') else reported_effort}
                    assert row['state'] == 'completed', row
                    assert all(row.get(key) == value for key,value in expected.items()), (model,transport,expected,row)
                    cases.append({'model':model,'transport':transport,'usage_reported':with_usage,
                                  'request_id':row['request_id'],**expected})
        expected_groups = Counter((row['model_id'],row['speed_mode'],row['requested_effort'],row.get('reported_effort')) for row in report['records'])
        actual_groups = {(row['model_id'],row['speed_mode'],row['requested_effort'],row.get('reported_effort')):row['calls'] for row in report['models']}
        assert expected_groups == actual_groups, (expected_groups,actual_groups)
        assert len(fixture.upstream.requests) == len(cases), 'Unexpected retry or duplicate execution'
        checks.append('Native, imported and external Responses HTTP/SSE/WebSocket plus Chat HTTP/SSE retain selected effort and group by confirmed tier, even without usage')
        checks.append('No duplicate upstream requests; model-group counts match persisted call details')

        web = Path(__file__).resolve().parents[2]/'crates/emp-app/web'
        head = '<script>localStorage.setItem("emp_management_session_v1",'+json.dumps(fixture.headers['X-EMP-Session'])+');</script>'
        frontend, _ = candidate_frontend(stack, fixture.base, web, index_head=head)
        browser = Browser(stack, root)
        sample = next(row for row in cases if row['model']=='gpt-6.1-sol' and row['transport']=='sse' and row['requested_service_tier']=='priority' and row['reported_service_tier']=='default' and row['usage_reported'])
        for language,width in [('zh-CN',390),('en',1280)]:
            browser.navigate(frontend)
            request(browser.base+'/window/rect',{'width':width,'height':1000})
            browser.execute('setLanguage(arguments[0]);statistics.open();',language)
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-stats-view=calls]");'))
            browser.execute('document.querySelector("[data-stats-view=calls]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-call-id]");'))
            browser.execute('document.querySelector("[data-call-action=next]").click();')
            selector = '[data-call-id="'+sample['request_id']+':responses"]'
            wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);',selector))
            data = browser.execute('''
                const record=document.querySelector(arguments[0]);record.open=true;
                const fields=[...record.querySelectorAll('.call-fact')].map(node=>[node.querySelector('span').textContent,node.querySelector('strong').textContent]);
                record.scrollIntoView({block:'center'});return {fields};
            ''',selector)
            values = [value for _,value in data['fields']]
            assert 'ultra' in values and 'xhigh' in values and any('Fast' in value for value in values) and any('Standard' in value for value in values), data
            _, png = request(browser.base+'/screenshot')
            args.output.with_suffix('.'+language+'.png').write_bytes(base64.b64decode(png['value']))
            checks.append(f'{language}: actual call details show Ultra → xhigh and requested Fast / reported Standard at {width}px')
    args.output.write_text(json.dumps({'passed':True,'real_provider_calls':0,'cases':cases,'checks':checks,
        'binary_sha256':hashlib.file_digest(args.emp.open('rb'),'sha256').hexdigest()},indent=2)+'\n')
    print(json.dumps({'passed':True,'cases':len(cases),'checks':checks}),flush=True)


if __name__ == '__main__':
    main()
