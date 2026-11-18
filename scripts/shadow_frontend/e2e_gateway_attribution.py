"""Client labels and external-session bills through an isolated gateway and browser."""
import argparse
import base64
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading

from e2e import Browser
from local_backend import request, start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = []

    class Upstream(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            payload = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            response = {'id': 'fixture', 'object': 'chat.completion', 'model': payload['model'],
                'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'done'}, 'finish_reason': 'stop'}]}
            if 'unreported fixture' not in json.dumps(payload['messages']):
                response['usage'] = {'prompt_tokens': 7, 'completion_tokens': 3, 'total_tokens': 10}
            body = json.dumps(response).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with contextlib.ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-client-browser-')))
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        config = {'auto_enable_on_start': False,
            'providers': [{'id': 'fixture', 'name': 'Fixture', 'auth_mode': 'api_key', 'api_key': 'fixture-only',
                'protocol': 'chat_completions', 'base_url': f'http://127.0.0.1:{upstream.server_port}/v1'}],
            'models': [{'id': 'fixture/' + alias, 'provider': 'fixture', 'upstream_id': model, 'enabled': True}
                for alias, model in [('model', 'upstream-one'), ('second', 'upstream-two')]]}
        backend, url, _, management = start_backend(stack, args.binary.resolve(), root, config)
        _, connection = request(backend + '/api/gateway', headers=management)
        _, device = request(backend + '/api/gateway/clients', {'name': 'Pi workstation'}, management)
        assert request(backend + '/api/gateway/clients', {'name': 'Spare device'}, management)[0] == 200
        for client, key, extra in [
            ({'User-Agent': 'Pi/1.4.0', 'x-session-id': 'pi-task'}, connection['api_key'], {}),
            ({'User-Agent': 'Pi/1.4.0', 'session_id': 'pi-task'}, connection['api_key'], {}),
            ({'X-EMP-Client': 'Python nightly'}, device['api_key'], {}),
            ({'X-EMP-Client': 'OMP'}, device['api_key'], {'input': 'unreported fixture'}),
            ({'X-EMP-Client': 'OMP'}, device['api_key'], {'model': 'fixture/second'}),
            ({'User-Agent': 'OpenAI/Python 1.0'}, connection['api_key'], {'metadata': {'session_id': 'python-task'}}),
        ]:
            status, response = request(backend + '/v1/responses',
                {'model': 'fixture/model', 'input': 'public fixture', 'stream': False, **extra},
                {'Authorization': 'Bearer ' + key, **client})
            assert status == 200 and response['status'] == 'completed', response
        browser = Browser(stack, root)
        browser.navigate(url)

        def click(selector):
            wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);', selector))
            browser.execute('document.querySelector(arguments[0]).click();', selector)

        def screenshot(name):
            status, data = request(browser.base + '/screenshot')
            assert status == 200, data
            (args.output / (name + '.png')).write_bytes(base64.b64decode(data['value']))

        for language in ['zh-CN', 'en']:
            browser.execute('closeModal();setLanguage(arguments[0]);', language)
            click('button[onclick="statistics.open()"]')
            click('[data-stats-view="calls"]')
            wait_for(lambda: browser.execute('return document.querySelectorAll(".call-record").length;') == 6)
            labels = browser.execute('return [...document.querySelectorAll(".call-session")].map(node=>node.childNodes[0].textContent);')
            assert 'Pi workstation' in labels and 'pi-task' in labels and 'python-task' in labels, labels
            browser.execute('''
                const row=[...document.querySelectorAll('.call-record')].find(node=>
                    node.querySelector('.call-session').childNodes[0].textContent==='Pi workstation' &&
                    node.textContent.includes('Python nightly'));
                row.querySelector('summary').click();
            ''')
            wait_for(lambda: browser.execute('return document.querySelector(".call-record[open]")?.textContent.includes("Python nightly");'))
            text = browser.execute('return document.querySelector(".call-record[open]").textContent;')
            assert ('未提供会话 ID' if language == 'zh-CN' else 'No session ID provided') in text
            browser.execute('document.querySelector(".call-record[open]").scrollIntoView({block:"start"});')
            screenshot('client-calls-' + language)
            click('[data-stats-view="sessions"]')
            wait_for(lambda: browser.execute('return document.querySelectorAll("[data-session-bill]").length;') == 2)
            assert browser.execute('return !!document.querySelector(`[data-session-bill="pi-task"]`);')
            click('[data-session-bill="pi-task"] > summary')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-bill-models]");'))
            screenshot('client-bills-' + language)
            browser.execute('closeModal();')
            click('button[onclick="openSettings()"]')
            click('[data-settings-category="general"]')
            click('[data-gateway-open]')
            click('[data-gateway-usage]')
            toggle = '.gateway-device-toggle'
            wait_for(lambda: browser.execute('return document.querySelector(".gateway-device-toggle")?.getAttribute("aria-expanded");') == 'false')
            assert browser.execute('''
                return [...document.querySelectorAll('.gateway-usage-table tbody:not([hidden]) tr')]
                    .map(row => [row.cells[0].textContent.trim(), row.cells[1].textContent.trim()]);
            ''') == [['Pi workstation', '3'], ['Spare device', '0']]
            click(toggle)
            assert browser.execute(r'''
                return [...document.querySelectorAll('.gateway-device-models:not([hidden]) tr')]
                    .map(row => [row.cells[0].textContent.trim(), row.cells[1].textContent.trim(),
                        row.cells[2].innerText.trim().replace(/\s+/g,' ')]);
            ''') == [['upstream-one', '2', '7 / 3'], ['upstream-two', '1', '7 / 3']]
            click('[data-gateway-days="7"]')
            wait_for(lambda: browser.execute('return document.querySelector("[data-gateway-days][aria-pressed=true]")?.dataset.gatewayDays;') == '7')
            assert browser.execute('return document.querySelector(".gateway-device-toggle").getAttribute("aria-expanded");') == 'true'
            screenshot('client-usage-' + language)
            click(toggle)
            assert browser.execute('return !!document.querySelector(".gateway-device-models[hidden]");')
            checks.append(language + ': Calls show client/device names, two external bills and expandable per-device model usage')
        report = {'passed': True, 'real_provider_calls': 0, 'checks': checks}
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report))


if __name__ == '__main__':
    main()
