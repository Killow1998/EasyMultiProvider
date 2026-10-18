"""Verify account settings are resolved in the import page against synthetic state."""
import argparse
import base64
import contextlib
import json
import tempfile
from pathlib import Path
from preview import fixture, preview
from e2e import Browser
from e2e_environment import request, wait_for


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-import-merge-e2e-') as temp,contextlib.ExitStack() as stack:
        endpoint,target,bundle=fixture(stack,args.emp.resolve(),Path(temp))
        backend,_,headers,upstream=endpoint
        frontend,_=preview(stack,backend,bundle,headers['X-EMP-Session'])
        native=target/'codex_home/auth.json';native_bytes=native.read_bytes()
        browser=Browser(stack,Path(temp))
        for language,theme,width in [('zh-CN','light',1280),('en','dark',640)]:
            request(browser.base+'/window/rect',{'width':width,'height':900})
            browser.navigate(frontend+'/')
            browser.execute('for(const [id,value] of [["language_select",arguments[0]],["theme_select",arguments[1]]]){const select=document.getElementById(id);select.value=value;select.dispatchEvent(new Event("change",{bubbles:true}));}',language,theme)
            wait_for(lambda:json.loads((target/'config.json').read_text()).get('language')==language)
            before=(target/'config.json').read_bytes()
            browser.execute('document.getElementById("shadow_demo_button").click();document.getElementById("demo_account_import").click();document.querySelector("[data-migration-decode]").click();')
            wait_for(lambda:browser.execute('return document.querySelectorAll("[data-migration-preference-kind]").length===2;'),seconds=90)
            initial=browser.execute('return {disabled:document.getElementById("modal_submit").disabled,rows:[...document.querySelectorAll("#migration_preview_rows .migration-preview-row")].map(row=>row.textContent),status:document.getElementById("modal_status").textContent};')
            assert initial['disabled'] and len(initial['rows'])==1, initial
            assert browser.execute('return document.getElementById("modal_body").scrollWidth<=document.getElementById("modal_body").clientWidth;')
            assert (target/'config.json').read_bytes()==before
            status,png=request(browser.base+'/screenshot');assert status==200
            args.output.with_name(f'import-account-conflicts-{theme}-20261014.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('document.getElementById("modal_back").click();')
            assert browser.execute('return document.getElementById("shadow_migration_file").files[0].name==="account-example.emp";')
            assert (target/'config.json').read_bytes()==before
            browser.execute('document.querySelector("[data-migration-decode]").click();')
            wait_for(lambda:browser.execute('return document.querySelectorAll("[data-migration-preference-kind]").length===2;'),seconds=90)
            for kind,value in [('visibility','incoming'),('context','current')]:
                browser.execute('const select=document.querySelector(`[data-migration-preference-kind="${arguments[0]}"]`);select.value=arguments[1];select.dispatchEvent(new Event("input",{bubbles:true}));',kind,value)
                wait_for(lambda:browser.execute('return document.getElementById("modal_status").textContent==="";'))
            wait_for(lambda:browser.execute('return !document.getElementById("modal_submit").disabled;'))
            assert (target/'config.json').read_bytes()==before
            browser.execute('document.getElementById("modal_submit").click();')
            wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
            stored=json.loads((target/'config.json').read_text());account=stored['accounts'][0]
            assert stored['native_hidden_models']==account['hidden_models']==['demo-model']
            assert stored['native_model_context_windows']==account['model_context_windows']=={'demo-model':400000}
            assert native.read_bytes()==native_bytes and upstream.calls==0
            assert browser.execute('return document.querySelectorAll(".service-account").length===1;')
            browser.execute('document.querySelector(`[data-service="account:@native"] [data-ui-action="account-details"]`).click();document.querySelector(`[data-ui-action="account-edit"]`).click();')
            wait_for(lambda:browser.execute('return document.querySelectorAll("[data-preference-kind]").length===0;'))
            browser.execute('document.querySelector(".modal-close").click();')
            checks.append(f'{language}/{theme}: import-page choices, one service preview, Back preserves package, no early writes, one commit, no post-import conflict')
            # Reset only this disposable fixture for the second browser case.
            status,result=request(backend+'/api/accounts/'+account['id'],headers=headers,method='DELETE');assert status==200,result
            status,config=request(backend+'/api/config',headers=headers);assert status==200
            config['native_hidden_models']=[];config['native_model_context_windows']={'demo-model':400000}
            status,result=request(backend+'/api/config',config,headers);assert status==200,result
        before_gallery=(target/'config.json').read_bytes()
        for mode in ['services','service-errors']:
            browser.navigate(frontend+'/?demo='+mode)
            wait_for(lambda:browser.execute('return document.querySelectorAll(".service-card").length>3;'))
            assert browser.execute('return fetch("/api/config",{method:"POST",headers:{"Content-Type":"application/json","X-EMP-Session":localStorage.getItem("emp_management_session_v1")},body:"{}"}).then(response=>response.status);')==403
            assert browser.execute('return Boolean(document.getElementById("shadow_demo_button"));')
            browser.execute('document.getElementById("shadow_demo_button").click();')
            assert browser.execute('return ["import","services","service-errors","review","recovery","lifecycle","removal"].every(mode=>document.querySelector(`[data-shadow-demo="${mode}"]`));')
            browser.execute('document.querySelector(".modal-close").click();document.querySelector("#shadow_services_demo button").click();')
            wait_for(lambda:browser.execute('return !document.getElementById("shadow_services_demo");'))
            assert (target/'config.json').read_bytes()==before_gallery
            checks.append(mode+': shared menu, rejected config write, return to same workspace, no backend mutation')
        args.output.write_text(json.dumps({'passed':True,'checks':checks,'real_model_calls':0,'production_state_used':False,'native_auth_unchanged':True},indent=2)+'\n')
        print(json.dumps({'passed':True,'checks':checks}))


if __name__=='__main__':
    main()
