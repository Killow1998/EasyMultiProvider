#!/usr/bin/env python3
"""Real-browser acceptance against an isolated EMP and synthetic upstream."""
import argparse
import contextlib
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from unittest.mock import patch
from e2e_environment import free_port, request, start_backend, stop, wait_for
from frontend import frontend

HERE = Path(__file__).resolve().parent


class Browser:
    def __init__(self, stack, root):
        self.base = f'http://127.0.0.1:{free_port()}'
        log = stack.enter_context((root / 'geckodriver.log').open('w'))
        process = subprocess.Popen(['geckodriver', '--port', self.base.rsplit(':', 1)[1]],
                                   stdout=log, stderr=log)
        stack.callback(stop, process)
        self.log_path = root / 'geckodriver.log'
        def ready():
            try:
                return request(self.base + '/status')[0] == 200
            except OSError:
                return False
        wait_for(ready)
        _, result = request(self.base + '/session', {'capabilities': {'alwaysMatch': {
            'browserName': 'firefox', 'moz:firefoxOptions': {'args': ['-headless'],
            'prefs': {'layout.css.devPixelsPerPx': '1.25', 'ui.prefersReducedMotion': 1}}}}})
        self.base += '/session/' + result['value']['sessionId']
        stack.callback(lambda: request(self.base, method='DELETE'))

    def execute(self, script, *args):
        status, data = request(self.base + '/execute/sync', {'script': script, 'args': list(args)})
        if status != 200:
            raise AssertionError({'status': status, 'response': data,
                                  'driver': self.log_path.read_text(encoding='utf-8')[-2000:]})
        return data['value']

    def navigate(self, url):
        status, data = request(self.base + '/url', {'url': url})
        assert status == 200, data
        try:
            wait_for(lambda: self.execute('return !!document.querySelector("#services .service-card");'))
        except TimeoutError:
            print(self.execute('return {state:typeof state,ready:document.readyState,text:document.body.innerText.slice(-2500)};'))
            raise

    def api(self, path):
        status, data = request(self.base + '/execute/async', {'script':
            'const done=arguments[arguments.length-1];fetch(arguments[0],{headers:{"X-EMP-Session":localStorage.getItem("emp_management_session_v1")}}).then(r=>r.json()).then(done,error=>done({error:error.message}));',
            'args': [path]})
        assert status == 200, data
        result = data['value']
        assert 'error' not in result, result
        return result

    def hover(self, selector):
        element=self.execute('return document.querySelector(arguments[0]);',selector)
        assert element,selector
        status,data=request(self.base+'/actions',{'actions':[{'type':'pointer','id':'mouse','parameters':{'pointerType':'mouse'},'actions':[{'type':'pointerMove','duration':50,'origin':element,'x':0,'y':0}]}]})
        assert status==200,data

    def escape(self):
        status, value = request(self.base + '/actions', {'actions': [{'type': 'key', 'id': 'keyboard',
            'actions': [{'type': 'keyDown', 'value': '\ue00c'}, {'type': 'keyUp', 'value': '\ue00c'}]}]})
        assert status == 200, value


