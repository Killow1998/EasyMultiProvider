"""Exercise preset setup and retry explanations with real local request outcomes."""
import argparse
import base64
import contextlib
import json
import socket
import tempfile
import threading
import time
import urllib.request
from collections import Counter
from compression import zstd
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_refinements import Browser
from e2e_environment import request, start_backend, wait_for


class Upstream(BaseHTTPRequestHandler):
    calls = Counter()
    models = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            raw = zstd.decompress(raw)
        body = json.loads(raw)
        text = json.dumps(body.get('input'))
        case = next(value for value in ['wait-advice','retry-once','partial-output','cancel-backoff','preset-save'] if value in text)
        self.calls[case] += 1
        self.models.append(body['model'])
        if case in ['wait-advice','cancel-backoff'] or (case == 'retry-once' and self.calls[case] == 1):
            seconds = 301 if case == 'wait-advice' else 5 if case == 'cancel-backoff' else 1
            self.send_response(429)
            self.send_header('Content-Type','application/json')
            self.send_header('Retry-After',str(seconds))
            self.end_headers()
            self.wfile.write(b'{"error":{"type":"rate_limit_error","message":"private-fixture-marker"}}')
            return
        if case == 'partial-output':
            self.send_response(200)
            self.send_header('Content-Type','text/event-stream')
            self.send_header('Connection','close')
            self.end_headers()
            for event in [{'type':'response.created','response':{'id':'partial','model':'fixture-model','status':'in_progress'}},
                          {'type':'response.output_text.delta','delta':'partial fixture output'}]:
                self.wfile.write(('data: '+json.dumps(event)+'\n\n').encode())
                self.wfile.flush()
            return
        if case == 'preset-save':
            assert self.headers.get('Authorization') == 'Bearer fixture-preset-key'
        self.send_response(200)
        self.send_header('Content-Type','application/json')
        self.end_headers()
        self.wfile.write(json.dumps({'id':case,'object':'response','model':'fixture-model',
            'status':'completed','output':[{'id':'msg','type':'message','role':'assistant',
                'status':'completed','content':[{'type':'output_text','text':'done','annotations':[]}]}],
            'usage':{'input_tokens':10,'output_tokens':2}}).encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    checks, records, viewports = [], {}, []
    with tempfile.TemporaryDirectory(prefix='emp-guidance-e2e-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, _, _, headers, _ = start_backend(stack,args.emp.resolve(),root,
            {'codex_base_url':'http://127.0.0.1:9/fixture-only'},
            {'HTTP_PROXY':'http://127.0.0.1:9','HTTPS_PROXY':'http://127.0.0.1:9',
             'ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'127.0.0.1,localhost','PATH':'/usr/bin:/bin'})
        headers['Origin'] = base
        upstream = ThreadingHTTPServer(('127.0.0.1',0),Upstream)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever,daemon=True).start()
        stack.callback(upstream.shutdown)
        upstream_url = f'http://127.0.0.1:{upstream.server_port}/v1'
        config = request(base+'/api/config',headers=headers)[1]
        config['providers'] = [{'id':'guide','name':'Guidance fixture','base_url':upstream_url,
            'protocol':'responses','auth_mode':'api_key','api_key':'fixture-only'}]
        config['models'] = [{'id':'guide/model','provider':'guide','upstream_id':'fixture-model','enabled':True}]
        assert request(base+'/api/config',config,headers)[0] == 200
        web = Path(__file__).resolve().parents[2]/'crates/emp-app/web'
        head = '<script>localStorage.setItem("emp_management_session_v1",'+json.dumps(headers['X-EMP-Session'])+');</script>'
        frontend, _ = candidate_frontend(stack,base,web,index_head=head)
        browser = Browser(stack,root)
        browser.navigate(frontend)
        for language, width in [('zh-CN',500),('en',1280)]:
            request(browser.base+'/window/rect',{'width':width,'height':1000})
            browser.execute('setLanguage(arguments[0]);statistics.open();document.querySelector(".diagnostics-details").open=true;',language)
            wait_for(lambda: browser.execute('return document.getElementById("diagnostics_records").textContent.includes(arguments[0]);',
                '服务地址' if language == 'zh-CN' else 'base URL'))
            assert browser.execute('return document.getElementById("diagnostics_summary").textContent===arguments[0];',
                '暂无请求记录。' if language == 'zh-CN' else 'No requests have been recorded.')
            browser.execute('closeModal();')
        checks.append('Empty diagnostics direct client errors to address/port and local proxy checks, without claiming a cause')

        preset = {'id':'preset-fixture','name':'Preset fixture','base_url':upstream_url,
            'protocol':'responses','auth_mode':'api_key'}
        browser.execute('officialProviders.fixture=arguments[0];chooseOfficialProvider("fixture");',preset)
        assert not browser.execute('return document.querySelector(".provider-connection-details").open;')
        browser.execute('document.getElementById("modal_provider_key").value="fixture-preset-key";document.querySelector(".provider-connection-details").open=true;')
        assert browser.execute('return document.getElementById("modal_provider_key").value==="fixture-preset-key" && document.querySelector(".provider-connection-details").textContent.includes(arguments[0]);',upstream_url)
        browser.execute('document.querySelector(".provider-connection-details").open=false;document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        saved = request(base+'/api/config',headers=headers)[1]
        provider = next(row for row in saved['providers'] if row['id'] == preset['id'])
        assert provider['api_key_set'] and all(provider[key] == preset[key] for key in preset)
        assert 'fixture-preset-key' not in json.dumps(saved)
        browser.execute('chooseOfficialProvider("fixture");')
        assert browser.execute('return document.getElementById("modal_provider_key").value==="" && document.getElementById("modal_provider_key").placeholder==="******";')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        saved = request(base+'/api/config',headers=headers)[1]
        saved['models'].append({'id':'preset-fixture/model','provider':preset['id'],'upstream_id':'fixture-model','enabled':True})
        assert request(base+'/api/config',saved,headers)[0] == 200
        assert request(base+'/v1/responses',{'model':'preset-fixture/model','input':'preset-save','stream':False},headers)[0] == 200
        checks.append('Folded preset details preserve the typed key; save and blank-key edit retain credentials and use the selected upstream')

        for case in ['wait-advice','retry-once','partial-output']:
            body = {'model':'guide/model','input':case,'stream':case == 'partial-output'}
            if body['stream']:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                with opener.open(urllib.request.Request(base+'/v1/responses',json.dumps(body).encode(),
                        {**headers,'Content-Type':'application/json'}),timeout=15) as response:
                    response.read()
            else:
                status, _ = request(base+'/v1/responses',body,headers)
                assert status == (429 if case == 'wait-advice' else 200)
            records[case] = request(base+'/api/calls',headers=headers)[1]['records'][0]
        assert records['wait-advice']['retry_after_seconds'] == 301
        assert records['wait-advice']['attempts'] == Upstream.calls['wait-advice'] == 1
        assert records['retry-once']['state'] == 'completed'
        assert records['retry-once']['attempts'] == Upstream.calls['retry-once'] == 2
        assert records['retry-once']['retries'][0]['delay_ms'] == 1000
        assert records['retry-once']['retries'][0]['retry_after_seconds'] == 1
        assert records['partial-output']['state'] == 'failed' and records['partial-output']['output_emitted']
        assert Upstream.calls['partial-output'] == 1
        checks.append('Real 429 responses retain 301-second advice without replay, retry once after 1 second, and never replay partial output')

        host, port = base.removeprefix('http://').split(':')
        cancelled = socket.create_connection((host,int(port)))
        body = json.dumps({'model':'guide/model','input':'cancel-backoff','stream':True}).encode()
        header = f'POST /v1/responses HTTP/1.1\r\nHost: {host}:{port}\r\nX-EMP-Session: {headers["X-EMP-Session"]}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'
        cancelled.sendall(header.encode()+body)
        wait_for(lambda: Upstream.calls['cancel-backoff'] == 1)
        time.sleep(.1)
        cancelled.shutdown(socket.SHUT_RDWR)
        cancelled.close()
        records['cancel-backoff'] = wait_for(lambda: next((row for row in request(base+'/api/calls',headers=headers)[1]['records']
            if row.get('retries') and row['retries'][0].get('retry_after_seconds') == 5),None))
        assert records['cancel-backoff']['attempts'] == Upstream.calls['cancel-backoff'] == 1
        assert records['cancel-backoff']['state'] in ['cancelled','interrupted']
        assert all(model == 'fixture-model' for model in Upstream.models)
        assert 'private-fixture-marker' not in json.dumps(records)
        checks.append('Disconnecting a streaming request during backoff leaves one dispatch and a scheduled retry; model selection stays fixed')

        for language, width in [('zh-CN',500),('en',1280)]:
            request(browser.base+'/window/rect',{'width':width,'height':1000})
            browser.execute('setLanguage(arguments[0]);statistics.open();',language)
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-stats-view=calls]");'))
            browser.execute('document.querySelector("[data-stats-view=calls]").click();')
            for case, row in records.items():
                selector = '[data-call-id="'+row['request_id']+':responses"]'
                wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);',selector))
                browser.execute('const node=document.querySelector(arguments[0]);node.open=true;node.scrollIntoView({block:"start"});',selector)
                text = browser.execute('return document.querySelector(arguments[0]+" .call-result")?.textContent || "";',selector)
                if case == 'retry-once':
                    assert ('重试一次后完成' if language == 'zh-CN' else 'Completed after one retry') in text
                elif case == 'wait-advice':
                    assert '301' in text and ('未再次发送' if language == 'zh-CN' else 'did not resend') in text
                elif case == 'partial-output':
                    assert ('先检查任务进度' if language == 'zh-CN' else 'Check the task') in text
                else:
                    assert not text
                assert browser.execute('return document.querySelector(arguments[0]).scrollWidth<=document.querySelector(arguments[0]).clientWidth+1 && document.documentElement.scrollWidth<=innerWidth;',selector)
            browser.execute('document.querySelector(arguments[0]).scrollIntoView({block:"start"});',
                '[data-call-id="'+records['wait-advice']['request_id']+':responses"]')
            _, png = request(browser.base+'/screenshot')
            args.output.with_name(args.output.stem+'-'+language+'.png').write_bytes(base64.b64decode(png['value']))
            viewports.append({'language':language,**browser.execute('return {width:innerWidth,height:innerHeight};')})
            checks.append(f'{language}: real call records explain recovery/wait/partial output, omit cancelled-retry claims and fit the viewport')
    args.output.write_text(json.dumps({'passed':True,'checks':checks,'viewports':viewports,
        'real_model_calls':0,'production_state_used':False},indent=2)+'\n')
    print(json.dumps({'passed':True,'checks':checks}))


if __name__ == '__main__':
    main()
