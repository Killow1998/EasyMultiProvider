"""Routing settings acceptance with disposable accounts and no real model calls."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser
from preview import preview


def change(browser, control, value):
    browser.execute('const node=document.getElementById(arguments[0]);node.value=arguments[1];node.dispatchEvent(new Event("change",{bubbles:true}));', control, value)


def save(browser, mode, account=''):
    change(browser, 'routing_image_mode', mode)
    if mode == 'account':
        change(browser, 'routing_image_account', account)
    browser.execute('document.getElementById("modal_submit").click();')
    wait_for(lambda: browser.execute('return !document.getElementById("routing_image_mode") && !!document.querySelector("[data-ui-action=routing-settings]");'))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--embedded', action='store_true', help='Verify product assets embedded in the supplied binary')
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-routing-ui-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        native = root / 'native-codex'
        native.mkdir()
        (native / 'auth.json').write_text(json.dumps({'tokens': {'access_token': 'native-fixture', 'account_id': 'native-owner'}}))
        (native / 'config.toml').write_text('openai_base_url = "http://127.0.0.1:9/backend-api/codex"\n')
        catalog = root / 'models.json'
        catalog.write_text(json.dumps({'models': [{'slug': 'gpt-6.1-sol'}]}))
        base, url, _, headers, upstream = start_backend(stack, args.emp.resolve(), root,
            {'auto_enable_on_start': False, 'native_catalog_path': str(catalog),
             'codex_base_url': 'http://127.0.0.1:9/backend-api/codex'},
            {'CODEX_HOME': str(native), 'PATH': '/usr/bin:/bin'})
        status, _ = request(base + '/api/accounts/import', {'id': 'image-fixture', 'prefix': 'image-fixture',
            'name': 'Image fixture <safe>', 'auth_json': {'tokens': {'access_token': 'imported-fixture', 'account_id': 'imported-owner'}}}, headers)
        assert status == 200
        frontend = base
        if not args.embedded:
            frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        if args.embedded:
            assert browser.execute('return !window.EMP_PREVIEW && !window.EMP_REVIEW_CHOICES;')
            for path in ['/assets/preview-routing.js', '/assets/preview-review.js', '/assets/copy-preview.js']:
                assert request(base + path, headers=headers)[0] == 404, path
            checks.append('Packaged product assets expose no Shadow adapters or preview state')
        browser.execute('setLanguage("en");openSettings();')
        collaboration = '[data-ui-action="setting-toggle"][data-id="external_collaboration"]'
        assert browser.execute('return document.querySelector(arguments[0]+"[aria-pressed=true]").dataset.mode;', collaboration) == 'off'
        for enabled in ['on_demand', 'on', 'off']:
            browser.execute('document.querySelector(arguments[0]+"[data-mode="+arguments[1]+"]").click();', collaboration, enabled)
            wait_for(lambda: browser.execute('return state.external_collaboration===arguments[0] && document.querySelector(arguments[1]+"[aria-pressed=true]").dataset.mode===arguments[0] && !document.querySelector(arguments[1]).disabled;', enabled, collaboration))
            assert json.loads((root / 'config.json').read_text())['external_collaboration'] == enabled
            browser.execute('closeModal();openSettings();')
            assert browser.execute('return document.querySelector(arguments[0]+"[aria-pressed=true]").dataset.mode;', collaboration) == enabled
        checks.append('Cross-service subagents defaults off despite a configured API provider; enabling/disabling persists and survives reopening Settings')
        browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
        assert browser.execute('return document.getElementById("routing_image_mode").value==="follow" && document.getElementById("routing_image_account_row").hidden;')
        assert browser.execute('return [...document.querySelectorAll("#routing_image_account option")].some(option=>option.textContent==="Image fixture <safe>") && !document.querySelector("#routing_image_account safe");')
        browser.execute('document.getElementById("routing_review").click();')
        for mode, account in [('account', 'image-fixture'), ('account', '@native'), ('auto', ''), ('follow', '')]:
            save(browser, mode, account)
            stored = json.loads((root / 'config.json').read_text())
            assert stored['image_routing'] == {'mode': mode, 'account_id': account}
            assert stored['auto_review_fallback'] is False
            browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
            assert browser.execute('return document.getElementById("routing_image_mode").value;', mode) == mode
        checks.append('All image modes persist through the configuration API and return to Settings after Save, including imported/Native selection; Auto Review remains independent')

        browser.execute('window.routingFetch=window.fetch;window.fetch=async(path,options)=>String(path)==="/api/config"&&options?.method==="POST"?new Response(JSON.stringify({error:{message:"Synthetic save failure"}}),{status:500,headers:{"Content-Type":"application/json"}}):window.routingFetch(path,options);')
        change(browser, 'routing_image_mode', 'auto')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return !!document.getElementById("modal_status").textContent;'))
        wait_for(lambda: browser.execute('return !document.getElementById("modal_submit").disabled;'))
        assert json.loads((root / 'config.json').read_text())['image_routing']['mode'] == 'follow'
        assert browser.execute('return document.getElementById("routing_image_mode").value==="auto" && !document.getElementById("routing_image_mode").disabled && state.image_routing.mode==="follow";')
        browser.execute('window.fetch=window.routingFetch;')
        assert browser.execute('return document.getElementById("modal_close").hidden && !document.getElementById("modal_back").hidden;')
        browser.escape()
        assert browser.execute('return !!document.querySelector("[data-ui-action=routing-settings]") && !document.getElementById("routing_image_mode") && document.getElementById("modal_back").hidden;')
        checks.append('Failed save preserves stored settings and editable draft; child has only Back, Escape returns one level to Settings')

        browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();window.fetch=(path,options)=>String(path)==="/api/config"&&options?.method==="POST"?new Promise(resolve=>window.releaseRoutingSave=()=>window.routingFetch(path,options).then(resolve)):window.routingFetch(path,options);')
        change(browser, 'routing_image_mode', 'auto')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return typeof window.releaseRoutingSave==="function";'))
        browser.execute('openModal("Another dialog","<div id=routing_replacement></div>","",null);window.releaseRoutingSave();')
        wait_for(lambda: browser.execute('return state.image_routing.mode==="auto";'))
        assert browser.execute('return !!document.getElementById("routing_replacement") && !document.getElementById("modal_backdrop").classList.contains("hidden");')
        browser.execute('window.fetch=window.routingFetch;')
        checks.append('A late successful save cannot close or replace a different dialog opened while the request was pending')

        for language in ['zh-CN', 'en']:
            for width in [420, 1280]:
                request(browser.base + '/window/rect', {'width': width, 'height': 800})
                browser.execute('closeModal();setLanguage(arguments[0]);openSettings();', language)
                assert browser.execute('return document.getElementById("collaboration-label").textContent;', language) == ('跨服务子代理' if language == 'zh-CN' else 'Cross-service subagents')
                assert browser.execute('const row=document.querySelector(".collaboration-setting"),choice=row.querySelector(".quota-display-choice");return Math.abs(row.getBoundingClientRect().right-choice.getBoundingClientRect().right)<=1;')
                assert browser.execute('const body=document.getElementById("modal_body");return body.scrollWidth<=body.clientWidth+1;'), browser.execute('const body=document.getElementById("modal_body"),bounds=body.getBoundingClientRect();return [...body.querySelectorAll("*")].map(node=>({tag:node.tagName,id:node.id,cls:node.className,width:node.getBoundingClientRect().width,right:node.getBoundingClientRect().right})).filter(row=>row.right>bounds.right+1);')
                browser.execute('document.querySelector("[data-shadow-help-template=collaboration-comparison]").focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                assert browser.execute('return [...document.querySelectorAll("#shadow_feedback .collaboration-modes dt")].map(node=>node.textContent);') == (['关闭','按需恢复','打开'] if language == 'zh-CN' else ['Off','On-demand recovery','On'])
                assert browser.execute('const descriptions=[...document.querySelectorAll("#shadow_feedback .collaboration-modes dd")];return descriptions.every(node=>node.textContent.length>20) && descriptions[1].textContent.includes("GPT-6 Luna") && descriptions[2].textContent.includes(arguments[0]);', '即使没有调用外部子代理' if language == 'zh-CN' else 'even when no external subagent')
                assert browser.execute('const bubble=document.getElementById("shadow_feedback"),box=bubble.getBoundingClientRect();return bubble.querySelectorAll("tbody tr").length===11 && box.left>=0 && box.right<=innerWidth+1 && box.top>=0 && box.bottom<=innerHeight+1;')
                assert browser.execute('return document.querySelector("#shadow_feedback tbody tr:nth-child(8) td").textContent;') == ('不允许' if language == 'zh-CN' else 'Not allowed')
                browser.execute('const table=document.querySelector("#shadow_feedback .collaboration-table");table.scrollTop=table.scrollHeight;table.dispatchEvent(new Event("scroll",{bubbles:true}));')
                assert browser.execute('return !document.getElementById("shadow_feedback").hidden;')
                assert browser.execute('const table=document.querySelector("#shadow_feedback .collaboration-table").getBoundingClientRect(),heading=document.querySelector("#shadow_feedback thead th").getBoundingClientRect();return heading.top>=table.top-1 && heading.bottom<=table.bottom;')
                status, png = request(browser.base + '/screenshot')
                assert status == 200
                args.output.with_name('collaboration-comparison-' + language + '-' + str(width) + '.png').write_bytes(base64.b64decode(png['value']))
                browser.escape()
                assert browser.execute('return document.getElementById("shadow_feedback").hidden && !document.getElementById("modal_backdrop").classList.contains("hidden");')
                if width == 1280:
                    status, png = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name('external-collaboration-' + language + '.png').write_bytes(base64.b64decode(png['value']))
                browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
                change(browser, 'routing_image_mode', 'account')
                assert browser.execute('return document.getElementById("modal_title").textContent===arguments[0] && !document.getElementById("routing_image_account_row").hidden;', '路由设置' if language == 'zh-CN' else 'Routing settings')
                assert browser.execute('const body=document.getElementById("modal_body"),bounds=body.getBoundingClientRect();return body.scrollWidth<=body.clientWidth+1 && [...document.querySelectorAll(".routing-settings select,.routing-settings label,#routing_review")].every(node=>{const box=node.getBoundingClientRect();return box.left>=bounds.left && box.right<=bounds.right+1;});')
                if width == 1280:
                    status, png = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name('routing-' + language + '.png').write_bytes(base64.b64decode(png['value']))
        checks.append('Simplified Chinese and English render correctly at narrow/desktop widths without overflow')
        checks.append('Help explains all three modes before the comparison, opens by keyboard, fits both viewports, scrolls without disappearing, and Escape dismisses help before Settings')
        for route, label in [('image_generation', 'Image generation'), ('image_edit', 'Image edit')]:
            assert browser.execute('const container=document.createElement("div");container.innerHTML=callReports.recordHtml({request_id:"fixture",route:arguments[0],model_id:"gpt-image-2",state:"completed"});return container.querySelector("summary").textContent.includes(arguments[1]) && container.textContent.includes("Image model");', route, label)
        assert upstream.calls == 0
        checks.append('Image generation/edit labels appear in collapsed and expanded call records; no real model requests')
        session = browser.execute('return localStorage.getItem("emp_management_session_v1");')
        shadow, _ = preview(stack, base, '', session)
        browser.navigate(shadow + '/?routing=1')
        wait_for(lambda: browser.execute('return !!document.getElementById("routing_image_mode");'))
        assert browser.execute('return !!window.EMP_PREVIEW && !!document.getElementById("shadow_routing_button") && document.querySelectorAll("#routing_image_mode option").length===3 && [...document.querySelectorAll("#routing_image_account option")].some(option=>option.value==="routing-demo");')
        browser.execute('closeModal();setLanguage("en");document.getElementById("shadow_routing_button").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_title").textContent==="Routing settings";'))
        save(browser, 'account', 'routing-demo')
        assert json.loads((root / 'config.json').read_text())['image_routing'] == {'mode':'account','account_id':'routing-demo'}
        checks.append('Unified Shadow opens the same routing dialog directly and saves only to its disposable synthetic backend')
        browser.navigate(shadow + '/?collaboration=1')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-shadow-help-template=collaboration-comparison]");'))
        checks.append('Unified Shadow directly opens the production-owned three-mode settings via collaboration preview link')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
