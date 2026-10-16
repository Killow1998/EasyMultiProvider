"""Shadow account/context/migration/geometry acceptance with synthetic credentials."""
import argparse
import base64
import contextlib
import json
import os
import subprocess
import sys
import tempfile
import time
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from e2e import Browser as BaseBrowser
from e2e_environment import request, start_backend, stop, wait_for

HERE = Path(__file__).resolve().parent


class ReleaseFixture(BaseHTTPRequestHandler):
    calls = 0
    notes = "### 中文\n- 预览与迁移修复\n<script>window.badReleaseScript=true</script>\n### English\n- Preview and migration corrections\n<script>window.badReleaseScript=true</script>"

    def log_message(self, *args):
        pass

    def do_GET(self):
        type(self).calls += 1
        body = json.dumps({'tag_name':'v9.0.0','draft':False,'prerelease':False,'body':self.notes}).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)



class Browser(BaseBrowser):
    def execute(self, script, *args):
        # Firefox's WebDriver sandbox cannot resolve page-global lexical bindings.
        return super().execute("""
          const node=document.createElement('script');
          node.textContent='try{window.shadowTestResult=(function(){'+arguments[0]+'}).apply(null,'+JSON.stringify(arguments[1])+')}catch(error){window.shadowTestResult={shadowTestError:error.stack}}';
          document.head.append(node);node.remove();
          const result=window.shadowTestResult;delete window.shadowTestResult;
          if(result?.shadowTestError)throw new Error(result.shadowTestError);
          return result;
        """,script,list(args))


def codec(helper, payload):
    result = subprocess.run([str(helper)], input=json.dumps(payload).encode(),
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=90)
    assert result.returncode == 0
    value = json.loads(result.stdout)
    assert 'error' not in value, value
    return value


