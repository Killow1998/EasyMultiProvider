"""Decision-model settings and native gateway requests, with synthetic services."""
import argparse
import base64
import contextlib
import json
import tempfile
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path

from candidate_web import candidate_frontend
from decision_fixture import DecisionUpstream
from e2e_refinements import Browser
from local_backend import request, start_backend, wait_for


def change(browser, control, value):
    browser.execute('const node=document.getElementById(arguments[0]);node.value=arguments[1];node.dispatchEvent(new Event("change",{bubbles:true}));', control, value)


def choose_service(browser, provider, protocol):
    change(browser, 'decision_provider', provider)
    if browser.execute('return document.getElementById("decision_api").value;') != protocol:
        change(browser, 'decision_api', protocol)
    wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--embedded', action='store_true')
    parser.add_argument('--shadow-url')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-decisions-ui-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        upstream = ThreadingHTTPServer(('127.0.0.1', 0), DecisionUpstream)
        stack.callback(upstream.server_close)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        stack.callback(upstream.shutdown)
        providers = [{'id':name, 'name':name + ' <safe>', 'base_url':f'http://127.0.0.1:{upstream.server_port}/{name}/v1',
            'auth_mode':'api_key', 'api_key':'fixture-' + name} for name in ['openai','openrouter','typesafe']]
        providers.append({**providers[0], 'id':'disabled', 'enabled':False})
        base, url, _, _ = start_backend(stack, args.emp.resolve(), root,
            {'auto_enable_on_start':False, 'providers':providers,
             'models':[{'id':'openai/luna', 'provider':'openai', 'upstream_id':'gpt-6-luna'},
                       {'id':'typesafe/jev', 'provider':'typesafe', 'upstream_id':'jev-latest'},
                       {'id':'openrouter/jev-router','provider':'openrouter','upstream_id':'typesafe/jev-router','output_modalities':['text']}]},
            {'PATH':'/usr/bin:/bin', 'HTTP_PROXY':'http://127.0.0.1:9', 'HTTPS_PROXY':'http://127.0.0.1:9',
             'ALL_PROXY':'http://127.0.0.1:9', 'NO_PROXY':'127.0.0.1,localhost'})
        frontend = base
        if not args.embedded:
            frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        browser.execute('openSettings();document.querySelector("[data-settings-category=external]").click();document.querySelector("[data-decision-open]").click();')
        assert browser.execute('return document.getElementById("decision_provider").value==="" && document.getElementById("decision_api").disabled && document.getElementById("decision_model").disabled && !document.querySelector("#decision_provider safe");')
        assert browser.execute('return [...document.querySelectorAll("#decision_provider option")].map(option=>option.value);') == ['', 'openai', 'openrouter', 'typesafe']
        choose_service(browser, 'openrouter', 'openrouter')
        change(browser, 'decision_model', '~typesafe/jev-latest')
        loaded = len(DecisionUpstream.catalogs)
        browser.execute('const reopen=modalReturn();setLanguage("en");reopen();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))
        assert browser.execute('return document.getElementById("decision_model").value==="~typesafe/jev-latest";')
        assert len(DecisionUpstream.catalogs) == loaded
        browser.escape()
        assert browser.execute('return !!document.querySelector("[data-decision-open]") && !document.getElementById("decision_model");')
        assert 'decision_provider' not in json.loads((root / 'config.json').read_text())
        checks.append('Settings loads live decision models and preserves aliases and drafts across language changes without refetching; Back discards unsaved settings')

        key = browser.api('/api/gateway')['api_key']
        gateway = {'Authorization':'Bearer ' + key}
        for name, protocol, model, route in [
            ('openai','openai','gpt-6-luna','decisions'),
            ('openrouter','openrouter','~typesafe/jev-latest','decisions'),
            ('typesafe','systemone','jev-latest','systemone')]:
            browser.execute('document.querySelector("[data-decision-open]").click();')
            choose_service(browser, name, protocol)
            choices = browser.execute('return [...document.querySelectorAll("#decision_model option")].map(option=>option.value);')
            assert 'typesafe/jev-router' not in choices and 'gpt-6.1-sol' not in choices
            change(browser, 'decision_model', model)
            assert model in choices
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda: browser.execute('return state.decision_provider===arguments[0] && !document.getElementById("decision_settings");', name))
            stored = json.loads((root / 'config.json').read_text())
            selected = next(provider for provider in stored['providers'] if provider['id'] == name)
            assert stored['decision_provider'] == name and selected['decision_api'] == {'protocol':protocol, 'model':model}
            body = {'input':'Public fixture', 'questions':[{'type':'predicate','name':'urgent','instructions':'Is this urgent?'}]} if protocol == 'openai' else {
                'state':{'task':'Public fixture'}, 'questions':{'urgent':{'type':'noul','instructions':'Is this urgent?'}}}
            status, reply = request(base + '/v1/' + route, body, gateway)
            assert status == 200 and reply['model'] == model, (status, reply)
            path, sent_headers, sent_body = DecisionUpstream.calls[-1]
            assert path == '/' + name + ('/alpha/decisions' if protocol == 'openrouter' else '/v1/' + route)
            assert sent_body == {**body, 'model':model}
            assert sent_headers['authorization'] == 'Bearer fixture-' + name
            assert key not in json.dumps(sent_headers)
        checks.append('All three account catalog formats supply actual choices, exclude text models, save defaults and forward native JSON using only the matching service key')

        DecisionUpstream.catalog_status = 503
        browser.execute('document.querySelector("[data-decision-open]").click();')
        choose_service(browser, 'openrouter', 'openrouter')
        assert browser.execute('return document.getElementById("decision_catalog_status").textContent.length>0 && document.getElementById("decision_custom_model").value==="~typesafe/jev-latest";')
        assert json.loads((root / 'config.json').read_text())['decision_provider'] == 'typesafe'
        DecisionUpstream.catalog_status = 200
        DecisionUpstream.extra_models = [('liquid/d1-test','LiquidAI: New decision model')]
        browser.execute('document.getElementById("decision_refresh").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))
        assert browser.execute('return [...document.querySelectorAll("#decision_model option")].some(option=>option.value==="liquid/d1-test") && document.getElementById("decision_model").value==="~typesafe/jev-latest";')
        DecisionUpstream.empty_catalog = True
        browser.execute('document.getElementById("decision_refresh").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))
        assert browser.execute('return [...document.querySelectorAll("#decision_model option")].map(option=>option.value);') == ['', '@custom']
        assert browser.execute('return document.getElementById("decision_catalog_status").textContent.length>0;')
        DecisionUpstream.empty_catalog = False
        browser.escape()
        checks.append('Catalog errors and empty results are visible, retain stored defaults and invent no model choices; refresh discovers new models')

        browser.execute('document.querySelector("[data-decision-open]").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))
        change(browser, 'decision_model', '@custom')
        change(browser, 'decision_custom_model', 'jev-custom-alias')
        browser.execute('window.decisionFetch=window.fetch;window.fetch=(path,options)=>String(path)==="/api/config"&&options?.method==="POST"?Promise.resolve(new Response(JSON.stringify({error:{message:"Synthetic save failure"}}),{status:500,headers:{"Content-Type":"application/json"}})):window.decisionFetch(path,options);document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_status").textContent==="Synthetic save failure" && !document.getElementById("modal_submit").disabled;'))
        assert browser.execute('return document.getElementById("decision_custom_model").value==="jev-custom-alias" && state.providers.find(provider=>provider.id==="typesafe").decision_api.model==="jev-latest";')
        browser.execute('window.fetch=window.decisionFetch;document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_settings");'))
        browser.navigate(frontend)
        browser.execute('openSettings("external");document.querySelector("[data-decision-open]").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("decision_model").disabled;'))
        assert browser.execute('return document.getElementById("decision_custom_model").value==="jev-custom-alias" && !document.getElementById("decision_custom_row").hidden;')
        checks.append('Save errors retain the draft and stored values; custom model IDs persist across reloads')

        choose_service(browser, 'openrouter', 'openrouter')
        change(browser, 'decision_model', '~typesafe/jev-latest')
        for language in ['zh-CN','en']:
            for width in [1280,520]:
                request(browser.base + '/window/rect', {'width':width, 'height':850})
                browser.execute('const reopen=modalReturn();setLanguage(arguments[0]);reopen();', language)
                _, shot = request(browser.base + '/screenshot')
                (args.output / f'decisions-{language}-{width}.png').write_bytes(base64.b64decode(shot['value']))
        checks.append('Captured Chinese and English views at desktop and narrow widths for visual review')
        change(browser, 'decision_provider', '')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return state.decision_provider==="" && !document.getElementById("decision_settings");'))
        body = {'state':'Public fixture','questions':{'urgent':{'type':'noul','instructions':'Is this urgent?'}}}
        assert request(base + '/v1/decisions', body, gateway)[0] == 400
        assert request(base + '/v1/systemone', body, {**gateway, 'X-EMP-Provider':'typesafe'})[1]['model'] == 'jev-custom-alias'
        assert len(DecisionUpstream.calls) == 4
        assert {item['id'] for item in request(base + '/v1/models', headers=gateway)[1]['data']} == {'openai/luna','openrouter/jev-router'}
        checks.append('Clearing the default retains preferences and explicit calls; decision models stay out of the Codex picker while the Jev Router text model remains available')
        for path, sent_headers in DecisionUpstream.catalogs:
            service = path.split('/')[1]
            assert sent_headers['authorization'] == 'Bearer fixture-' + service
            assert key not in json.dumps(sent_headers)
            if service == 'openrouter':
                assert path == '/openrouter/v1/models?output_modalities=decisions'
        assert request(base + '/api/decisions/models', {'provider':'openrouter','protocol':'openrouter'}, gateway)[0] == 401
        checks.append('Catalog reads use the configured service credentials and the OpenRouter decision filter; Gateway Key cannot access management discovery')

        if args.shadow_url:
            browser.navigate(args.shadow_url)
            assert browser.execute('return window.EMP_REAL_SHADOW===true;')
            browser.execute('document.getElementById("shadow_review_button").click();document.querySelector("[data-shadow-demo=gateway]").click();')
            wait_for(lambda: browser.execute('return !!document.getElementById("decision_settings");'), seconds=60)
            assert browser.execute('return !window.EMP_REAL_SHADOW && !!document.getElementById("onboarding_fixture_banner");')
            choose_service(browser, 'example', 'openrouter')
            change(browser, 'decision_model', '~typesafe/jev-latest')
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda: browser.execute('return state.decision_provider==="example" && !document.getElementById("decision_settings");'))
            checks.append('Shadow current-review entry opens the production dialog on a separate disposable backend; saving leaves real configuration alone')
    result = {'passed':True, 'checks':checks, 'mock_decision_requests':len(DecisionUpstream.calls), 'mock_catalog_requests':len(DecisionUpstream.catalogs), 'real_model_requests':0, 'embedded_assets':args.embedded}
    (args.output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
