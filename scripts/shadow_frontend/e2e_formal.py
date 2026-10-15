"""Promoted frontend acceptance against writable temporary EMP backends and fake models."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile
from e2e_refinements import Browser
from e2e_environment import request, start_backend, wait_for


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-formal-ui-') as temp,contextlib.ExitStack() as stack:
        root=Path(temp);source=root/'source';target=root/'target';source.mkdir();target.mkdir()
        catalog=source/'fixture-models.json'
        catalog.write_text(json.dumps({'models':[{'slug':'gpt-fixture','display_name':'Fixture model','context_window':272000,'max_context_window':872000,'visibility':'list','base_instructions':'fixture'},
            {'slug':'unrelated','display_name':'Unrelated','context_window':1000,'max_context_window':1000,'visibility':'list','base_instructions':'fixture'}]}))
        fixture_accounts=[{'id':'sync-one','prefix':'sync-one','name':'One','auth_file':'','hidden_models':['unrelated'],'model_context_windows':{'unrelated':123}},
            {'id':'sync-two','prefix':'sync-two','name':'Two','auth_file':'','hidden_models':[]}]
        base,url,_,headers,upstream=start_backend(stack,args.emp.resolve(),source,{'native_catalog_path':str(catalog),'accounts':fixture_accounts})
        target_base,target_url,_,target_headers,_=start_backend(stack,args.emp.resolve(),target)
        headers['Origin']=base;target_headers['Origin']=target_base
        upstream.failed=False;upstream.calls=0
        browser=Browser(stack,root,system_access=True);browser.navigate(url)
        wait_for(lambda:browser.execute('return !!state?.providers?.length;'))
        headers['X-EMP-Session']=json.loads((source/'state/web-session.json').read_text())['token']
        assert browser.execute('return !window.EMP_PREVIEW && !!document.getElementById("shadow_migration_open") && !document.querySelector("[data-i18n=export_emp]");')
        assert browser.execute('return !window.EMP_REVIEW_CHOICES && !window.EMP_REVIEW_ACTION && !document.getElementById("demo_account_import");')
        for path in ['/recovery', '/assets/preview-review.js', '/assets/preview-recovery.js', '/api/recovery']:
            assert request(base + path, headers=headers)[0] == 404, path
        checks.append('Production serves real product assets only; review adapters, recovery demos and mock actions are unavailable')
        browser.execute('managementEvents.stop();clearInterval(quotaStateTimer);quotaStateTimer=null;')
        for language in ['zh-CN','en']:
            for theme in ['light','dark']:
                browser.execute('setLanguage(arguments[0]);setTheme(arguments[1]);openAccountDetails(state.native_account.id);',language,theme)
                assert browser.execute('return !!document.querySelector(".shadow-compact-account") && !!document.querySelector(".shadow-reset-action svg rect");')
                browser.execute('document.querySelector("#service_detail_nav [data-ui-action=account-edit]").click();')
                assert browser.execute('return document.querySelector("#service_detail_nav [data-ui-action=account-edit]").getAttribute("aria-selected")==="true";')
                browser.execute('closeModal();openServiceChooser();')
                assert browser.execute('return document.getElementById("modal_back").hidden;')
                browser.execute('document.querySelector("[data-ui-action=service-custom-api]").click();')
                assert browser.execute('const controls=[document.getElementById("modal_back"),document.querySelector(".modal-close")];return controls.every(n=>getComputedStyle(n).borderTopWidth==="0px");')
                browser.execute('document.getElementById("modal_back").click();closeModal();')
        checks.append('Formal service/account/model presentation, physical Reset and borderless navigation in EN/ZH and light/dark')
        browser.execute('setLanguage("zh-CN");setTheme("light");editProvider("demo");')
        assert browser.execute('const key=document.getElementById("modal_provider_key");return key.type==="text" && key.placeholder==="******";')
        before=(source/'config.json').read_bytes()
        browser.execute('const key=document.getElementById("modal_provider_key");key.focus();key.value="unsaved-fixture-replacement";closeModal();')
        assert (source/'config.json').read_bytes()==before
        browser.execute('editProvider("demo");')
        assert browser.execute('return document.getElementById("modal_provider_key").placeholder==="******";')
        browser.execute('document.getElementById("modal_provider_name").value="Formal Alias";document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        assert request(base+'/api/config',headers=headers)[1]['providers'][0]['name']=='Formal Alias'
        checks.append('Formal API-key cancellation preserves saved credentials; name Save reaches the real temporary config')
        browser.execute('providerModelSettings.open("demo");')
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-manager-action=test]");'))
        browser.execute('document.querySelector("[data-manager-action=test]").click();')
        wait_for(lambda:browser.execute('return document.getElementById("status").textContent.includes("测试成功");'))
        assert upstream.calls==1
        browser.execute('closeModal();')
        checks.append('Promoted model Test makes exactly one fake inference; preview-only guards are absent')
        # Exercise subscription Save against the real temporary configuration owner.
        current=request(base+'/api/config',headers=headers)[1]
        browser.execute('load();')
        wait_for(lambda:browser.execute('return state.accounts.some(a=>a.id==="sync-two");'))
        browser.execute(r'''window.fixtureAccountFetch=window.fetch;const models=[{id:'gpt-fixture',display_name:'Fixture model',default_context_window:272000,max_context_window:872000}];
          state.subscription_models=models;window.fetch=function(path,options){if(/\/api\/accounts\/.*\/models$/.test(String(path)))return Promise.resolve(new Response(JSON.stringify({models}),{status:200,headers:{'Content-Type':'application/json'}}));return window.fixtureAccountFetch(path,options);};
          editAccount(state.native_account.id);''')
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-shadow-subscription-sync]");'))
        before=(source/'config.json').read_bytes()
        browser.execute('document.querySelector("input[name=subscription_context]").value="467368";document.querySelector("[data-shadow-subscription-sync]").click();closeModal();')
        assert (source/'config.json').read_bytes()==before
        browser.execute('editAccount(state.native_account.id);')
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-shadow-subscription-sync]");'))
        browser.execute('document.querySelector("input[name=subscription_context]").value="467368";document.querySelector("[data-shadow-subscription-sync]").click();document.querySelector("[data-shadow-subscription-visibility]").click();document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        saved=request(base+'/api/config',headers=headers)[1]
        assert saved['native_model_context_windows']['gpt-fixture']==467368
        assert 'gpt-fixture' in saved['native_hidden_models']
        assert all(a['model_context_windows']['gpt-fixture']==467368 and 'gpt-fixture' in a['hidden_models'] for a in saved['accounts'])
        assert saved['accounts'][0]['model_context_windows']['unrelated']==123
        assert saved['providers']==current['providers'] and saved['models']==current['models']
        browser.execute('window.fetch=window.fixtureAccountFetch;delete window.fixtureAccountFetch;')
        checks.append('Formal subscription cancel preserves bytes; Save commits marked visibility/context to both peers and preserves unrelated settings')
        # Encrypted selected export is issued by the actual backend, not browser credentials.
        selection=[{'kind':'provider','id':'demo','name':'Formal Alias','models':[{'id':'demo/model','selected':True}]}]
        status,confirmation=request(base+'/api/migration/export/confirm',{},headers);assert status==200
        status,exported=request(base+'/api/migration/export',{'password':'formal-fixture-password','confirmation':confirmation['confirmation'],'selections':selection},headers);assert status==200,exported
        bundle=exported['bundle'];assert base64.b64decode(bundle).startswith(b'EMP-MIGRATION\x01\n')
        browser.navigate(target_url)
        wait_for(lambda:browser.execute('return !!state?.providers?.length;'))
        target_headers['X-EMP-Session']=json.loads((target/'state/web-session.json').read_text())['token']
        browser.execute('managementEvents.stop();clearInterval(quotaStateTimer);quotaStateTimer=null;migration.open();')
        target_before=(target/'config.json').read_bytes()
        browser.execute('''
          const input=document.getElementById('shadow_migration_file');const data=Uint8Array.from(atob(arguments[0]),c=>c.charCodeAt(0));
          const transfer=new DataTransfer();transfer.items.add(new File([data],'fixture.emp',{type:'application/octet-stream'}));input.files=transfer.files;
          document.getElementById('shadow_import_password').value='formal-fixture-password';document.querySelector('[data-migration-decode]').click();
        ''',bundle)
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-migration-name]") && !document.getElementById("modal_submit").disabled;'),seconds=90)
        assert (target/'config.json').read_bytes()==target_before
        assert browser.execute('return !document.getElementById("modal_body").textContent.includes("fixture-only");')
        browser.execute('const field=document.querySelector("[data-migration-name]");field.value="Imported Alias";field.dispatchEvent(new Event("input",{bubbles:true}));closeModal();')
        assert (target/'config.json').read_bytes()==target_before
        checks.append('Actual encrypted package opens without writes or credential exposure; cancel discards inline names')
        browser.execute('migration.open();const input=document.getElementById("shadow_migration_file"),transfer=new DataTransfer();transfer.items.add(new File([Uint8Array.from(atob(arguments[0]),c=>c.charCodeAt(0))],"fixture.emp"));input.files=transfer.files;document.getElementById("shadow_import_password").value="formal-fixture-password";document.querySelector("[data-migration-decode]").click();',bundle)
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-migration-name]") && !document.getElementById("modal_submit").disabled;'),seconds=90)
        browser.execute('const field=document.querySelector("[data-migration-name]");field.value="Imported Alias";field.dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'),seconds=60)
        stored=request(target_base+'/api/config',headers=target_headers)[1]
        imported=next(p for p in stored['providers'] if p['name']=='Imported Alias')
        assert imported['id']!='demo' and len(stored['providers'])==2
        assert any(m['provider']==imported['id'] for m in stored['models'])
        assert stored['providers'][0]['name']=='Demo'
        checks.append('One Import saves the inline name and verified independent connection; existing service and model routes remain')
        for theme in ['light','dark']:
            browser.execute('setTheme(arguments[0]);migration.open();document.querySelector("[data-migration-demo]").click();',theme)
            wait_for(lambda:browser.execute('return !!document.querySelector("[data-migration-name]") && !document.getElementById("modal_submit").disabled;'))
            before=(target/'config.json').read_bytes()
            browser.execute('document.getElementById("modal_submit").click();')
            assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");')
            assert (target/'config.json').read_bytes()==before
        checks.append('Formal import demo remains read-only in both themes')
        # Verify a selected imported service can be encrypted and re-imported via backend endpoints.
        status,confirmation=request(target_base+'/api/migration/export/confirm',{},target_headers);assert status==200
        status,reexported=request(target_base+'/api/migration/export',{'password':'formal-fixture-password','confirmation':confirmation['confirmation'],'selections':[{'kind':'provider','id':imported['id'],'name':'Imported Alias','models':[{'id':m['id'],'selected':True} for m in stored['models'] if m['provider']==imported['id']]}]},target_headers);assert status==200,reexported
        status,decoded=request(target_base+'/api/migration/decode',{'password':'formal-fixture-password','bundle':reexported['bundle']},target_headers);assert status==200,decoded
        assert len(decoded['payload']['config']['providers'])==1 and decoded['payload']['config']['providers'][0]['name']=='Imported Alias'
        assert 'provider_keys' not in decoded['payload']
        checks.append('Selected encrypted re-export retains edited names and excludes unselected services and plaintext credentials')
        args.output.write_text(json.dumps({'passed':True,'checks':checks,'formal_backend_saves':True,'fake_model_requests':upstream.calls,'real_model_requests':0},indent=2)+'\n')
        print(json.dumps({'passed':True,'checks':checks}))


if __name__=='__main__':main()
