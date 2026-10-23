"""Ultra save/reset, published catalogs and real-data Shadow with isolated state."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

from e2e_environment import request, start_backend, wait_for, stop
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-ultra-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        home = root / 'native'
        home.mkdir()
        (home / 'auth.json').write_text(json.dumps({'tokens':{'account_id':'fixture','access_token':'fixture-only'}}))
        (home / 'config.toml').write_text('model="gpt-ultra"\n[memories]\nextract_model="gpt-original-extraction"\nconsolidation_model="gpt-original-consolidation"\n')
        model = {'slug':'gpt-ultra','display_name':'GPT Ultra fixture','visibility':'list',
            'context_window':272000,'max_context_window':872000,'base_instructions':'Synthetic fixture',
            'shell_type':'shell_command','supported_in_api':True,'priority':1,'support_verbosity':False,
            'truncation_policy':{'mode':'tokens','limit':10000},'experimental_supported_tools':[],
            'supports_reasoning_summaries':True,
            'default_reasoning_level':'high','multi_agent_reasoning_effort':'xhigh','multi_agent_version':'v2',
            'supported_reasoning_levels':[{'effort':level,'description':level} for level in ['low','high','xhigh','ultra']]}
        catalog = root / 'models.json'
        catalog.write_text(json.dumps({'models':[model,{**model,'slug':'gpt-ordinary','display_name':'Ordinary',
            'supported_reasoning_levels':[{'effort':'high','description':'high'}]}]}))
        base, url, state_dir, headers, upstream = start_backend(stack,args.emp.resolve(),root,
            {'auto_enable_on_start':False,'native_catalog_path':str(catalog),'codex_base_url':'http://127.0.0.1:9/v1',
             'catalog_presentations':{'gpt-ultra':{'catalog_alias':'My model'}}},
            {'CODEX_HOME':str(home),'PATH':'/usr/bin:/bin','HTTP_PROXY':'http://127.0.0.1:9',
             'HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'localhost,127.0.0.1'})

        def read(path):
            if path == '/v1/models':
                path += '?client_version=0.163.0'
            token = json.loads((state_dir/'web-session.json').read_text())['token']
            status, body = request(base + path,headers={'X-EMP-Session':token})
            assert status == 200, body
            return body

        def current_model():
            return next(entry for entry in read('/v1/models')['models'] if entry['slug']=='gpt-ultra')

        def published_model():
            return next(entry for entry in json.loads(published_path.read_text())['models'] if entry['slug']=='gpt-ultra')

        assert request(base+'/api/integration/enable',{'confirm_reload':True},headers)[0] == 200
        status, refreshed = request(base+'/api/catalog/refresh',{},headers)
        assert status == 200, refreshed
        published_path = Path(refreshed['catalog_path'])
        browser = Browser(stack,root)
        browser.navigate(url)
        def open_settings():
            browser.execute('closeModal();openSettings();document.querySelector("[data-ui-action=ultra-settings]").click();')
        def save(value):
            browser.execute('document.querySelector("[data-ultra-family]").value=arguments[0];document.getElementById("modal_submit").click();',value)
            wait_for(lambda: browser.execute('return !document.getElementById("ultra_settings");'))
        open_settings()
        assert browser.execute('return document.querySelector("label[for=ultra_effort_0]").textContent;') == model['display_name']
        assert browser.execute('return [...document.querySelectorAll("[data-ultra-family]")].map(node=>node.dataset.ultraFamily);') == ['gpt-ultra']
        assert browser.execute('return [...document.querySelector("[data-ultra-family]").options].map(node=>node.value);') == ['auto','low','high','xhigh']
        assert browser.execute('return !document.getElementById("modal_back").hidden && document.getElementById("modal_close").hidden;')
        baseline = current_model()
        assert next(entry for entry in read('/api/config')['catalog_models'] if entry['id']=='gpt-ultra')['ultra_default_effort'] == 'xhigh'
        review = read('/api/config')['auto_review_fallback']
        save('low')
        assert current_model() == {**baseline,'multi_agent_reasoning_effort':'low'}
        assert next(entry for entry in read('/api/config')['catalog_models'] if entry['id']=='gpt-ultra')['ultra_default_effort'] == 'xhigh'
        wait_for(lambda: published_model()['multi_agent_reasoning_effort']=='low')
        (args.output/'catalog-low.json').write_text(json.dumps(read('/v1/models')))
        assert read('/api/config')['auto_review_fallback'] == review
        assert read('/api/config')['catalog_presentations']['gpt-ultra']['catalog_alias'] == 'My model'
        checks.append('Ultra identifies the model by its upstream name; save changes only its effort mapping and preserves the catalog alias and Auto Review')
        for language in ['zh-CN','en']:
            for width in [390,1280]:
                request(browser.base+'/window/rect',{'width':width,'height':900})
                browser.execute('setLanguage(arguments[0]);',language)
                open_settings()
                assert browser.execute('return document.querySelector("[data-ultra-family]").value;') == 'low'
                assert browser.execute('const node=document.getElementById("modal_body");return node.scrollWidth<=node.clientWidth+1;')
                assert ('重启 Codex Runtime' if language=='zh-CN' else 'restart Codex Runtime') in browser.execute('return document.getElementById("ultra_settings").textContent;')
                status, image = request(browser.base+'/screenshot')
                assert status == 200
                (args.output/f'ultra-{language}-{width}.png').write_bytes(base64.b64decode(image['value']))
        checks.append('Chinese/English desktop and narrow layouts, shared back navigation and restart instructions')
        # Fail a save at the actual browser boundary; the draft must survive.
        browser.execute('window.ultraOriginalFetch=window.fetch;window.fetch=(input,options)=>String(input).endsWith("/api/config")&&options?.method==="POST" ? Promise.resolve(new Response(JSON.stringify({error:{message:"Fixture save rejected"}}),{status:503,headers:{"Content-Type":"application/json"}})) : window.ultraOriginalFetch(input,options);document.querySelector("[data-ultra-family]").value="high";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_status").textContent.includes("Fixture save rejected") && !document.querySelector("[data-ultra-family]").disabled;'))
        assert read('/api/config')['ultra_reasoning_efforts'] == {'gpt-ultra':'low'}
        assert browser.execute('return document.querySelector("[data-ultra-family]").value;') == 'high'
        browser.execute('window.fetch=window.ultraOriginalFetch;')
        save('auto')
        assert read('/api/config')['ultra_reasoning_efforts'] == {}
        assert current_model() == baseline
        wait_for(lambda: published_model()['multi_agent_reasoning_effort']=='xhigh')
        (args.output/'catalog-default.json').write_text(json.dumps(read('/v1/models')))
        checks.append('Rejected save preserves the draft; Follow official default removes the override')
        model['multi_agent_reasoning_effort'] = 'high'
        catalog.write_text(json.dumps({'models':[model]}))
        assert current_model()['multi_agent_reasoning_effort'] == 'high'
        checks.append('Reset preference follows a later upstream default')
        browser.execute('closeModal();')
        browser.navigate(url)
        open_settings()
        assert browser.execute('return document.querySelector("[data-ultra-family] option[value=auto]").textContent;').endswith(' · high')
        assert browser.execute('return document.querySelector("[data-ultra-family]").value;') == 'auto'
        checks.append('Official-default label tracks the upstream value without turning Auto into a fixed override')

        catalog.write_text(json.dumps({'models':[model,{**model,'slug':'gpt-ordinary','display_name':'Ordinary',
            'supported_reasoning_levels':[{'effort':'high','description':'high'}]}]}))
        browser.navigate(url)
        original_memory = {'extract_model':'gpt-original-extraction','consolidation_model':'gpt-original-consolidation'}
        memory_stages = {'extract_model':'routing_memory_extraction_model','consolidation_model':'routing_memory_model'}
        for extraction, consolidation in [('gpt-ordinary','gpt-ultra'),('','gpt-ultra'),('gpt-ordinary',''),('','')]:
            browser.execute('closeModal();routingSettings.open();')
            for key, element in memory_stages.items():
                caption = browser.execute('return document.getElementById(arguments[0]).options[0].textContent;',element)
                assert original_memory[key] in caption
                assert original_memory[next(other for other in memory_stages if other!=key)] not in caption
            browser.execute('document.getElementById("routing_memory_extraction_model").value=arguments[0];document.getElementById("routing_memory_model").value=arguments[1];document.getElementById("modal_submit").click();',extraction,consolidation)
            wait_for(lambda: browser.execute('return !document.getElementById("routing_memory_model");'))
            assert read('/api/config')['codex_context_settings']['memory_default_models'] == original_memory
            assert read('/api/config')['memory_extraction_model'] == extraction
            assert read('/api/config')['memory_model'] == consolidation
            expected_memory = {'extract_model':extraction or original_memory['extract_model'],
                'consolidation_model':consolidation or original_memory['consolidation_model']}
            assert tomllib.loads((home/'config.toml').read_text())['memories'] == expected_memory
        checks.append('Extraction and consolidation save different models; resetting either restores only its original model and leaves the other selection intact')
        browser.execute('routingSettings.open();document.getElementById("routing_memory_extraction_model").value="gpt-ordinary";document.getElementById("routing_memory_model").value="gpt-ultra";')
        for language in ['zh-CN','en']:
            for width in [390,1280]:
                request(browser.base+'/window/rect',{'width':width,'height':900})
                browser.execute('const reopen=modalReturn();setLanguage(arguments[0]);reopen();',language)
                assert browser.execute('return document.getElementById("routing_memory_extraction_model").value;') == 'gpt-ordinary'
                assert browser.execute('return document.getElementById("routing_memory_model").value;') == 'gpt-ultra'
                assert browser.execute('const node=document.getElementById("modal_body");return node.scrollWidth<=node.clientWidth+1;')
                _, image = request(browser.base+'/screenshot')
                (args.output/f'memory-{language}-{width}.png').write_bytes(base64.b64decode(image['value']))
        browser.execute('window.memoryOriginalFetch=window.fetch;window.fetch=(input,options)=>String(input).endsWith("/api/config")&&options?.method==="POST" ? Promise.resolve(new Response(JSON.stringify({error:{message:"Fixture memory save rejected"}}),{status:503,headers:{"Content-Type":"application/json"}})) : window.memoryOriginalFetch(input,options);document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_status").textContent.includes("Fixture memory save rejected") && !document.getElementById("routing_memory_extraction_model").disabled;'))
        assert read('/api/config')['memory_extraction_model'] == read('/api/config')['memory_model'] == ''
        assert tomllib.loads((home/'config.toml').read_text())['memories'] == original_memory
        assert browser.execute('return document.getElementById("routing_memory_extraction_model").value;') == 'gpt-ordinary'
        assert browser.execute('return document.getElementById("routing_memory_model").value;') == 'gpt-ultra'
        browser.execute('window.fetch=window.memoryOriginalFetch;closeModal();')
        checks.append('Both drafts survive dialog reopening and rejected saves; Chinese/English desktop and narrow memory settings fit without overflow')
        saved_native_config = (home/'config.toml').read_text()
        for configured, expected in [
            ({}, {'extract_model':'gpt-5.6-luna','consolidation_model':'gpt-5.6-terra'}),
            ({'extract_model':'gpt-custom'}, {'extract_model':'gpt-custom','consolidation_model':'gpt-5.6-terra'}),
            ({'consolidation_model':'gpt-custom'}, {'extract_model':'gpt-5.6-luna','consolidation_model':'gpt-custom'}),
        ]:
            fixture_config = saved_native_config.split('[memories]')[0]
            if configured:
                fixture_config += '[memories]\n' + ''.join(f'{key}="{value}"\n' for key, value in configured.items())
            (home/'config.toml').write_text(fixture_config)
            browser.navigate(url)
            browser.execute('routingSettings.open();')
            assert read('/api/config')['codex_context_settings']['memory_default_models'] == expected
            for key, element in memory_stages.items():
                caption = browser.execute('return document.getElementById(arguments[0]).options[0].textContent;',element)
                assert expected[key] in caption
                assert browser.execute('return document.getElementById(arguments[0]).value;',element) == ''
            assert (home/'config.toml').read_text() == fixture_config
        (home/'config.toml').write_text(saved_native_config)
        checks.append('Unset memory stages show verified Codex defaults; per-stage overrides take precedence without writing defaults to config')
        # Launch the real-data read-only bridge, never a demonstration fallback.
        read('/api/calls')
        before = (state_dir.parent/'config.json').read_bytes()
        log_path = root/'shadow.log'
        log = stack.enter_context(log_path.open('w'))
        process = subprocess.Popen([sys.executable,str(Path(__file__).with_name('shadow-web.py')),
            '--emp',str(args.emp.resolve()),'--codex-home',str(home),'--scratch-dir',str(root/'shadow-state'),
            '--port','0','--backend-port',base.rsplit(':',1)[1],'--state-dir',str(state_dir)],stdout=log,stderr=log)
        stack.callback(stop,process)
        shadow = wait_for(lambda: log_path.read_text().split('EMP real-data Shadow: ')[-1].strip() if 'EMP real-data Shadow:' in log_path.read_text() else None)
        browser.navigate(shadow)
        browser.execute('openReview();document.querySelector("[data-shadow-demo=ultra]").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("ultra_settings");'))
        assert browser.execute('return document.querySelector("[data-ultra-family] option[value=auto]").textContent;').endswith(' · high')
        assert browser.execute('return state.codex_context_settings.memory_default_models;') == original_memory
        save('low')
        assert browser.execute('return state.ultra_reasoning_efforts["gpt-ultra"];') == 'low'
        assert (state_dir.parent/'config.json').read_bytes() == before
        assert current_model()['multi_agent_reasoning_effort'] == 'high'
        browser.execute('closeModal();openReview();document.querySelector("[data-shadow-demo=routing]").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("routing_memory_extraction_model");'))
        browser.execute('document.getElementById("routing_memory_extraction_model").value="gpt-ordinary";document.getElementById("routing_memory_model").value="gpt-ultra";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("routing_memory_model");'))
        browser.execute('routingSettings.open();')
        assert browser.execute('return document.getElementById("routing_memory_extraction_model").value;') == 'gpt-ordinary'
        assert browser.execute('return document.getElementById("routing_memory_model").value;') == 'gpt-ultra'
        assert (state_dir.parent/'config.json').read_bytes() == before
        assert tomllib.loads((home/'config.toml').read_text())['memories'] == original_memory
        assert upstream.calls == 0
        checks.append('Shadow current changes opens the production Ultra and memory controllers; independent preview saves never write the source instance or its Codex preferences')
        (args.output/'catalog.json').write_text(json.dumps(read('/v1/models')))
    report = {'checks':checks,'real_model_calls':0}
    (args.output/'result.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps(report,ensure_ascii=False))


if __name__ == '__main__':
    main()