def check_page(browser, shadow):
    cases = []
    assert browser.execute('return document.body.dataset.quotaStyle;') == ('ring' if shadow else 'bar')
    assert browser.execute('return document.querySelectorAll("[data-i18n=statistics]").length===1 && !document.querySelector(".page-header-controls [data-i18n=usage]");')
    assert browser.execute('return document.querySelectorAll("#services .request-activity").length===0 && document.querySelectorAll("#services .presentation-service-icon").length>0;')
    assert browser.execute('return document.querySelector("#integration_toggle").disabled;') is shadow
    for language in ['en', 'zh-CN']:
        for theme in ['light', 'dark']:
            for width in [640, 1280]:
                request(browser.base + '/window/rect', {'width': width, 'height': 900})
                browser.execute('setLanguage(arguments[0]);setTheme(arguments[1]);', language, theme)
                browser.execute('closeModal();')
                for control in ['.page-header-controls [data-i18n=settings_menu]','#services .presentation-service-icon','#services .account-summary']:
                    background=browser.execute('return getComputedStyle(document.querySelector(arguments[0])).backgroundColor;',control)
                    browser.hover(control)
                    wait_for(lambda:browser.execute('return !document.querySelector(".presentation-hover-feedback").hidden;'))
                    assert browser.execute('return getComputedStyle(document.querySelector(arguments[0])).backgroundColor;',control)==background
                browser.execute('document.querySelector("[data-i18n=statistics]").click();')
                wait_for(lambda: browser.execute('return !!document.querySelector("[data-call-outcomes]") && !document.querySelector("[data-call-outcomes]").textContent.includes("—");'))
                assert browser.execute('return document.querySelector("[data-call-outcomes]").textContent.includes("50.0%");')
                assert browser.execute('const grid=document.querySelector(".presentation-stat-grid");return [...grid.children].every(node=>{const title=node.querySelector("span").getBoundingClientRect(),value=node.querySelector("strong").getBoundingClientRect();return title.bottom<=value.top+1;});')
                for metric in ['cost_nanos','calls','tokens_per_second','ttft_ms','tokens']:
                    browser.execute('document.querySelector("[data-stats-metric="+arguments[0]+"]").click();',metric)
                for group in ['services','models','none']:
                    browser.execute('const select=document.querySelector("[data-stats-group]");select.value=arguments[0];select.dispatchEvent(new Event("change",{bubbles:true}));',group)
                for view in ['services','models','calls','overview']:
                    browser.execute('document.querySelector("[data-stats-view="+arguments[0]+"]").click();',view)

                browser.execute('document.querySelector("[data-call-outcomes]").click();')
                assert browser.execute('return !!document.querySelector("dialog[open] tbody") && document.querySelector("dialog[open]").textContent.includes("401");')
                assert browser.execute('const r=document.querySelector("dialog[open]").getBoundingClientRect();return r.left>=0 && r.right<=innerWidth+1 && r.height<=innerHeight;')
                browser.escape()
                wait_for(lambda: browser.execute('return !document.querySelector("dialog[open]") && !!document.querySelector("[data-call-outcomes]");'))
                cases.append({'language': language, 'theme': theme, 'width': width})
    browser.execute('closeModal();')
    browser.execute('document.querySelector("[data-i18n=settings_menu]").click();document.querySelector("[data-presentation-style=bar]").click();')
    assert browser.execute('return document.body.dataset.quotaStyle === "bar";')
    browser.execute('document.querySelector("[data-presentation-style=ring]").click();')
    assert browser.execute('return document.body.dataset.quotaStyle === "ring";')
    assert browser.execute('const heights=[...document.querySelectorAll("#services .service-card")].map(node=>node.getBoundingClientRect().height);return Math.max(...heights)-Math.min(...heights)<=1;')
    browser.execute('document.querySelector("[data-presentation-dot-color=purple]").click();closeModal();')
    browser.execute('document.querySelector("#services [data-ui-action=provider-model-settings]").click();')
    assert browser.execute('return !!document.querySelector("#modal_body button[data-icon=refresh]");')
    browser.execute('closeModal();')
    if shadow:
        for mode in ['services', 'service-errors']:
            wait_for(lambda: browser.execute('return !!document.getElementById("shadow_demo_button");'))
            browser.execute('document.getElementById("shadow_demo_button").click();document.querySelector("[data-shadow-demo="+arguments[0]+"]").click();', mode)
            wait_for(lambda: browser.execute('return document.querySelectorAll(".presentation-service-icon").length >= 12;'))
            if mode == 'service-errors':
                assert browser.execute('return document.querySelectorAll(".presentation-service-icon[data-error-message]").length >= 12;')
            browser.execute('document.querySelector("#shadow_services_demo button").click();')
            wait_for(lambda: browser.execute('return !!document.getElementById("shadow_demo_button") && !document.getElementById("shadow_services_demo") && document.querySelectorAll(".presentation-service-icon").length > 0 && document.querySelectorAll(".presentation-service-icon").length < 12;'))
        # Edits remain in preview storage, including credential scrubbing.
        browser.execute('''const script=document.createElement("script");
            script.textContent='shadowSave({...state,providers:state.providers.map(p=>({...p,name:"Draft",api_key:"must-not-persist"}))});';
            document.head.append(script);script.remove();''')
        assert browser.execute('return !localStorage.getItem("emp.shadow.config.v1").includes("must-not-persist");')
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    result = {}
    # Simulate a Windows legacy codepage. Assets must retain Chinese text even
    # when the user's default text-file encoding is not UTF-8.
    read_text = Path.read_text
    with patch.object(Path, 'read_text', lambda path, encoding=None, errors=None:
                      read_text(path, encoding=encoding or 'cp1252', errors=errors)):
        assert '服务'.encode('utf-8') in frontend('index.html', 'fixture-only')
    with tempfile.TemporaryDirectory(prefix='emp-shadow-e2e-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, url, state, headers, upstream = start_backend(stack, args.emp.resolve(), root)
        payload = {'model': 'demo/model', 'input': 'ok', 'stream': False}
        assert request(base + '/v1/responses', payload, headers)[0] == 401
        browser = Browser(stack, root)
        browser.navigate(url)
        wait_for(lambda: browser.execute('return !!document.querySelector("#services .presentation-service-icon[data-request-error]");'))
        browser.execute('document.querySelector("#services .presentation-service-icon[data-request-error]").focus();')
        wait_for(lambda: browser.execute('return !document.querySelector(".presentation-service-error").hidden;'))
        assert browser.execute('return document.querySelector(".presentation-service-error").textContent.includes("401");')
        headers['X-EMP-Session']=json.loads((state/'web-session.json').read_text(encoding='utf-8'))['token']
        upstream.failed = False
        assert request(base + '/v1/responses', payload, headers)[0] == 200
        wait_for(lambda: browser.execute('return !document.querySelector("#services .presentation-service-icon[data-request-error]");'))
        _, report = request(base + '/api/calls', headers=headers)
        summary = report['summary']
        assert (summary['completed'], summary['failed'], summary['success_samples']) == (1, 1, 2), summary
        assert summary['failure_breakdown'][0]['origin'] == 'upstream', summary
        status,filtered=request(base+'/api/usage?series=true&start=0&end='+str(time.time()+60)+'&provider=demo&model=demo/model&state=completed',headers=headers)
        assert status==200 and filtered['totals']['input_tokens']==10 and filtered['totals']['output_tokens']==2,filtered
        assert len(filtered['series'])==1 and filtered['series'][0]['owner_name']=='Demo'
        assert request(base+'/api/usage?series=true&start=0&end='+str(time.time()+60)+'&provider=absent',headers=headers)[1]['totals']['requests']==0
        before = request(base + '/api/config', headers=headers)[1]
        stored_before = (state.parent/'config.json').read_bytes()
        shadow_log_path = root / 'shadow.log'
        shadow_log = stack.enter_context(shadow_log_path.open('w'))
        process = subprocess.Popen([sys.executable, str(HERE / 'shadow-web.py'), '--port', '0',
            '--backend-port', base.rsplit(':', 1)[1], '--state-dir', str(state)], stdout=shadow_log, stderr=shadow_log)
        stack.callback(stop, process)
        shadow = wait_for(lambda: shadow_log_path.read_text(encoding='utf-8').split('EMP shadow: ')[-1].strip()
                          if 'EMP shadow:' in shadow_log_path.read_text(encoding='utf-8') else None)
        result['formal'] = check_page(browser, False)
        browser.navigate(shadow)
        result['shadow'] = check_page(browser, True)
        usage = browser.api('/api/shadow/usage?start=0&end=' + str(time.time()+60) + '&state=completed')
        assert usage['totals']['input_tokens'] == 10 and usage['totals']['output_tokens'] == 2, usage
        assert request(shadow + 'api/config', {'providers': []})[0] == 403
        assert request(shadow + 'api/accounts/fixture', method='DELETE')[0] == 403
        # Formal-page bootstrap rotates its management session; use the current
        # backend token for the final read, as the shadow server does itself.
        headers['X-EMP-Session'] = json.loads((state/'web-session.json').read_text(encoding='utf-8'))['token']
        status, after = request(base + '/api/config', headers=headers)
        assert status == 200, after
        assert (state.parent/'config.json').read_bytes() == stored_before
        for key in ['providers','models','accounts','settings']:
            assert before.get(key) == after.get(key), key
        assert upstream.calls == 2
        result['checks'] = ['UTF-8 assets under legacy codepage', 'formal service error bubble and recovery', 'shared Statistics views and colors', 'formal default bars', 'filtered accounting chart series', 'HTTP failure source', 'success denominator', 'native Escape',
            'nested dialogs', 'actual hover feedback in both themes', 'equal ring row heights', 'light/dark', 'English/Chinese', '125% scale', 'quota style',
            'activity/error demos', 'credential scrubbing', 'backend write rejection', 'backend unchanged']
    if args.output:
        args.output.write_text(json.dumps(result, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'passed': True, 'browser_cases': len(result['formal']) + len(result['shadow']),
                      'checks': result['checks']}))


if __name__ == '__main__':
    main()
