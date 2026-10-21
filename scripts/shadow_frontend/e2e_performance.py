"""Verify real stream timing, durable reports and bilingual Shadow presentation."""
import argparse
import base64
import contextlib
import json
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from compression import zstd

from e2e_environment import request, wait_for
from e2e_refinements import Browser
from preview import fixture, preview


class PacedResponses(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers['Content-Length']))
        if self.headers.get('Content-Encoding') == 'zstd':
            raw = zstd.decompress(raw)
        body = json.loads(raw)
        response = {'id': 'timing-' + str(time.time_ns()), 'model': 'timing-model',
            'status': 'completed', 'output': [],
            'usage': {'input_tokens': 10, 'output_tokens': 120,
                'output_tokens_details': {'reasoning_tokens': 20}}}
        self.send_response(200)
        self.send_header('Connection', 'close')
        self.send_header('Content-Type', 'text/event-stream' if body['stream'] else 'application/json')
        self.end_headers()
        if not body['stream']:
            self.wfile.write(json.dumps(response).encode())
            return

        def event(value):
            self.wfile.write(('data: ' + json.dumps(value) + '\n\n').encode())
            self.wfile.flush()

        event({'type': 'response.output_item.added', 'item': {'type': 'reasoning'}})
        time.sleep(.2)
        event({'type': 'response.output_text.delta', 'delta': 'first'})
        if body['input'] != 'single-batch':
            if body['input'] != 'coalesced':
                time.sleep(.2)
            event({'type': 'response.output_text.delta', 'delta': 'last'})
        time.sleep(.2)
        event({'type': 'response.completed', 'response': response})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks, viewports = [], []
    with tempfile.TemporaryDirectory(prefix='emp-performance-e2e-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        endpoint, _, bundle = fixture(stack, args.emp.resolve(), root)
        backend, state, headers, _ = endpoint
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), PacedResponses)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        config = request(backend + '/api/config', headers=headers)[1]
        config['providers'] = [{'id': 'timing', 'name': 'Timing fixture',
            'base_url': f'http://127.0.0.1:{upstream.server_port}/v1',
            'protocol': 'responses', 'auth_mode': 'forward'}]
        config['models'] = [{'id': 'timing/model', 'provider': 'timing',
            'upstream_id': 'timing-model', 'enabled': True}]
        assert request(backend + '/api/config', config, headers)[0] == 200
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for prompt, streamed in [('paced', True), ('single-batch', True), ('coalesced', True), ('buffered', False)]:
            body = json.dumps({'model': 'timing/model', 'input': prompt, 'stream': streamed}).encode()
            call = urllib.request.Request(backend + '/v1/responses', body,
                {**headers, 'Content-Type': 'application/json', 'Authorization': 'Bearer fixture'})
            with opener.open(call, timeout=15) as result:
                assert result.status == 200
                result.read()
        report = request(backend + '/api/calls', headers=headers)[1]
        rows = report['records']
        measured = next(row for row in rows if row['model_id'] == 'timing/model'
            and row.get('generation_tokens_per_second') is not None)
        assert measured['performance_schema'] == 5
        assert measured['first_content_ms'] >= measured['ttft_ms'] + 100
        assert measured['generation_ms'] >= 150
        expected = round(100000 / measured['generation_ms'], 2)
        assert abs(measured['generation_tokens_per_second'] - expected) <= .01
        assert measured['generation_tokens_per_second'] > measured['tokens_per_second'] * 2
        unknown = [row for row in rows if row['model_id'] == 'timing/model' and row != measured]
        assert len(unknown) == 3 and all(row.get('generation_tokens_per_second') is None for row in unknown)
        checks.append('Paced stream excludes reasoning and terminal waits; single-batch, burst and buffered rates remain unknown')
        samples = [row['generation_tokens_per_second'] for row in rows if row.get('generation_tokens_per_second') is not None]
        assert report['summary']['tps_samples'] == len(samples) == 2
        assert abs(report['summary']['tokens_per_second'] - sum(samples) / len(samples)) < .01
        group = next(row for row in report['models'] if row['model_id'] == 'timing/model')
        assert group['tokens_per_second'] == measured['generation_tokens_per_second']
        assert next(row for row in rows if row['request_id'] == 'metrics-1')['tokens_per_second'] == 20
        burst = next(row for row in rows if row['request_id'] == 'metrics-burst')
        assert burst['generation_ms'] == 6 and burst.get('generation_tokens_per_second') is None
        assert burst['tokens_per_second'] == 8.36
        tools = next(row for row in rows if row['request_id'] == 'metrics-0')
        assert tools['turn_tool_ms'] == 5000
        assert next(row for row in rows if row['request_id'] == 'metrics-1')['turn_tool_ms'] is None
        checks.append('Summary and model groups use generation samples while legacy call facts retain end-to-end rates')
        checks.append('Historical 11500 TPS burst is excluded without rewriting facts; real local tool intervals union to 5 seconds and missing telemetry stays unknown')

        frontend, _ = preview(stack, backend, bundle, headers['X-EMP-Session'])
        browser = Browser(stack, root)
        for language, width, full_label in [('zh-CN', 390, '全程 TPS'), ('en', 1280, 'End-to-end TPS')]:
            browser.navigate(frontend)
            request(browser.base + '/window/rect', {'width': width, 'height': 1000})
            viewports.append({'language': language, 'requested_width': width,
                **browser.execute('return {width:innerWidth,height:innerHeight,pixel_ratio:devicePixelRatio};')})
            browser.execute('setLanguage(arguments[0]);window.EMP_REVIEW_ACTION("metrics");', language)
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-stats-view=calls]");'))
            browser.execute('document.querySelector("[data-stats-view=calls]").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-call-id=\\"metrics-0:responses\\"]");'))
            browser.execute('document.querySelector("[data-call-id=\\"metrics-0:responses\\"]").open=true;')
            text = browser.execute('return document.querySelector("[data-call-id=\\"metrics-0:responses\\"]").textContent;')
            assert 'TTFT 2.00 s · TPS 80.0' in text
            assert full_label in text and '20.0' in text
            tool_label, unavailable = ('本轮工具耗时','未提供') if language == 'zh-CN' else ('Turn tool time','Unavailable')
            assert tool_label in text
            assert browser.execute('''
              const record=document.querySelector('[data-call-id="metrics-0:responses"]');
              const fact=[...record.querySelectorAll('.call-fact')].find(node=>node.querySelector('span')?.textContent===arguments[0]);
              return fact.querySelector('strong').textContent==='5.00 s' && fact.querySelector('span').title.length>50;
            ''', tool_label)
            legacy = browser.execute('return document.querySelector("[data-call-id=\\"metrics-1:responses\\"]").textContent;')
            assert 'TTFT — · TPS —' in legacy and full_label in legacy
            assert unavailable in legacy
            assert 'TPS —' in browser.execute('return document.querySelector("[data-call-id=\\"metrics-burst:responses\\"]").textContent;')
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            assert browser.execute('''
              const record=document.querySelector('[data-call-id="metrics-0:responses"]');
              const field=[...record.querySelectorAll('.call-fact')]
                .find(node=>node.querySelector('span')?.textContent===arguments[0]);
              field.scrollIntoView({block:'center'});
              const bounds=field.getBoundingClientRect();
              return record.scrollWidth<=record.clientWidth+1 && bounds.top>=0 && bounds.bottom<=innerHeight;
            ''', full_label)
            _, png = request(browser.base + '/screenshot')
            args.output.with_name(args.output.stem + '-' + language + '.png').write_bytes(base64.b64decode(png['value']))
            checks.append(f'{language}: real statistics UI shows separate rates, measured 5-second tool time, missing telemetry, hidden burst TPS and no page overflow')
            browser.execute('document.querySelector("[data-stats-view=overview]").click();const select=document.querySelector("[data-stats-group]");select.value="services";select.dispatchEvent(new Event("change",{bubbles:true}));')
            color_check = '''
              const legend=[...document.querySelectorAll('.presentation-chart-legend>span')];
              const colors=legend.map(node=>getComputedStyle(node.querySelector('i')).backgroundColor);
              const fills=[...document.querySelectorAll('[data-stack-value]')].map(node=>getComputedStyle(node).fill);
              // The timing provider has no USD price; only its Token bar is nonzero.
              const visible=document.querySelector('[data-stats-metric=cost_nanos][aria-pressed=true]') ? 3 : 4;
              return legend.length===4 && new Set(colors).size===4 && new Set(fills).size===visible &&
                fills.every(color=>colors.includes(color));
            '''
            wait_for(lambda: browser.execute('return document.querySelectorAll(".presentation-chart-legend>span").length>=3;'))
            assert browser.execute(color_check), browser.execute('return {legend:[...document.querySelectorAll(".presentation-chart-legend>span")].map(node=>[node.textContent,getComputedStyle(node.querySelector("i")).backgroundColor]),fills:[...document.querySelectorAll("[data-stack-value]")].map(node=>getComputedStyle(node).fill)};')
            before = browser.execute('return [...document.querySelectorAll(".presentation-chart-legend>span")].map(node=>[node.textContent,getComputedStyle(node.querySelector("i")).backgroundColor]);')
            for metric in ['cost_nanos','tokens']:
                browser.execute('document.querySelector("[data-stats-metric="+arguments[0]+"]").click();', metric)
                assert browser.execute(color_check)
                assert browser.execute('return [...document.querySelectorAll(".presentation-chart-legend>span")].map(node=>[node.textContent,getComputedStyle(node.querySelector("i")).backgroundColor]);') == before
            browser.execute('document.querySelector(".presentation-chart-canvas").dispatchEvent(new KeyboardEvent("keydown",{key:"End",bubbles:true}));')
            assert browser.execute('return [...document.querySelectorAll(".presentation-chart-tooltip small")].filter(node=>node.style.color).length===4;')
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            _, png = request(browser.base + '/screenshot')
            args.output.with_name(args.output.stem + '-services-' + language + '.png').write_bytes(base64.b64decode(png['value']))
            checks.append(f'{language}: three formerly colliding services have distinct rendered bars and matching legends across Token/USD, with keyboard tooltip and no page overflow')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks,
        'real_model_calls': 0, 'viewports': viewports, 'measured': {key: measured[key] for key in
            ['duration_ms', 'ttft_ms', 'first_content_ms', 'generation_ms', 'tokens_per_second', 'generation_tokens_per_second']}}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
