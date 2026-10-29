"""Production onboarding and encrypted LAN transfer, using synthetic credentials."""
import argparse
import base64
import contextlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
from http.server import ThreadingHTTPServer

from e2e_ultra_runtime import Responses
from e2e_refinements import Browser
from local_backend import request, start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--codex',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-onboarding-') as directory,contextlib.ExitStack() as stack:
        root=Path(directory);a=root/'sender';b=root/'receiver';a.mkdir();b.mkdir()
        bin_dir=root/'bin';bin_dir.mkdir(mode=0o700)
        shutil.copy(Path(__file__).with_name('fixtures')/'device-codex.py',bin_dir/'codex')
        (bin_dir/'codex').chmod(0o700);control=bin_dir/'login-control';control.write_text('pending')
        home=root/'native';home.mkdir()
        config=home/'config.toml'
        original='model="demo/fixture"\napproval_policy="never"\nsandbox_mode="read-only"\nweb_search="disabled"\n[features]\nplugins=false\nmemories=false\nasync_compaction=false\n'
        config.write_text(original)
        upstream=ThreadingHTTPServer(('127.0.0.1',0),Responses)
        stack.callback(upstream.server_close);threading.Thread(target=upstream.serve_forever,daemon=True).start();stack.callback(upstream.shutdown)
        fixture={'auto_enable_on_start':False,'providers':[{'id':'demo','name':'Fixture API',
            'base_url':f'http://127.0.0.1:{upstream.server_port}/v1','protocol':'responses','auth_mode':'api_key','api_key':'fixture-upstream'}],
            'models':[{'id':'demo/fixture','provider':'demo','upstream_id':'fixture','context_window':100000,'enabled':True}]}
        env={'CODEX_HOME':str(home),'PATH':str(bin_dir)+':/usr/bin:/bin',
            'HTTP_PROXY':'http://127.0.0.1:9','HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'localhost,127.0.0.1'}
        base,url,state_dir,headers=start_backend(stack,args.emp.resolve(),a,fixture,env,device_fixture=True)
        other,_,_,other_headers=start_backend(stack,args.emp.resolve(),b,{'auto_enable_on_start':False}, {'PATH':'/usr/bin:/bin',**{k:v for k,v in env.items() if 'PROXY' in k}})
        def call(path,body=None,peer=False,expected=200):
            target,auth=(other,other_headers) if peer else (base,{'X-EMP-Session':json.loads((state_dir/'web-session.json').read_text())['token']})
            status,value=request(target+path,body,auth,timeout=60)
            assert status==expected,(path,status,value)
            return value
        def login(operation):return call('/api/native/login/'+operation,{})
        assert not (home/'auth.json').exists()
        call('/api/integration/enable',{'confirm_reload':True})
        auth=(home/'auth.json').read_bytes();key=json.loads(auth)['OPENAI_API_KEY'];assert key.startswith('emp-local-')
        assert call('/api/config')['native_account']['auth_mode']=='local_key'
        client_env={k:v for k,v in os.environ.items() if not any(word in k.upper() for word in ['TOKEN','KEY','SECRET','AUTH']) and not k.startswith(('CODEX_','OPENAI_','ANTHROPIC_','EASY_MULTI_PROVIDER'))}
        client_env.update(env,HOME=str(home),XDG_CONFIG_HOME=str(home),DBUS_SESSION_BUS_ADDRESS='unix:path=/tmp/emp-fixture-no-keyring')
        result=subprocess.run([str(args.codex.resolve()),'exec','--skip-git-repo-check','Say fixture complete. Do not call tools.'],cwd=home,env=client_env,text=True,capture_output=True,timeout=70)
        assert result.returncode==0 and 'fixture complete' in result.stdout,result.stderr[-3000:]
        assert request(base+'/v1/responses',{'model':'demo/fixture','input':'hello'},{'Authorization':'Bearer wrong'})[0]==401
        checks.append('Installed Codex starts signed out through the managed local key and completes an external Responses turn')
        browser=Browser(stack,root);browser.navigate(url)
        def screenshot(name):
            status,image=request(browser.base+'/screenshot');assert status==200
            (args.output/name).write_bytes(base64.b64decode(image['value']))
        browser.execute('nativeLogin.open();')
        wait_for(lambda:browser.execute('return !!document.getElementById("native_login");'))
        browser.execute('window.fixtureConfirm=window.confirm;window.confirm=()=>true;document.querySelector("[data-native-key-remove]").click();window.confirm=window.fixtureConfirm;')
        wait_for(lambda:not (home/'auth.json').exists())
        assert request(base+'/healthz')[0]==200
        call('/api/integration/enable',{'confirm_reload':True})
        auth=(home/'auth.json').read_bytes()
        browser.execute('state.native_account.auth_mode="local_key";nativeLogin.open();')
        checks.append('Removing the local key restores signed-out Codex while EMP keeps running')
        def pending_login():
            value=login('status')
            assert value['status'] not in ('failed','cancelled'),value
            return value if value['status']=='pending' else None
        login('start');first=wait_for(pending_login,seconds=50)
        assert login('start')['user_code']==first['user_code']
        assert (home/'auth.json').read_bytes()==auth
        browser.execute('document.querySelector("[data-native-login-start]").click();')
        wait_for(lambda:browser.execute('return !!document.querySelector(".device-code");'))
        screenshot('login-pending.png')
        request(browser.base+'/window/rect',{'width':360,'height':840})
        assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
        screenshot('login-narrow.png')
        browser.execute('document.querySelector("[data-login-cancel]").click();')
        wait_for(lambda:login('status')['status']=='cancelled')
        assert (home/'auth.json').read_bytes()==auth
        browser.execute('closeModal();')
        login('start');wait_for(lambda:login('status')['status']=='pending');control.write_text('complete')
        completed=wait_for(lambda:(v if (v:=login('status'))['status'] in ('completed','failed') else None))
        assert completed['status']=='completed',completed
        saved=(home/'auth.json').read_bytes();assert json.loads(saved)['tokens']['access_token']=='fixture-access'
        call('/api/integration/restore',{'confirm_reload':True})
        assert (home/'auth.json').read_bytes()==saved
        checks.append('Device code, duplicate start, cancellation, successful replacement and restore preserve the correct authentication')
        control.write_text('pending');login('start');wait_for(lambda:login('status')['status']=='pending')
        changed=b'{"OPENAI_API_KEY":"changed-by-another-login"}';(home/'auth.json').write_bytes(changed)
        control.write_text('complete');failure=wait_for(lambda:(v if (v:=login('status'))['status']=='failed' else None))
        assert failure['code']=='login_auth_changed' and (home/'auth.json').read_bytes()==changed
        checks.append('A concurrent login is preserved instead of overwritten')
        # Add-service login writes the account vault, never the Native auth file.
        control.write_text('pending')
        assert request(base+'/api/accounts/login/start',{'name':'Fixture'})[0]==401
        call('/api/accounts/login/start',{},expected=409)
        browser.execute('openServiceChooser();document.querySelector("[data-ui-action=account-import-device]").click();document.getElementById("device_account_name").value="Device fixture";')
        wait_for(lambda:browser.execute('return !document.querySelector("[data-native-login-start]").disabled;'))
        browser.execute('document.querySelector("[data-native-login-start]").click();')
        wait_for(lambda:browser.execute('return !!document.querySelector(".device-code");'))
        wait_for(lambda:call('/api/accounts/login/status',{})['status']=='pending')
        login('cancel')
        assert call('/api/accounts/login/status',{})['status']=='pending'
        assert (home/'auth.json').read_bytes()==changed
        screenshot('subscription-device-pending.png')
        browser.execute('document.querySelector("[data-login-cancel]").click();')
        wait_for(lambda:call('/api/accounts/login/status',{})['status']=='cancelled')
        wait_for(lambda:browser.execute('return !document.querySelector("[data-native-login-start]").disabled;'))
        assert not call('/api/config')['accounts']
        browser.execute('document.querySelector("[data-native-login-start]").click();')
        wait_for(lambda:call('/api/accounts/login/status',{})['status']=='pending')
        control.write_text('complete')
        wait_for(lambda:len(call('/api/config')['accounts'])==1,seconds=30)
        wait_for(lambda:browser.execute('return state.accounts.length===1;'))
        assert not call('/api/accounts/login/status',{})['restart_required']
        assert (home/'auth.json').read_bytes()==changed
        checks.append('Add-service device login cancels independently, adds one subscription and preserves Native authentication byte-for-byte')
        # File selection and pasted JSON use the same form and account transaction.
        import_auth={'tokens':{'account_id':'file-fixture','access_token':'synthetic-file-token','id_token':'synthetic-id'}}
        browser.execute('closeModal();openServiceChooser();document.querySelector("[data-ui-action=account-import-auth]").click();document.getElementById("modal_account_name").value="File fixture";const input=document.getElementById("a_auth_json"),transfer=new DataTransfer();transfer.items.add(new File([JSON.stringify(arguments[0])],"auth.json",{type:"application/json"}));input.files=transfer.files;input.dispatchEvent(new Event("change",{bubbles:true}));',import_auth)
        wait_for(lambda:browser.execute('return !!document.getElementById("modal_account_auth").value;'))
        assert browser.execute('return document.getElementById("modal_account_name").value;')=='File fixture'
        screenshot('subscription-auth-file.png')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda:len(call('/api/config')['accounts'])==2)
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        browser.execute('openServiceChooser();document.querySelector("[data-ui-action=account-import-auth]").click();document.getElementById("modal_account_name").value="Pasted fixture";document.getElementById("modal_account_auth").value=JSON.stringify(arguments[0]);document.getElementById("modal_submit").click();',import_auth)
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        assert len(call('/api/config')['accounts'])==2
        assert (home/'auth.json').read_bytes()==changed
        checks.append('auth.json file and paste share one form, preserve the entered name and deduplicate the same subscription')
        selected=[{'kind':'provider','id':'demo','model_ids':['demo/fixture']}]
        for requested in [1,15,None]:
            authorization=call('/api/migration/export/confirm',{})
            payload={'confirmation':authorization['confirmation'],'selections':selected}
            if requested is not None:payload['minutes']=requested
            transfer=call('/api/migration/lan/start',payload)
            assert 298 <= transfer['remaining_seconds'] <= 300, transfer
            if requested is not None:call('/api/migration/lan/cancel',{})
        checks.append('Migration codes always last five minutes, including requests from older clients with a different duration')
        code=transfer['code'];assert len(code)==49
        receive={'code':code,'receiver':'01'*32}
        assert call('/api/migration/lan/receive',receive,peer=True)['status']=='waiting'
        assert call('/api/migration/lan/status',{})['status']=='confirmation'
        assert not call('/api/config',peer=True)['providers']
        call('/api/migration/lan/approve',{'code':code})
        package=call('/api/migration/lan/receive',receive,peer=True)
        assert 'fixture-upstream' not in json.dumps(package)
        assert not call('/api/config',peer=True)['providers']
        selection=[{'kind':'provider','id':p['id']} for p in package['payload']['config']['providers']]
        preview=call('/api/migration/preview',{'token':package['token'],'selections':selection},peer=True)
        assert preview['config']['providers']
        call('/api/migration/import',{'token':package['token'],'selections':selection},peer=True)
        assert call('/api/config',peer=True)['providers']
        call('/api/migration/lan/receive',receive,peer=True,expected=400)
        checks.append('Two isolated EMP instances transfer once, require sender approval, redact secrets, preview first and import only after confirmation')
        for language in ['zh-CN','en']:
            browser.execute('setLanguage(arguments[0]);migration.open();document.querySelector("[data-migration-mode=tunnel]").click();',language)
            wait_for(lambda:browser.execute('return !!document.getElementById("lan_code");'))
            assert browser.execute('return document.documentElement.scrollWidth<=innerWidth;')
            browser.execute('for(const box of document.querySelectorAll("[data-migration-select]")){box.checked=box.closest("label").textContent.includes("Fixture API");box.dispatchEvent(new Event("input",{bubbles:true}));}')
            screenshot(f'lan-{language}.png')
            browser.execute('document.querySelector("[data-lan-create]").click();')
            try:
                wait_for(lambda:browser.execute('return !document.getElementById("lan_share_block").hidden && document.getElementById("lan_share_code").value.length===49;'),seconds=30)
            except TimeoutError:
                raise AssertionError(browser.execute('return document.getElementById("lan_send_status").textContent;')) from None
            assert 295 <= call('/api/migration/lan/status',{})['remaining_seconds'] <= 300
            browser.execute('document.querySelector("[data-lan-cancel]").click();')
            wait_for(lambda:browser.execute('return document.getElementById("lan_share_block").hidden;'))
        checks.append('Bilingual production migration controls and sign-in fit a narrow viewport')
    report={'real_model_calls':0,'checks':checks}
    (args.output/'result.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))

if __name__=='__main__':main()
