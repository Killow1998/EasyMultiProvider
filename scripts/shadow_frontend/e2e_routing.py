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
    checks, viewports = [], []
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
        assert browser.execute('return !window.EMP_PREVIEW && !window.EMP_REVIEW_CHOICES;')
        for path in ['/assets/preview-actions.js', '/assets/preview-review.js', '/assets/copy-preview.js']:
            assert browser.execute('return fetch(arguments[0]).then(response=>response.status);', path) == 404, path
        checks.append('Product assets expose no Shadow adapters or preview state')
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
        assert browser.execute('return document.getElementById("routing_adaptive_effort").getAttribute("aria-checked")==="false" && document.getElementById("routing_effort_classifier").disabled;')
        browser.execute('document.getElementById("routing_adaptive_effort").click();')
        change(browser, 'routing_effort_classifier', 'gpt-6.1-sol')
        change(browser, 'routing_effort_max', 'high')
        save(browser, 'follow')
        assert json.loads((root / 'config.json').read_text())['adaptive_effort'] == {
            'enabled':True, 'classifier_model':'gpt-6.1-sol', 'max_effort':'high'}
        browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
        assert browser.execute('return document.getElementById("routing_adaptive_effort").getAttribute("aria-checked")==="true" && document.getElementById("routing_effort_max").value==="high";')
        browser.execute('document.getElementById("routing_adaptive_effort").click();')
        save(browser, 'follow')
        browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
        checks.append('Automatic effort defaults off; classifier and ceiling save, reopen and disable without model calls')
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
                viewports.append({'language': language, 'requested_width': width,
                    **browser.execute('return {width:innerWidth,height:innerHeight,pixel_ratio:devicePixelRatio};')})
                browser.execute('closeModal();setLanguage(arguments[0]);openSettings();', language)
                assert browser.execute('return document.getElementById("collaboration-label").textContent;', language) == ('跨服务子代理' if language == 'zh-CN' else 'Cross-service subagents')
                assert browser.execute('const row=document.querySelector(".collaboration-setting"),choice=row.querySelector(".quota-display-choice");return Math.abs(row.getBoundingClientRect().right-choice.getBoundingClientRect().right)<=1;')
                assert browser.execute('const body=document.getElementById("modal_body");return body.scrollWidth<=body.clientWidth+1;'), browser.execute('const body=document.getElementById("modal_body"),bounds=body.getBoundingClientRect();return [...body.querySelectorAll("*")].map(node=>({tag:node.tagName,id:node.id,cls:node.className,width:node.getBoundingClientRect().width,right:node.getBoundingClientRect().right})).filter(row=>row.right>bounds.right+1);')
                browser.execute('document.querySelector("[data-shadow-help-template=collaboration-comparison]").focus();')
                wait_for(lambda: browser.execute('return !document.getElementById("shadow_feedback").hidden;'))
                assert browser.execute('return [...document.querySelectorAll("#shadow_feedback .collaboration-modes dt")].map(node=>node.textContent);') == (['关闭','按需恢复','打开'] if language == 'zh-CN' else ['Off','On-demand recovery','On'])
                assert browser.execute('const descriptions=[...document.querySelectorAll("#shadow_feedback .collaboration-modes dd")];return descriptions.every(node=>node.textContent.length>20) && descriptions[1].textContent.includes("GPT-6 Luna") && descriptions[2].textContent.includes("0.164");')
                assert browser.execute('const bubble=document.getElementById("shadow_feedback");return bubble.querySelector(".collaboration-takeaway").textContent.includes("0.164") && !bubble.querySelector("details").open && !bubble.textContent.includes("Terra");')
                status, png = request(browser.base + '/screenshot')
                assert status == 200
                args.output.with_name('collaboration-summary-' + language + '-' + str(width) + '.png').write_bytes(base64.b64decode(png['value']))
                browser.execute('document.querySelector("#shadow_feedback summary").focus();document.querySelector("#shadow_feedback summary").click();')
                wait_for(lambda: browser.execute('const bubble=document.getElementById("shadow_feedback"),box=bubble.getBoundingClientRect();return !bubble.hidden && bubble.querySelector("details").open && box.left>=0 && box.right<=innerWidth+1 && box.top>=0 && box.bottom<=innerHeight+1;'))
                for metric, direction in [('tps', '越高越好' if language == 'zh-CN' else 'higher is better'), ('time', '越低越好' if language == 'zh-CN' else 'lower is better')]:
                    assert browser.execute('const heading=document.getElementById("collaboration-"+arguments[0]+"-title"),table=document.querySelector("#shadow_feedback table[aria-labelledby="+heading.id+"]");return heading.textContent.includes(arguments[1]) && table.querySelectorAll("tbody tr").length===10 && [...table.querySelectorAll("td")].every(cell=>(cell.textContent.match(/×/g)||[]).length<=1);', metric, direction)
                    assert browser.execute('const table=document.querySelector("#shadow_feedback table[aria-labelledby=collaboration-"+arguments[0]+"-title]"),row=[...table.querySelectorAll("tbody tr")].find(row=>row.textContent.includes("GLM-5.3-Flash"));return row.querySelector("td").textContent;', metric) == ('不允许' if language == 'zh-CN' else 'Not allowed')
                    browser.execute('const bubble=document.getElementById("shadow_feedback"),title=document.getElementById("collaboration-"+arguments[0]+"-title");bubble.scrollTop+=title.getBoundingClientRect().top-bubble.getBoundingClientRect().top-16;', metric)
                    assert browser.execute('const table=document.querySelector("#shadow_feedback table[aria-labelledby=collaboration-"+arguments[0]+"-title]"),box=table.parentElement.getBoundingClientRect(),heading=table.querySelector("thead th").getBoundingClientRect();return !document.getElementById("shadow_feedback").hidden && heading.top>=box.top-1 && heading.bottom<=box.bottom;', metric)
                    status, png = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name('collaboration-' + metric + '-' + language + '-' + str(width) + '.png').write_bytes(base64.b64decode(png['value']))
                assert browser.execute('''
                  const title=document.getElementById('subscription-comparison-title');
                  const table=document.querySelector('table[aria-labelledby="subscription-comparison-title"]');
                  const rows=[...table.querySelectorAll('tbody tr')];
                  return title.textContent===arguments[0] && rows.length===5 &&
                    rows.map(row=>row.querySelector('th').textContent).join(',')==='low,medium,high,xhigh,max' &&
                    [...table.querySelectorAll('td')].every(cell=>/^\\d+\\.\\d{2}×$/.test(cell.textContent) && cell.title.includes('20')) &&
                    !/\\b(?:egg|yht)\\b/i.test(document.getElementById('shadow_feedback').textContent);
                ''', '导入订阅与原生直连' if language == 'zh-CN' else 'Imported subscriptions vs native direct')
                browser.execute('const bubble=document.getElementById("shadow_feedback"),title=document.getElementById("subscription-comparison-title");bubble.scrollTop+=title.getBoundingClientRect().top-bubble.getBoundingClientRect().top-16;')
                assert browser.execute('const bubble=document.getElementById("shadow_feedback"),table=document.querySelector("table[aria-labelledby=subscription-comparison-title]"),box=table.parentElement.getBoundingClientRect();return !bubble.hidden && bubble.scrollWidth<=bubble.clientWidth+1 && box.left>=0 && box.right<=innerWidth;')
                _, png = request(browser.base + '/screenshot')
                args.output.with_name('subscription-comparison-' + language + '-' + str(width) + '.png').write_bytes(base64.b64decode(png['value']))
                browser.execute('document.querySelector("#shadow_feedback .collaboration-table").focus({preventScroll:true});')
                browser.escape()
                assert browser.execute('return document.getElementById("shadow_feedback").hidden && !document.getElementById("modal_backdrop").classList.contains("hidden") && document.activeElement.matches("[data-shadow-help-template=collaboration-comparison]");')
                if width == 1280:
                    status, png = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name('external-collaboration-' + language + '.png').write_bytes(base64.b64decode(png['value']))
                browser.execute('document.querySelector("[data-ui-action=routing-settings]").click();')
                change(browser, 'routing_image_mode', 'account')
                assert browser.execute('return document.getElementById("modal_title").textContent===arguments[0] && !document.getElementById("routing_image_account_row").hidden;', '路由设置' if language == 'zh-CN' else 'Routing settings')
                assert browser.execute('const body=document.getElementById("modal_body"),bounds=body.getBoundingClientRect();return body.scrollWidth<=body.clientWidth+1 && [...document.querySelectorAll(".routing-settings select,.routing-settings label,#routing_review")].every(node=>{const box=node.getBoundingClientRect();return box.left>=bounds.left && box.right<=bounds.right+1;});')
                browser.execute('document.getElementById("routing_effort_max").scrollIntoView({block:"end"});')
                _, png = request(browser.base + '/screenshot')
                args.output.with_name('adaptive-effort-' + language + '-' + str(width) + '.png').write_bytes(base64.b64decode(png['value']))
                if width == 1280:
                    status, png = request(browser.base + '/screenshot')
                    assert status == 200
                    args.output.with_name('routing-' + language + '.png').write_bytes(base64.b64decode(png['value']))
        checks.append('Simplified Chinese and English render correctly at narrow/desktop widths without overflow')
        checks.append('Help leads with the mode recommendation, omits Terra, and expands into separate higher-is-better speed and lower-is-better time tables with one value per cell; keyboard focus, scrolling and Escape preserve Settings')
        checks.append('Imported/native direct comparison shows five efforts and 20 matched pairs per effort in the existing help, without account names, at narrow/desktop widths in both languages')
        for route, label in [('image_generation', 'Image generation'), ('image_edit', 'Image edit')]:
            assert browser.execute('const container=document.createElement("div");container.innerHTML=callReports.recordHtml({request_id:"fixture",route:arguments[0],model_id:"gpt-image-2",state:"completed"});return container.querySelector("summary").textContent.includes(arguments[1]) && container.textContent.includes("Image model");', route, label)
        assert upstream.calls == 0
        checks.append('Image generation/edit labels appear in collapsed and expanded call records; no real model requests')
        for fallback in [None, 'classifier_timeout']:
            assert browser.execute('''
              const node=document.createElement('div');
              node.innerHTML=callReports.recordHtml({request_id:'auto',route:'responses',model_id:'gpt-6-astra',state:'completed',
                configured_effort:'xhigh',requested_effort:'low',adaptive_effort:{classifier_model:'gpt-6-luna',classifier_ms:2000,fallback:arguments[0]}});
              return node.textContent.includes('Configured reasoning effort') && node.textContent.includes('gpt-6-luna') &&
                node.textContent.includes('2.00 s') && node.textContent.includes(arguments[0]?'configured effort retained':'Automatic');
            ''', fallback)
        checks.append('Call details distinguish configured/selected effort, classifier time and fallback')
        session = browser.execute('return localStorage.getItem("emp_management_session_v1");')
        shadow, _ = preview(stack, base, '', session)
        browser.navigate(shadow + '/?routing=1')
        wait_for(lambda: browser.execute('return !!document.getElementById("routing_image_mode");'))
        assert browser.execute('return !!window.EMP_PREVIEW && !document.getElementById("shadow_routing_button") && document.querySelectorAll("#routing_image_mode option").length===3 && [...document.querySelectorAll("#routing_image_account option")].some(option=>option.value==="routing-demo");')
        browser.execute('closeModal();setLanguage("en");document.getElementById("shadow_review_button").click();document.querySelector("#modal_body [data-shadow-demo=routing]").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_title").textContent==="Routing settings";'))
        save(browser, 'account', 'routing-demo')
        assert json.loads((root / 'config.json').read_text())['image_routing'] == {'mode':'account','account_id':'routing-demo'}
        checks.append('Unified Shadow opens the same routing dialog directly and saves only to its disposable synthetic backend')
        browser.navigate(shadow + '/?collaboration=1')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-shadow-help-template=collaboration-comparison]") && !document.getElementById("shadow_feedback").hidden;'))
        browser.execute('closeModal();fill();')
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");')
        checks.append('Unified Shadow opens the production-owned three-mode settings once via the collaboration link; later renders preserve closure')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'viewports': viewports, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
