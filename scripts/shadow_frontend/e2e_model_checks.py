"""Real model editing, upstream limits, probe receipts and unknown usage in Firefox."""
import argparse
import base64
import contextlib
import json
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser


class Models(BaseHTTPRequestHandler):
    requests = []
    fail_metadata = False

    def log_message(self, *args):
        pass

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.fail_metadata:
            return self.reply(503, {'error': {'message': 'fixture unavailable'}})
        self.reply(200, {'data': [{'id': 'model', 'context_window': 4096,
            'max_output_tokens': 768}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        type(self).requests.append(body)
        usage = {'prompt_tokens': 10, 'completion_tokens': 2,
                 'prompt_tokens_details': {'cached_tokens': 0}}
        if body['model'] == 'zero':
            usage = {'prompt_tokens': 0, 'completion_tokens': 0,
                     'prompt_tokens_details': {'cached_tokens': 0}}
        if body['model'] == 'partial':
            usage = {'prompt_tokens': 7}
        response = {'id': f'fixture-{len(self.requests)}', 'model': body['model'],
            'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'ok'},
                         'finish_reason': 'stop'}]}
        if body['model'] != 'missing':
            response['usage'] = usage
        self.reply(200, response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-model-checks-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        (root / 'state').mkdir()
        (root / 'state/api_prices.json').write_text(json.dumps({'fetched_at': time.time(), 'prices': {
            'model': {'input_cost_per_token': '0.001', 'output_cost_per_token': '0.002',
                      'cache_read_input_token_cost': '0.00025', 'cache_creation_input_token_cost': '0.002'}}}))
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Models)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        base, url, _, headers, _ = start_backend(stack, args.emp.resolve(), root,
            {'auto_enable_on_start': False, 'codex_base_url': 'http://127.0.0.1:9/fixture-only',
             'providers': [{'id': 'demo', 'name': 'Model checks',
                'base_url': f'http://127.0.0.1:{upstream.server_port}/v1',
                'protocol': 'chat_completions', 'auth_mode': 'api_key', 'api_key': 'fixture-only'}],
             'models': [{'id': 'demo/' + name, 'provider': 'demo',
                'upstream_id': name,
                'context_window': 4096, 'enabled': True} for name in ['model', 'zero', 'missing', 'partial']]},
            {'PATH': '/usr/bin:/bin', 'HTTP_PROXY': 'http://127.0.0.1:9',
             'HTTPS_PROXY': 'http://127.0.0.1:9', 'ALL_PROXY': 'http://127.0.0.1:9',
             'NO_PROXY': '127.0.0.1,localhost'})
        headers['Origin'] = base
        status, discovered = request(base + '/api/providers/discover', {'provider': 'demo'}, headers)
        assert status == 200, discovered
        advertised = discovered['models'][0]
        assert advertised['output_limit'] == 768, advertised
        assert request(base + '/api/providers/discover', {'provider': 'demo', 'cached': True, 'selected': ['model']}, headers)[0] == 200
        frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        wait_for(lambda: browser.execute('return !!state;'))
        headers = {'X-EMP-Session': browser.execute('return localStorage.getItem("emp_management_session_v1");'), 'Origin': base}
        browser.execute('setLanguage("zh-CN");openManualModelModal("demo/model");')
        wait_for(lambda: browser.execute('return document.getElementById("modal_model_output").value==="768";'))
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return state.models.find(m=>m.id==="demo/model").output_limit===768 && document.getElementById("modal_backdrop").classList.contains("hidden");'))
        config = request(base + '/api/config', headers=headers)[1]
        saved = next(model for model in config['models'] if model['id'] == 'demo/model')
        assert saved['output_limit'] == 768 and saved['capability_sources']['output_limit']['source'] == 'advertised', saved
        browser.execute('openManualModelModal("demo/model");document.getElementById("modal_model_output").value="-1";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("modal_status").textContent;'))
        assert next(model for model in request(base + '/api/config', headers=headers)[1]['models'] if model['id'] == 'demo/model')['output_limit'] == 768
        browser.execute('openManualModelModal("demo/model");const output=document.getElementById("modal_model_output");output.value="64";output.dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return state.models.find(m=>m.id==="demo/model").output_limit===64 && document.getElementById("modal_backdrop").classList.contains("hidden");'))
        saved = next(model for model in request(base + '/api/config', headers=headers)[1]['models'] if model['id'] == 'demo/model')
        assert saved['capability_sources']['output_limit']['source'] == 'manual'
        browser.execute('testModel("demo/model");')
        wait_for(lambda: len(Models.requests) == 1)
        assert Models.requests[0].get('max_completion_tokens', Models.requests[0].get('max_tokens')) == 64, Models.requests[0]
        probes = wait_for(lambda: request(base + '/api/diagnostics', headers=headers)[1].get('probes'))
        wait_for(lambda: any(probe['operation'] == 'model_test' for probe in request(base + '/api/diagnostics', headers=headers)[1]['probes']))
        probe = next(probe for probe in request(base + '/api/diagnostics', headers=headers)[1]['probes'] if probe['operation'] == 'model_test')
        assert request(base + '/api/calls?request=' + probe['request_id'], headers=headers)[1]['summary']['calls'] == 1
        assert request(base + '/api/calls', headers=headers)[1]['summary']['calls'] == 0
        usage = request(base + '/api/usage?series=true', headers=headers)[1]
        assert usage['totals']['input_tokens'] == 10 and usage['totals']['output_tokens'] == 2, usage
        assert usage['totals']['cost_nanos'] == 14_000_000 and usage['totals']['priced_requests'] == 1, usage
        assert request(base + '/api/usage?sessions=true', headers=headers)[1]['total'] == 0
        for operation, action in [('vision_test', 'testModelVision'), ('audio_test', 'testModelAudio')]:
            browser.execute(f'{action}("demo/model");')
            wait_for(lambda: any(probe['operation'] == operation for probe in request(base + '/api/diagnostics', headers=headers)[1]['probes']))
        generated = len(Models.requests)
        usage = request(base + '/api/usage?series=true', headers=headers)[1]['totals']
        assert usage['input_tokens'] == 10 * generated and usage['cost_nanos'] == 14_000_000 * generated, usage
        # A normal proxy credential cannot hide a conversation by claiming to be a UI probe.
        conversation = '11111111-1111-4111-8111-111111111111'
        (root / 'codex_home/auth.json').write_text(json.dumps({'OPENAI_API_KEY': 'fixture-caller'}))
        (root / 'codex_home/session_index.jsonl').write_text(json.dumps(
            {'id': conversation, 'thread_name': 'Synthetic conversation'}) + '\n')
        status, result = request(base + '/v1/responses',
            {'model': 'demo/model', 'input': 'synthetic caller check'},
            {'Authorization': 'Bearer fixture-caller', 'X-EMP-Probe': 'model_test',
             'thread-id': conversation})
        assert status == 200, result
        wait_for(lambda: request(base + '/api/calls', headers=headers)[1]['summary']['calls'] == 1)
        wait_for(lambda: request(base + '/api/usage?sessions=true', headers=headers)[1]['total'] == 1)
        assert request(base + '/api/usage?sessions=true', headers=headers)[1]['records'][0]['session_id'] == conversation
        checks.append('Discovered output limit persists and reaches upstream; authenticated model tests retain usage without sessions/performance samples, direct request lookup works, and forged probe headers cannot hide a normal conversation')
        for limit, expected in [(16, 16), (128, 64), (None, 64)]:
            payload = {'model': 'demo/model', 'input': 'synthetic limit check'}
            if limit is not None:
                payload['max_output_tokens'] = limit
            status, result = request(base + '/v1/responses', payload, headers)
            assert status == 200, result
            assert Models.requests[-1].get('max_completion_tokens', Models.requests[-1].get('max_tokens')) == expected
        browser.execute('openManualModelModal("demo/model");const output=document.getElementById("modal_model_output");output.value="";output.dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return state.models.find(m=>m.id==="demo/model").output_limit===0 && document.getElementById("modal_backdrop").classList.contains("hidden");'))
        assert request(base + '/v1/responses', {'model': 'demo/model', 'input': 'synthetic default check', 'max_output_tokens': 128}, headers)[0] == 200
        assert Models.requests[-1].get('max_completion_tokens', Models.requests[-1].get('max_tokens')) == 128
        checks.append('A smaller caller limit wins; blank removes the configured cap without replacing the caller limit')
        Models.fail_metadata = True
        assert request(base + '/api/models/metadata', {'provider': 'demo', 'model': 'model'}, headers)[0] == 400
        assert request(base + '/api/providers/discover', {'provider': 'demo'}, headers)[0] == 503
        diagnostics = request(base + '/api/diagnostics', headers=headers)[1]
        assert any(item['operation'] == 'model_metadata' and item['outcome'] == 'failed' for item in diagnostics['probes'])
        assert 'fixture-only' not in json.dumps(diagnostics)
        for model in ['missing', 'partial', 'zero']:
            assert request(base + '/v1/responses', {'model': 'demo/' + model, 'input': 'synthetic usage check'}, headers)[0] == 200
        browser.execute('closeModal();statistics.open();document.querySelector("[data-stats-view=models]").click();')
        def model_cells(model):
            return browser.execute('const row=[...document.querySelectorAll(".usage-table tbody tr")].find(row=>row.querySelector("strong")?.textContent===arguments[0]);return row?[...row.querySelectorAll("td")].map(cell=>cell.textContent):null;', model)
        wait_for(lambda: model_cells('missing'))
        assert model_cells('missing')[2:5] == ['—', '—', '—'], model_cells('missing')
        assert model_cells('partial')[2:5] == ['7', '—', '—'], model_cells('partial')
        assert model_cells('zero')[2:5] == ['0', '0', '0'], model_cells('zero')
        browser.execute('document.querySelector("[data-stats-view=services]").click();')
        assert browser.execute('return [...document.querySelectorAll(".usage-table td")].some(cell=>cell.textContent.endsWith(" *"));')
        checks.append('Actual upstream missing, partial and zero usage remain distinct in model/service tables, including grouped coverage')
        for language in ['zh-CN', 'en']:
            browser.execute('setLanguage(arguments[0]);openManualModelModal("demo/model");', language)
            for width in [640, 1280]:
                request(browser.base + '/window/rect', {'width': width, 'height': 900})
                assert browser.execute('return document.getElementById("modal_body").scrollWidth<=document.getElementById("modal_body").clientWidth+1;')
            status, png = request(browser.base + '/screenshot')
            assert status == 200
            args.output.with_name('model-editor-' + language + '.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('statistics.open();loadDiagnostics();')
            wait_for(lambda: browser.execute('return !!document.getElementById("diagnostics_probes");'))
            browser.execute('document.querySelector("#presentation_stats > details").open=true;document.getElementById("diagnostics_probes").open=true;window.probesRefreshed=false;loadDiagnostics().then(()=>window.probesRefreshed=true);')
            wait_for(lambda: browser.execute('return window.probesRefreshed;'))
            assert browser.execute('return document.getElementById("diagnostics_probes").open;')
            assert browser.execute('return !document.getElementById("diagnostics_probes").textContent.includes("reports.");')
            for width in [640, 1280]:
                request(browser.base + '/window/rect', {'width': width, 'height': 900})
                assert browser.execute('return document.getElementById("modal_body").scrollWidth<=document.getElementById("modal_body").clientWidth+1;')
            browser.execute('const probes=document.getElementById("diagnostics_probes");probes.querySelector(".request-failure").open=true;probes.scrollIntoView({block:"start"});')
            status, png = request(browser.base + '/screenshot')
            assert status == 200
            args.output.with_name('model-checks-' + language + '.png').write_bytes(base64.b64decode(png['value']))
        checks.append('Success and failure probes are visible with localized labels and no prompt/key contents at narrow and desktop widths')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