def async_api(browser, path, body):
    browser.execute('window.shadowAsyncResult=null;api(arguments[0],{method:"POST",body:JSON.stringify(arguments[1])}).then(value=>window.shadowAsyncResult=value,error=>window.shadowAsyncResult={error:error.message});',path,body)
    result=wait_for(lambda:browser.execute('return window.shadowAsyncResult;'),seconds=90)
    assert 'error' not in result,result
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--helper', type=Path, required=True)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--visual-only', action='store_true')
    args = parser.parse_args()
    checks, geometry = [], []
    with tempfile.TemporaryDirectory(prefix='emp-refinements-') as temp, contextlib.ExitStack() as stack:
        root=Path(temp)
        base, _, state_dir, _, upstream=start_backend(stack,args.emp.resolve(),root)
        stored_before=(state_dir.parent/'config.json').read_bytes()
        release=ThreadingHTTPServer(('127.0.0.1',0),ReleaseFixture)
        stack.callback(release.server_close)
        threading.Thread(target=release.serve_forever,daemon=True).start()
        stack.callback(release.shutdown)
        log_path=root/'shadow.log';log=stack.enter_context(log_path.open('w'))
        env=dict(os.environ);env['HOME']=str(root/'shadow-home');env['CODEX_HOME']=str(root/'codex_home')
        Path(env['HOME']).mkdir()
        process=subprocess.Popen([sys.executable,str(HERE/'shadow-web.py'),'--port','0',
            '--backend-port',base.rsplit(':',1)[1],'--state-dir',str(state_dir),
            '--migration-helper',str(args.helper.resolve()),'--release-api',f'http://127.0.0.1:{release.server_port}/latest'],env=env,stdout=log,stderr=log)
        stack.callback(stop,process)
        shadow=wait_for(lambda:log_path.read_text().split('EMP shadow: ')[-1].strip() if 'EMP shadow:' in log_path.read_text() else None)
        browser=Browser(stack,root,reduced_motion=0,system_access=True)
        request(browser.base+'/timeouts',{'script':120000})
        browser.navigate(shadow)
        assert browser.execute('return !!document.getElementById("shadow_migration_open") && !document.querySelector("[data-i18n=export_emp]");')
        checks.append('One Data migration entry')
        browser.execute('openUpdate();')
        wait_for(lambda:browser.execute('return document.getElementById("update_notes")?.textContent.includes("预览与迁移修复");'))
        assert browser.execute('return !window.badReleaseScript && document.getElementById("update_notes").textContent.includes("<script>") && document.getElementById("update_install").disabled;')
        browser.execute('setLanguage("en");')
        assert browser.execute('return document.getElementById("update_notes").textContent.includes("Preview and migration corrections") && !document.getElementById("update_notes").textContent.includes("预览与迁移修复");')
        browser.execute('setLanguage("zh");')
        assert browser.execute('return !document.getElementById("update_notes").textContent.includes("Preview and migration corrections") && localizedReleaseNotes(legacyEnglish,"0.12.18","zh").includes("改进 Web 界面") && !/shadow|影子|开发预览|developer preview/i.test(localizedReleaseNotes(legacyEnglish,"0.12.18",language)) && localizedReleaseNotes("Unknown English release","9.1","zh")==="";')
        browser.execute('closeModal();openUpdate();')
        wait_for(lambda:browser.execute('return document.getElementById("update_notes")?.textContent.includes("预览与迁移修复");'))
        assert ReleaseFixture.calls==1
        browser.execute('closeModal();')
        checks.append('Bilingual release notes switch with locale; maintained 0.12.18 Chinese fallback; unknown translations remain explicit')
        checks.append('Release notes shown safely before update; repeat checks reuse metadata and never install')

        # Native context and lifetime summary reads are fixture-only browser data.
        browser.execute(r'''
          const context=[{id:'gpt-6-luna',display_name:'GPT-6 Luna',default_context_window:272000,max_context_window:872000,effective_context_window_percent:95}];
          state.subscription_models=context;window.fixtureSubscriptionModels=context;
          state.native_model_context_windows={'gpt-6-luna':872000};state.native_account.model_context_windows={'gpt-6-luna':872000};
          const fetchBefore=window.fetch;window.fetch=async function(url,options){
            if(String(url).includes('/api/accounts/')&&(/\/models(?:\/refresh)?$/).test(String(url)))return new Response(JSON.stringify({models:window.fixtureSubscriptionModels}),{status:200,headers:{'Content-Type':'application/json'}});
            if(String(url).startsWith('/api/usage?account_id='))return new Response(JSON.stringify({first_record_at:1700000000,groups:[{model:'gpt-6-luna',requests:10,priced_requests:8,input_reports:10,output_reports:10,input_tokens:1790000000,output_tokens:508204,cost_nanos:1484881700000}]}),{status:200,headers:{'Content-Type':'application/json'}});
            return fetchBefore(url,options);
          }; openAccountDetails(state.native_account.id);
        ''')
        wait_for(lambda:browser.execute('return document.querySelector("#account_usage strong")?.textContent.includes("B");'))
        assert browser.execute('return document.querySelector(".shadow-compact-account").children.length===3 && document.querySelector(".shadow-compact-account").textContent.includes("本机登录");')
        assert browser.execute('return document.querySelector(".shadow-account-footnotes").compareDocumentPosition(document.querySelector("#account_usage .usage-table"))&Node.DOCUMENT_POSITION_PRECEDING;')
        assert browser.execute('return !!document.querySelector("#service_detail_nav .shadow-reset-action");')
        browser.hover('#account_usage strong')
        wait_for(lambda:browser.execute('return !document.querySelector("#shadow_feedback").hidden;'))
        assert browser.execute('return document.querySelector("#shadow_feedback").textContent.includes("1,790,508,204");')
        checks.append('Compact account, footer, exact number tooltip and visible Reset')
        browser.execute("state.accounts.push({id:'alignment',prefix:'alignment',name:'Alias',credential_set:true,quota:{account_label:'a***@fixture.invalid',plan_type:'pro',credits:{reset_credits:{available_count:2}}}});openAccountDetails('alignment');")
        wait_for(lambda:browser.execute('return !!document.querySelector("#account_usage .shadow-usage-summary");'))
        assert browser.execute("const grid=document.querySelector('.shadow-compact-account');const values=[...grid.children].map(cell=>cell.lastElementChild.getBoundingClientRect());const center=r=>r.top+r.height/2;return Math.max(...values.map(center))-Math.min(...values.map(center))<1"),'Account values are not vertically aligned'
        assert browser.execute("const grid=document.querySelector('.shadow-compact-account'),nav=document.getElementById('service_detail_nav'),button=nav.querySelector('[data-ui-action=account-remove]'),widths=[...grid.children].map(node=>node.getBoundingClientRect().width),tabs=nav.querySelector('.service-detail-tabs');return !grid.contains(button)&&Math.max(...widths)-Math.min(...widths)<1&&Math.abs(button.getBoundingClientRect().right-nav.getBoundingClientRect().right)<1&&Math.abs(button.getBoundingClientRect().top-tabs.firstElementChild.getBoundingClientRect().top)<1"),'Account removal did not stay on the tab row or fields stopped sharing equal widths'
        assert browser.execute("const nav=document.getElementById('service_detail_nav'),reset=nav.querySelector('.shadow-reset-action');return reset&&Math.abs(reset.getBoundingClientRect().top-nav.querySelector('[role=tab]').getBoundingClientRect().top)<1"),'Reset left its tab row'

        assert browser.execute("const row=document.querySelector('.shadow-usage-summary'),r=row.getBoundingClientRect();return [...row.querySelectorAll('h3,.usage-total')].every(n=>{const b=n.getBoundingClientRect();return Math.abs(b.top+b.height/2-r.top-r.height/2)<2})"),'Lifetime totals left their heading row'
        if args.output:
            for theme in ['light','dark']:
                browser.execute('setTheme(arguments[0]);',theme)
                status,png=request(browser.base+'/screenshot');assert status==200
                args.output.with_name('shadow-cleanup-account-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('setTheme("light");')
        browser.execute('openAccountDetails(state.native_account.id);')
        checks.append('Imported alias/sign-in alignment, Reset tab row and inline lifetime totals')

        browser.execute('document.querySelector("#service_detail_nav [data-ui-action=account-edit]").click();')
        wait_for(lambda:browser.execute('return !!document.querySelector("#shadow_usable_0");'))
        assert browser.execute('return document.querySelector("[name=subscription_context]").value==="872000" && document.querySelector("#shadow_usable_0").textContent.includes("828.4K");')
        browser.hover('.shadow-context-control .shadow-help')
        wait_for(lambda:browser.execute('return document.getElementById("shadow_feedback").textContent.includes("95%");'))
        assert browser.execute('return document.getElementById("shadow_feedback").textContent.includes("828,400");')
        browser.execute('fixtureSubscriptionModels[0].effective_context_window_percent=90;document.getElementById("subscription_model_refresh").click();')
        wait_for(lambda:browser.execute('return document.querySelector("#shadow_usable_0").textContent.includes("784.8K");'))
        assert browser.execute('return document.querySelector("#shadow_usable_0").textContent.includes("784.8K");')
        browser.execute('fixtureSubscriptionModels=Array.from({length:12},(_,i)=>({...fixtureSubscriptionModels[0],id:"scroll-"+i}));document.getElementById("subscription_model_refresh").click();')
        wait_for(lambda:browser.execute('return document.querySelectorAll("[name=subscription_context]").length===12;'))
        request(browser.base+'/window/rect',{'width':680,'height':550})
        for zoom in [1,1.5,1.7]:
            request(browser.base+'/moz/context',{'context':'chrome'})
            request(browser.base+'/execute/sync',{'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=arguments[0];','args':[zoom]})
            request(browser.base+'/moz/context',{'context':'content'})
            for theme in ['light','dark']:
                browser.execute('setTheme(arguments[0]);',theme)
                scroll=browser.execute('const list=document.getElementById("subscription_model_list"),modal=list.closest(".modal"),body=document.getElementById("modal_body");const owners=[modal,...modal.querySelectorAll("*")].filter(node=>/auto|scroll/.test(getComputedStyle(node).overflowY)&&node.scrollHeight>node.clientHeight+1);return {inner:getComputedStyle(list).overflowY,owners:owners.map(node=>node.id),save:document.getElementById("modal_submit").getBoundingClientRect().bottom<=innerHeight};')
                assert scroll=={'inner':'visible','owners':['modal_body'],'save':True},scroll
        request(browser.base+'/moz/context',{'context':'chrome'})
        request(browser.base+'/execute/sync',{'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=1;','args':[]})
        request(browser.base+'/moz/context',{'context':'content'})
        request(browser.base+'/window/rect',{'width':1280,'height':1000})
        checks.append('Model settings have one scroll owner and a reachable Save button in a short viewport')
        browser.execute('closeModal();toast("Saved","Ready");')
        assert browser.execute('return !document.querySelector(".toast") && !document.querySelector("#notification").hidden;')
        checks.append('Catalog-derived context ratio and one notification owner')

        # Exercise the same narrow-container transition as 150%/170% desktop zoom.
        browser.navigate(shadow+'?demo=services')
        for language in ['zh-CN','en']:
            for theme in ['light','dark']:
                browser.execute('setLanguage(arguments[0]);setTheme(arguments[1]);',language,theme)
                for width in [1040,1280,1540]:
                    request(browser.base+'/window/rect',{'width':width,'height':1050})
                    measure=browser.execute('''return [...document.querySelectorAll('#services .service-card:has(.presentation-quota-pair)')].map(row=>{const card=row.getBoundingClientRect(),pair=row.querySelector('.presentation-quota-pair').getBoundingClientRect();return {width:card.width,error:Math.abs((card.left+card.right-pair.left-pair.right)/2)}});''')
                    assert measure and all(m['error']<1.1 for m in measure),measure
                    geometry.append({'language':language,'theme':theme,'window':width,'rows':measure})
                halos=browser.execute('''return [...document.querySelectorAll('#services .presentation-service-icon[data-activity-state=active]')].map(icon=>({brand:icon.dataset.brand,border:getComputedStyle(icon.querySelector('[data-presentation-motion=halo]')).borderTopWidth,shadow:getComputedStyle(icon.querySelector('[data-presentation-motion=halo]')).boxShadow}));''')
                assert halos and all(float(h['border'].removesuffix('px'))>0 and h['shadow']!='none' for h in halos),halos
                if language=='zh-CN':
                    sample=lambda:browser.execute('return getComputedStyle(document.querySelector("[data-presentation-motion=halo]")).opacity;')
                    first=sample();time.sleep(.35);assert sample()!=first,'Activity halo is static with normal motion'
                    if args.output:
                        status,png=request(browser.base+'/screenshot');assert status==200
                        args.output.with_name('shadow-refinements-'+theme+'.png').write_bytes(base64.b64decode(png['value']))

        request(browser.base+'/window/rect',{'width':1920,'height':1050})
        original_scale=browser.execute('return devicePixelRatio;')
        for zoom in [1.5,1.7]:
            request(browser.base+'/moz/context',{'context':'chrome'})
            status,result=request(browser.base+'/execute/sync',{'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=arguments[0];return gBrowser.selectedBrowser.browsingContext.fullZoom;','args':[zoom]})
            assert status==200 and abs(result['value']-zoom)<.01,result
            request(browser.base+'/moz/context',{'context':'content'});time.sleep(.2)
            actual=browser.execute('return devicePixelRatio;')/original_scale
            centered=browser.execute("return [...document.querySelectorAll('#services .service-card:has(.presentation-quota-pair)')].every(row=>{const r=row.getBoundingClientRect(),p=row.querySelector('.presentation-quota-pair').getBoundingClientRect();return Math.abs(r.left+r.right-p.left-p.right)<2})")
            assert centered
            geometry.append({'browser_full_zoom':zoom,'device_pixel_ratio_multiplier':actual,'centered':True})
        request(browser.base+'/moz/context',{'context':'chrome'})
        request(browser.base+'/execute/sync',{'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=1;','args':[]})
        request(browser.base+'/moz/context',{'context':'content'})
        browser.execute('openSettings();document.querySelector("[data-presentation-style=bar]").click();closeModal();')
        assert browser.execute("return [...document.querySelectorAll('#services .service-card:has(.quota-stack)')].every(row=>{const r=row.getBoundingClientRect(),q=row.querySelector('.service-quota').getBoundingClientRect();return q.width>r.width-30})"),'Bar switch shrank quota content'
        browser.execute('openSettings();document.querySelector("[data-presentation-style=ring]").click();closeModal();')
        checks.append('Bar switch preserves full-width quota layout')
        if args.visual_only:
            result={'passed':True,'geometry':geometry,'normal_motion_verified':True,'checks':checks,'synthetic_model_calls':0}
            if args.output:args.output.write_text(json.dumps(result,indent=2)+'\n')
            print(json.dumps({'passed':True,'geometry_cases':len(geometry),'normal_motion':True}));return
        browser.navigate(shadow);browser.execute('setLanguage("zh-CN");setTheme("light");')
        checks.append('Centered rings across breakpoint and light/dark activity halos for all brands')

        package={'schema':'easy-multi-provider-migration','version':1,'config':{
            'providers':[{'id':'imported','name':'Imported API','base_url':'https://fixture.invalid/v1','protocol':'responses','auth_mode':'api_key'}],
            'models':[{'id':'imported/model','provider':'imported','upstream_id':'model','display_name':'Original model','context_window':64000,'enabled':True}]},
            'accounts':[{'metadata':{'id':'import-account','name':'Imported account','prefix':'import-account','enabled':True,'hidden_models':[],'model_context_windows':{'gpt-6-luna':872000}},'auth':{'tokens':{'access_token':'fixture-import-token','account_id':'fixture-import-account'}}}],
            'provider_keys':{'imported':'fixture-import-key'}}
        package['config']['accounts']=[package['accounts'][0]['metadata']]
        password='fixture-preview-password'
        encoded=codec(args.helper,{'operation':'encode','password':password,'payload':package})['bundle']
        browser.execute('state.providers[0].name="Earlier draft";shadowSave(state);');
        browser.execute('document.getElementById("shadow_migration_open").click();')
        browser.execute('''const bytes=Uint8Array.from(atob(arguments[0]),c=>c.charCodeAt(0));const data=new DataTransfer();data.items.add(new File([bytes],'fixture.emp'));document.getElementById('shadow_migration_file').files=data.files;document.getElementById('shadow_import_password').value=arguments[1];document.querySelector('[data-migration-decode]').click();''',encoded,password)
        wait_for(lambda:browser.execute('return document.querySelectorAll(".migration-service").length===2 && !document.getElementById("modal_submit").disabled;'),seconds=90)
        assert browser.execute('return !document.querySelector(".migration-service [data-migration-name],[data-migration-model-name],[data-migration-model-context]") && !document.querySelector(".migration-service details");')
        assert browser.execute('return [...document.querySelectorAll(".migration-preview-models")].every(node=>!node.open);')
        assert browser.execute('return document.querySelectorAll("#migration_preview_rows [data-migration-name]").length===2 && !state.providers.some(p=>p.id==="imported");')
        browser.execute('const name=[...document.querySelectorAll("[data-migration-name]")].find(input=>input.dataset.migrationName==="provider:imported");name.focus();name.value="Imported API renamed";name.dispatchEvent(new Event("input",{bubbles:true}));')
        assert browser.execute('return document.activeElement===[...document.querySelectorAll("[data-migration-name]")].find(input=>input.dataset.migrationName==="provider:imported");'),'Typing reset name-field focus'
        browser.execute('const name=[...document.querySelectorAll("[data-migration-name]")].find(input=>input.dataset.migrationName==="account:import-account");name.value="";name.dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("migration_include_context").click();document.getElementById("migration_include_rename").click();')
        wait_for(lambda:browser.execute('return !document.getElementById("modal_submit").disabled;'))
        assert browser.execute('return [...document.querySelectorAll("[data-migration-name]")].find(input=>input.dataset.migrationName==="provider:imported").value==="Imported API renamed" && [...document.querySelectorAll("[data-migration-name]")].find(input=>input.dataset.migrationName==="account:import-account").value==="";')
        browser.execute('document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return [...document.querySelectorAll("[data-service]")].some(node=>node.dataset.service==="provider:imported");'))
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && state.providers.find(p=>p.id==="imported").name==="Imported API renamed" && state.accounts.find(a=>a.id==="import-account").name==="Imported account";')
        stored=browser.execute('return localStorage.getItem("emp.shadow.config.v1");')
        assert 'fixture-import-token' not in stored and 'fixture-import-key' not in stored
        assert (state_dir.parent/'config.json').read_bytes()==stored_before
        checks.append('Encrypted one-click import, inline name retention through selection refresh, empty-name preservation, read-only projection, draft-only save and credential scrubbing')
        assert browser.execute('return state.providers.find(p=>p.id==="demo").name==="Earlier draft";'),'Import discarded an earlier shadow edit'
        repeated=browser.execute('return {token:state.accounts.find(a=>a.id==="import-account")._shadow_migration_token,current_config:shadowScrub(state),selections:[{id:"import-account",kind:"account",name:"Again",models:[]}],include_context:false,include_rename:false};')
        repeat_result=async_api(browser,'/api/shadow/migration/preview',repeated)
        assert [a['id'] for a in repeat_result['config']['accounts']].count('import-account')==1
        assert not any(a['id'].startswith('import-account-') for a in repeat_result['config']['accounts'])
        assert next(p for p in repeat_result['config']['providers'] if p['id']=='demo')['name']=='Earlier draft'
        checks.append('Prior shadow edits survive import and repeat account identity remains stable')
        assert any('已确认同一账号' in warning['message'] for warning in repeat_result['warnings'])
        browser.execute('document.getElementById("shadow_migration_open").click();')
        browser.execute('const bytes=Uint8Array.from(atob(arguments[0]),c=>c.charCodeAt(0)),data=new DataTransfer();data.items.add(new File([bytes],"same.emp"));document.getElementById("shadow_migration_file").files=data.files;document.getElementById("shadow_import_password").value=arguments[1];document.querySelector("[data-migration-decode]").click();',encoded,password)
        wait_for(lambda:browser.execute('return document.querySelector("[data-shadow-tone=warning]") && !document.getElementById("modal_submit").disabled;'),seconds=90)
        browser.execute('const detail=[...document.querySelectorAll("[data-migration-detail]")].find(node=>node.dataset.migrationDetail==="provider:imported");detail.open=true;detail.querySelector("[data-migration-model-select]").click();')
        wait_for(lambda:browser.execute('return !document.getElementById("modal_submit").disabled;'))
        assert browser.execute('const detail=[...document.querySelectorAll("[data-migration-detail]")].find(node=>node.dataset.migrationDetail==="provider:imported");return detail.open && detail.querySelector("summary").textContent.includes("0");')
        browser.execute('[...document.querySelectorAll("[data-migration-detail]")].find(node=>node.dataset.migrationDetail==="provider:imported").querySelector("[data-migration-model-select]").click();')
        wait_for(lambda:browser.execute('return !document.getElementById("modal_submit").disabled;'))
        browser.execute('const field=document.querySelector("[data-migration-name]");field.value="Discard this rename";field.dispatchEvent(new Event("input",{bubbles:true}));document.getElementById("modal_back").click();document.querySelector(".modal-close").click();')
        assert browser.execute('return localStorage.getItem("emp.shadow.config.v1");')==stored
        checks.append('Actual duplicate-ID import stays enabled; model expansion survives selection; closing discards inline name edits')
        browser.execute('document.getElementById("shadow_migration_open").click();document.querySelector("[data-migration-demo]").click();')
        wait_for(lambda:browser.execute('return document.querySelector(".migration-demo-note") && document.querySelector("[data-shadow-tone=warning]") && !document.getElementById("modal_submit").disabled;'))
        assert browser.execute('return !document.getElementById("modal_submit").disabled && !document.querySelector(".migration-service input:not([type=checkbox])") && [...document.querySelectorAll(".migration-preview-models")].every(node=>!node.open);')
        assert browser.execute('return [...document.querySelectorAll("[data-shadow-tone=warning]")].some(node=>node.dataset.shadowHelp.includes("名称相同不影响导入")) && !/服务 ID|新 ID|名称：/.test(document.getElementById("migration_preview_rows").textContent);')
        for theme in ['light','dark']:
            browser.execute('setTheme(arguments[0]);',theme)
            browser.hover('[data-shadow-tone=warning]')
            wait_for(lambda:browser.execute('return !document.getElementById("shadow_feedback").hidden && document.getElementById("shadow_feedback").dataset.tone==="warning";'))
            assert browser.execute('return /服务|账号|名称|连接/.test(document.getElementById("shadow_feedback").textContent);')
            if args.output:
                status,png=request(browser.base+'/screenshot');assert status==200
                args.output.with_name('shadow-migration-warning-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
        assert browser.execute('return document.querySelectorAll("#migration_preview_rows [data-migration-name]").length===2;')
        for theme in ['light','dark']:
            browser.execute('setTheme(arguments[0]);document.querySelectorAll("[data-migration-name]").forEach(input=>{input.value="Same name";input.dispatchEvent(new Event("input",{bubbles:true}));});',theme)
            assert browser.execute('return !document.getElementById("modal_submit").disabled && [...document.querySelectorAll("#migration_preview_rows [data-migration-name]")].every(input=>input.getBoundingClientRect().right<=input.closest(".modal").getBoundingClientRect().right && input.getBoundingClientRect().width>100 && Math.abs(input.closest(".migration-preview-row").querySelector(".service-brand").getBoundingClientRect().left-input.closest(".migration-preview-row").getBoundingClientRect().left)<1);')
            if args.output:
                status,png=request(browser.base+'/screenshot');assert status==200
                args.output.with_name('shadow-migration-names-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
        browser.execute('document.getElementById("modal_submit").click();setTheme("light");')
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && localStorage.getItem("emp.shadow.config.v1");')==stored
        checks.append('Import demo supports inline duplicate names in both themes without saving')
        browser.execute('document.getElementById("shadow_migration_open").click();document.querySelector("[data-migration-mode=export]").click();')
        for theme in ['light','dark']:
            browser.execute('setTheme(arguments[0]);',theme)
            for width in [1000,680]:
                request(browser.base+'/window/rect',{'width':width,'height':950})
                assert browser.execute("const row=document.querySelector('.migration-service'),check=row.firstElementChild,body=row.lastElementChild;const c=check.getBoundingClientRect(),b=body.getBoundingClientRect();return c.right<b.left&&Math.abs(c.top+c.height/2-b.top-b.height/2)<2"),'Migration checkbox is not aligned'
                assert browser.execute("const pane=document.querySelector('.migration-choices'),bar=pane.querySelector('.toolbar'),rows=pane.querySelector('.migration-services');return rows.getBoundingClientRect().top-bar.getBoundingClientRect().bottom<16"),'Migration list acquired a large empty gap'
            if args.output:
                status,png=request(browser.base+'/screenshot');assert status==200
                args.output.with_name('shadow-cleanup-migration-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
        browser.execute('closeModal();setTheme("light");')
        checks.append('Migration layout keeps checkbox columns and list top alignment in both themes')


        # Export an imported preview service: the private token keeps credentials on the server.
        row=browser.execute('const p=state.providers.find(p=>p.id==="imported");return {id:p.id,kind:"provider",name:p.name,source_token:p._shadow_migration_token,models:state.models.filter(m=>m.provider===p.id).map(m=>({id:m.id,name:m.display_name,selected:true,context_window:m.context_window}))};')
        response=async_api(browser,'/api/shadow/migration/export',{'password':password,'selections':[row],'include_context':False,'include_rename':False})
        exported=codec(args.helper,{'operation':'decode','password':password,'bundle':response['bundle']})['payload']
        assert len(exported['config']['providers'])==1 and exported['config']['providers'][0]['name']=='Imported API renamed'
        assert exported['provider_keys']=={'imported':'fixture-import-key'}
        assert not exported['accounts'] and 'demo' not in exported['provider_keys']
        # Synthetic Native credentials validate the random ID against the encrypted file.
        (root/'codex_home'/'auth.json').write_text(json.dumps({'tokens':{'access_token':'fixture-native-only','account_id':'fixture-native-only'}}))
        browser.execute('state.native_account.credential_set=true;document.getElementById("shadow_migration_open").click();document.querySelector("[data-migration-mode=export]").click();')
        native_id=browser.execute('return [...document.querySelectorAll("[data-migration-detail]")].find(node=>node.dataset.migrationDetail.startsWith("account:")&&!node.dataset.migrationDetail.includes("import-account")).dataset.migrationDetail.split(":")[1];')
        assert len(native_id)==4 and native_id.isalnum()
        browser.execute('document.getElementById("migration_include_context").click();')
        assert browser.execute('return !!document.querySelector(arguments[0]);',f'[data-migration-detail="account:{native_id}"]')
        native_response=async_api(browser,'/api/shadow/migration/export',{'password':password,'selections':[{'id':'@native','kind':'account','export_id':native_id,'models':[]}],'include_context':False,'include_rename':False})
        native_payload=codec(args.helper,{'operation':'decode','password':password,'bundle':native_response['bundle']})['payload']
        assert native_payload['accounts'][0]['metadata']['id']==native_id
        assert native_payload['accounts'][0]['metadata']['prefix']==native_id
        assert native_payload['accounts'][0]['auth']['tokens']['account_id']=='fixture-native-only'
        browser.execute('closeModal();')
        checks.append('Native four-character ID stays stable in the preview and encrypted export')
        assert (state_dir.parent/'config.json').read_bytes()==stored_before
        checks.append('Selected encrypted re-export retains the imported key and excludes unselected services')
        assert upstream.calls==0
        result={'passed':True,'checks':checks,'geometry':geometry,'synthetic_model_calls':0,'production_configuration_unchanged':True}
        if args.output:args.output.write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps({'passed':True,'checks':checks,'geometry_cases':len(geometry)}))


if __name__=='__main__':
    main()
