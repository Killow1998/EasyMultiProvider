"""Context help, memory routing and owned Codex preferences with disposable state."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
import tomllib

from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser
from preview import preview


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks, viewports = [], []
    with tempfile.TemporaryDirectory(prefix='emp-codex-preferences-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        home = root / 'native'
        home.mkdir()
        original = ('# Preserve user settings\nmodel = "gpt-6.1-sol"\n'
            'openai_base_url = "http://127.0.0.1:9/v1"\n'
            '[features.async_compaction]\nenabled = false\nmodel_async_compact_threshold_percent = 73\n'
            '[memories]\ngenerate_memories = false\nuse_memories = false\n'
            'extract_model = "original-extraction" # Keep this comment\nconsolidation_model = "original-consolidation"\n')
        config_path = home / 'config.toml'
        config_path.write_text(original)
        (home / 'auth.json').write_text(json.dumps({'tokens':{'account_id':'fixture-native','access_token':'fixture-only'}}))
        catalog = root / 'models.json'
        catalog.write_text(json.dumps({'models':[{'slug':model,'display_name':model,
            'context_window':272000,'max_context_window':872000,'auto_compact_token_limit':200000,
            'effective_context_window_percent':95,'visibility':'list','base_instructions':'fixture'}
            for model in ['gpt-6.1-sol','gpt-6-luna']]}))
        base, url, state_dir, _, upstream = start_backend(stack,args.emp.resolve(),root,
            {'auto_enable_on_start':False,'native_catalog_path':str(catalog),
             'codex_base_url':'http://127.0.0.1:9/v1'},
            {'CODEX_HOME':str(home),'PATH':'/usr/bin:/bin','HTTP_PROXY':'http://127.0.0.1:9',
             'HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9',
             'NO_PROXY':'localhost,127.0.0.1'})
        def post(path, payload=None):
            headers = {'X-EMP-Session':json.loads((state_dir / 'web-session.json').read_text())['token']}
            status, value = request(base + path,payload or {'confirm_reload':True},headers)
            assert status == 200, value
            return value
        def stored():
            return tomllib.loads(config_path.read_text())
        post('/api/integration/enable')
        browser = Browser(stack,root)
        browser.navigate(url)
        browser.execute('setLanguage("en");openSettings();')
        toggle = '[data-ui-action=setting-toggle][data-id=background_compaction]'
        assert browser.execute('return document.querySelector(arguments[0]).getAttribute("aria-checked");',toggle) == 'false'
        browser.execute('document.querySelector(arguments[0]).click();',toggle)
        wait_for(lambda: stored()['features']['async_compaction']['enabled'])
        wait_for(lambda: browser.execute('return !document.querySelector(arguments[0]).disabled && state.background_compaction===true;',toggle))
        assert stored()['features']['async_compaction']['model_async_compact_threshold_percent'] == 73
        browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
        assert set(browser.execute('return [...document.querySelectorAll("#routing_memory_model option")].map(node=>node.value);')) == {'', 'gpt-6.1-sol','gpt-6-luna'}
        browser.execute('document.getElementById("routing_memory_model").value="gpt-6-luna";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !document.getElementById("routing_memory_model") && state.memory_model==="gpt-6-luna";'))
        assert stored()['memories'] == {'generate_memories':False,'use_memories':False,
            'extract_model':'gpt-6-luna','consolidation_model':'gpt-6-luna'}
        checks.append('Saving applies both memory model budgets and background activation, preserving memory permissions and the user threshold')
        for language in ['zh-CN','en']:
            for width in [390,1280]:
                request(browser.base + '/window/rect',{'width':width,'height':900})
                viewports.append({'language':language,'requested_width':width,
                    **browser.execute('return {width:innerWidth,height:innerHeight,pixel_ratio:devicePixelRatio};')})
                browser.execute('closeModal();setLanguage(arguments[0]);openSettings();',language)
                labels = ['显示','通用服务设置','外部服务设置','原生服务设置'] if language == 'zh-CN' else ['Display','General service settings','External service settings','Native service settings']
                assert browser.execute('return [...document.querySelectorAll("#modal_body [role=region]")].map(node=>document.getElementById(node.getAttribute("aria-labelledby")).textContent);') == labels
                browser.execute('document.querySelector("[aria-labelledby=settings-display-title] [data-ui-action=quota-display][data-mode=used]").click();document.querySelector("[aria-labelledby=settings-display-title] [data-presentation-style=bar]").click();closeModal();openSettings();')
                assert browser.execute('return document.querySelector("[aria-labelledby=settings-display-title] [data-ui-action=quota-display][data-mode=used]").getAttribute("aria-pressed")==="true" && document.querySelector("[aria-labelledby=settings-display-title] [data-presentation-style=bar]").getAttribute("aria-pressed")==="true";')
                assert browser.execute('return !!document.querySelector("[aria-labelledby=settings-general-title] [data-id=auto_enable_on_start]") && !!document.querySelector("[aria-labelledby=settings-general-title] [data-id=external_collaboration]") && !!document.querySelector("[aria-labelledby=settings-external-title] [data-id=subscription_search]") && !!document.querySelector("[aria-labelledby=settings-native-title] [data-id=background_compaction]") && document.querySelectorAll("[data-ui-action=routing-settings]").length===1;')
                assert browser.execute('const node=document.getElementById("modal_body");return node.scrollWidth<=node.clientWidth+1;')
                screenshot(browser,args.output,f'settings-{language}-{width}')
                browser.execute('document.getElementById("background-compaction-label").parentNode.querySelector(".shadow-help").focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                text = browser.execute('return document.getElementById("shadow_feedback").textContent;')
                assert '73%' in text and 'GA' in text and '0.163' in text
                assert ('重启 Codex' if language == 'zh-CN' else 'restart Codex') in text
                browser.escape()
                browser.execute('document.querySelector("[aria-labelledby=settings-native-title] [data-ui-action=routing-settings]").scrollIntoView({block:"center"});')
                assert browser.execute('const box=document.querySelector("[aria-labelledby=settings-native-title] [data-ui-action=routing-settings]").getBoundingClientRect();return box.top>=0 && box.bottom<=innerHeight;')
                screenshot(browser,args.output,f'native-settings-{language}-{width}')
                browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
                assert browser.execute('return document.getElementById("routing_memory_model").value;') == 'gpt-6-luna'
                browser.execute('document.querySelector("label[for=routing_memory_model]").parentNode.querySelector(".shadow-help").focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                text = browser.execute('return document.getElementById("shadow_feedback").textContent;')
                assert 'Native' in text and 'GPT' in text
                assert browser.execute('const node=document.getElementById("modal_body");return node.scrollWidth<=node.clientWidth+1;')
                screenshot(browser,args.output,f'routing-{language}-{width}')
                browser.escape()
                browser.execute('closeModal();editAccount(state.native_account.id);')
                wait_for(lambda: browser.execute('return !!document.querySelector("[name=subscription_context]");'))
                browser.execute('document.querySelector(\'[data-shadow-context-help="0"]\').focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                text = browser.execute('return document.getElementById("shadow_feedback").textContent;')
                assert all(number in text for number in ['272,000','258,400','200,000','198,560','73%']), text
                assert browser.execute('const node=document.getElementById("modal_body");return node.scrollWidth<=node.clientWidth+1;')
                screenshot(browser,args.output,f'context-{language}-{width}')
                browser.escape()
                browser.execute('const node=document.querySelector("[name=subscription_context]");node.focus();node.value="100000";node.dispatchEvent(new Event("input",{bubbles:true}));document.querySelector(\'[data-shadow-context-help="0"]\').focus();')
                wait_for(lambda: browser.execute('return document.getElementById("shadow_feedback").textContent.includes("90,000");'))
                assert browser.execute('return document.getElementById("shadow_feedback").textContent.includes("95,000");')
                browser.escape()
                browser.execute('closeModal();openManualModelModal("demo/model");document.getElementById("modal_model_context").value="100000";document.getElementById("modal_model_context").dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("modal_model_context_help").focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                assert browser.execute('const text=document.getElementById("shadow_feedback").textContent;return text.includes("95,000") && text.includes("80,000");')
                browser.escape()
        checks.append('Chinese/English at narrow/desktop widths: grouped settings keep local display choices, service controls and a single native routing entry; context and Beta help fit without overflow')
        browser.execute('closeModal();openSettings();document.querySelector("[data-ui-action=routing-settings]").click();window.savedFetch=window.fetch;window.fetch=(path,options)=>String(path)==="/api/config"&&options?.method==="POST"?Promise.resolve(new Response(JSON.stringify({error:{message:"Fixture save failure"}}),{status:500})):window.savedFetch(path,options);document.getElementById("routing_memory_model").value="gpt-6.1-sol";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("modal_status").textContent && !document.getElementById("modal_submit").disabled;'))
        assert stored()['memories']['extract_model'] == 'gpt-6-luna'
        assert browser.execute('return document.getElementById("routing_memory_model").value==="gpt-6.1-sol" && state.memory_model==="gpt-6-luna";')
        browser.execute('window.fetch=window.savedFetch;closeModal();')
        lease = home / 'easy-multi-provider/integration/memory-model.json'
        saved_lease, before = lease.read_bytes(), stored()
        headers = {'X-EMP-Session':json.loads((state_dir / 'web-session.json').read_text())['token']}
        update = request(base + '/api/config',headers=headers)[1]
        update.update(background_compaction=False,memory_model='gpt-6.1-sol')
        lease.write_text('invalid fixture journal')
        try:
            status, error = request(base + '/api/config',update,headers)
            assert status == 400 and error.get('error'), (status,error)
            assert stored() == before, 'A rejected memory write must roll back the background toggle'
            persisted = request(base + '/api/config',headers=headers)[1]
            assert persisted['memory_model'] == 'gpt-6-luna' and persisted['background_compaction'] is True
        finally:
            lease.write_bytes(saved_lease)
        post('/api/integration/restore')
        assert stored() == tomllib.loads(original)
        assert '# Preserve user settings' in config_path.read_text() and '# Keep this comment' in config_path.read_text()
        assert request(base + '/healthz')[0] == 200
        post('/api/integration/enable')
        assert stored()['memories']['extract_model'] == 'gpt-6-luna'
        assert stored()['features']['async_compaction']['enabled'] is True
        checks.append('Failed saves preserve the draft; a rejected memory journal rolls back the background toggle without committing settings; Restore Native and re-enable preserve original values/comments and keep EMP running')
        session = browser.execute('return localStorage.getItem("emp_management_session_v1");')
        shadow, _ = preview(stack,base,'',session)
        browser.navigate(shadow + '/?review=1')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-shadow-demo=tray]");'))
        assert browser.execute('return [...document.querySelectorAll("#modal_body [data-shadow-demo]")].map(node=>node.dataset.shadowDemo);') == ['settings','bills','context','routing','tray']
        browser.execute('document.querySelector("[data-shadow-demo=tray]").click();')
        assert browser.execute('return [...document.querySelectorAll("#modal_body [role=img]")].every(node=>getComputedStyle(node).maskImage.startsWith("url("));')
        screenshot(browser,args.output,'mac-template')
        checks.append('Shadow groups current changes in the existing review dialog and previews the actual monochrome PNG on light/dark backgrounds')
        post('/api/quit')
        wait_for(lambda: stored() == tomllib.loads(original))
        assert upstream.calls == 0
        checks.append('Quit restores both setting groups; no real model calls or user state used')
    args.output.write_text(json.dumps({'passed':True,'checks':checks,'viewports':viewports,'real_model_calls':0},indent=2) + '\n')
    print(json.dumps({'passed':True,'checks':checks}))


def screenshot(browser, output, suffix):
    _, png = request(browser.base + '/screenshot')
    output.with_name(output.stem + '-' + suffix + '.png').write_bytes(base64.b64decode(png['value']))


if __name__ == '__main__':
    main()
