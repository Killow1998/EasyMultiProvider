"""Nested preview dialogs: explicit parents, close, icons and API classification."""
import argparse
import contextlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from e2e_refinements import Browser, HERE
from e2e_environment import request, start_backend, stop, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-navigation-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, _, state_dir, _, upstream = start_backend(stack, args.emp.resolve(), root)
        before = (root / 'config.json').read_bytes()
        log_path = root / 'preview.log'
        log = stack.enter_context(log_path.open('w'))
        process = subprocess.Popen([sys.executable, str(HERE / 'shadow-web.py'), '--port', '0',
            '--backend-port', base.rsplit(':', 1)[1], '--state-dir', str(state_dir)],
            env={**os.environ, 'HOME':str(root / 'preview-home'), 'CODEX_HOME':str(root / 'codex_home')},
            stdout=log, stderr=log)
        stack.callback(stop, process)
        url = wait_for(lambda: log_path.read_text().split('EMP shadow: ')[-1].strip()
            if 'EMP shadow:' in log_path.read_text() else None)
        browser = Browser(stack, root, system_access=True)
        browser.navigate(url)
        wait_for(lambda: browser.execute('return !!state?.providers?.length;'))
        browser.execute('''
          managementEvents.stop();clearInterval(quotaStateTimer);
          window.navigationWrites=[];window.navigationConfigs=[];
          const saved=window.fetch;
          window.fetch=async function(path,options={}){
            if((options.method||'GET')!=='GET')navigationWrites.push(String(path));
            if(String(path)==='/api/config' && options.method==='POST')navigationConfigs.push(JSON.parse(options.body));
            if(String(path).includes('/models'))return new Response(JSON.stringify({models:[],cached:true}),{status:200});
            return saved(path,options);
          };
          state.accounts=[{id:'nav-account',name:'Navigation account',prefix:'nav',credential_set:true,
            quota:{credits:{reset_credits:{available_count:2,credits:[
              {id:'credit-a',status:'available',expires_at:'2030-01-01T00:00:00Z'},
              {id:'credit-b',status:'available',expires_at:'2031-01-01T00:00:00Z'}]}}}}];
        ''')
        def execute(script, *values):
            return browser.execute(script, *values)
        def click(selector):
            assert execute('const node=document.querySelector(arguments[0]);if(!node||node.hidden)return false;if(node.matches("input"))node.focus();node.click();return true;', selector), selector
        def present(selector):
            wait_for(lambda: execute('return !!document.querySelector(arguments[0]);', selector))
            assert execute('return [...document.querySelectorAll("#modal_backdrop .modal-header .modal-navigation button")].filter(node=>node.getBoundingClientRect().width>0).length===1;')
        def back_to(selector):
            click('#modal_back')
            present(selector)
        for language in ['en', 'zh-CN']:
            execute('language=arguments[0];applyTranslations();', language)
            for theme in ['light', 'dark']:
                execute('setTheme(arguments[0]);closeModal();openServiceChooser();', theme)
                assert execute('return document.getElementById("modal_back").hidden;')
                for action, field in [('service-custom-api','#modal_provider_base'),
                        ('claude-cpa-choose','#modal_cpa_base'),
                        ('claude-local-choose','#modal_local_claude_name')]:
                    click('[data-ui-action="'+action+'"]')
                    present(field)
                    back_to('.service-choice-group')
                click('[data-ui-action="official-provider-choose"][data-provider="openai"]')
                present('#modal_provider_key')
                back_to('.service-choice-group')
                click('[data-ui-action="account-import-auth"]')
                present('#codex_import_form')
                browser.escape()
                present('.service-choice-group')
                click('[data-ui-action="account-import-auth"]')
                execute('codexAccountImport.fileChosen({files:[{name:"auth.json",text:async()=>"{}"}],value:""});')
                present('#codex_import_form')
                back_to('.service-choice-group')
                assert execute('return document.getElementById("modal_back").hidden;')
                click('[data-ui-action="service-custom-api"]')
                present('#modal_provider_base')
                assert execute('return document.getElementById("modal_close").hidden;')
                browser.escape()
                present('.service-choice-group')
                click('#modal_close')
                assert execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && document.getElementById("modal_back").onclick===null;')
                execute('openAccountDetails("nav-account");')
                present('#account_usage')
                assert execute('const b=document.querySelector(".shadow-detail-actions [data-ui-action=account-remove]");return b.dataset.icon==="trash" && !b.querySelector("svg") && getComputedStyle(b,"::before").maskImage!=="none";')
                click('[data-ui-action="account-edit"]')
                present('#subscription_model_list')
                click('[data-ui-action="account-reset-credits"]')
                present('.reset-credit-list')
                execute('document.querySelectorAll("[name=reset_credit_choice]")[1].click();')
                click('#modal_submit')
                wait_for(lambda: execute('return !document.querySelector(".reset-credit-list");'))
                back_to('.reset-credit-list')
                assert execute('return document.querySelector("[name=reset_credit_choice]:checked").value==="1";')
                click('#modal_submit')
                browser.escape()
                present('.reset-credit-list')
                assert execute('return document.querySelector("[name=reset_credit_choice]:checked").value==="1";')
                back_to('#subscription_model_list')
                assert execute('return document.querySelector("[data-service-view=edit]").getAttribute("aria-selected")==="true" && state.accounts[0].quota.credits.reset_credits.available_count===2;')
                execute('closeModal();state.providers.push({id:"nav-claude",name:"Local",execution_backend:"claude_cli",auth_mode:"claude_login"});editProvider("nav-claude");')
                click('[data-ui-action="claude-change-connection"]')
                click('[data-ui-action="claude-cpa-choose"]')
                present('#modal_cpa_base')
                back_to('.choice-grid')
                back_to('#modal_local_claude_name')
                execute('closeModal();state.providers=state.providers.filter(p=>p.id!=="nav-claude");')
                checks.append(language+' / '+theme+': one navigation control, add/import/file/Claude/reset direct parents, Escape, root close and preserved selection/tab')
        execute('closeModal();usageReport.open();')
        present('#usage_report')
        execute('document.getElementById("usage_category").value="external";document.getElementById("usage_bucket").value="day";window.navUsageRange={start:periodPickers.usage.start,end:periodPickers.usage.end};')
        click('[data-usage-action="calls"]')
        present('#call_report')
        back_to('#usage_report')
        assert execute('return document.getElementById("usage_category").value==="external" && document.getElementById("usage_bucket").value==="day" && periodPickers.usage.start===navUsageRange.start && periodPickers.usage.end===navUsageRange.end;')
        checks.append('Legacy usage-to-calls Back retains category, grouping and selected period')
        for language in ['en', 'zh-CN']:
            for theme in ['light', 'dark']:
                execute('language=arguments[0];setTheme(arguments[1]);', language, theme)
                forms = [
                    ('openCustomProviderForm(state.providers.find(p=>p.id==="demo"));', '#modal_provider_key', 'demo'),
                    ('state.providers.push({id:"openai",name:"OpenAI",base_url:"https://api.openai.com/v1",protocol:"responses",auth_mode:"api_key",api_key_set:true});chooseOfficialProvider("openai");', '#modal_provider_key', 'openai'),
                    ('state.providers.push({id:"nav-cpa",name:"CPA",base_url:"https://cpa.example.com",execution_backend:"claude_cli",auth_mode:"api_key",api_key_set:true});editProvider("nav-cpa");', '#modal_cpa_key', 'nav-cpa')]
                for opening, selector, provider_id in forms:
                    execute(opening)
                    assert execute('const key=document.querySelector(arguments[0]);return key.type==="text" && key.value==="" && key.placeholder==="******" && !/可选|必填|optional|required|留空保持|leave blank to keep/i.test(document.getElementById("modal_body").textContent);', selector)
                    click('#modal_submit')
                    wait_for(lambda: execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
                    assert execute('return navigationConfigs.at(-1).providers.find(p=>p.id===arguments[0]).api_key==="••••••••";', provider_id)
                    execute('editProvider(arguments[0]);', provider_id)
                    click(selector)
                    assert execute('const key=document.querySelector(arguments[0]);return key.value==="" && key.placeholder==="";', selector)
                    click('#modal_submit')
                    wait_for(lambda: execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
                    assert execute('return navigationConfigs.at(-1).providers.find(p=>p.id===arguments[0]).api_key==="••••••••";', provider_id)
                    execute('editProvider(arguments[0]);window.keyDraftBefore=JSON.stringify(shadowDraft);window.keyWritesBefore=navigationConfigs.length;', provider_id)
                    click(selector)
                    execute('document.querySelector(arguments[0]).value="fixture-discarded-key";', selector)
                    click('#modal_close')
                    assert execute('return JSON.stringify(shadowDraft)===keyDraftBefore && navigationConfigs.length===keyWritesBefore;')
                    execute('editProvider(arguments[0]);', provider_id)
                    assert execute('return document.querySelector(arguments[0]).placeholder==="******" && document.querySelector(arguments[0]).value==="";', selector)
                    click(selector)
                    execute('const key=document.querySelector(arguments[0]);key.value="fixture-visible-key";key.dispatchEvent(new InputEvent("input",{inputType:"insertFromPaste",data:"fixture-visible-key",bubbles:true}));', selector)
                    assert execute('const key=document.querySelector(arguments[0]);return key.type==="text" && key.value==="fixture-visible-key";', selector)
                    click('#modal_submit')
                    wait_for(lambda: execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
                    assert execute('return navigationConfigs.at(-1).providers.find(p=>p.id===arguments[0]).api_key==="fixture-visible-key" && !JSON.stringify(shadowDraft).includes("fixture-visible-key");', provider_id)
                    execute('state.providers=state.providers.filter(p=>!["openai","nav-cpa"].includes(p.id));')
                execute('openCustomProviderForm();')
                assert execute('return document.getElementById("modal_provider_key").placeholder==="";')
                execute('document.getElementById("modal_provider_base").value="https://empty-key.example.com";')
                click('#modal_submit')
                wait_for(lambda: execute('return !!document.getElementById("modal_status").textContent;'))
                assert execute('return !document.getElementById("modal_backdrop").classList.contains("hidden");')
                execute('closeModal();')
        checks.append('Custom/official/CPA keys: six-star saved marker, clear on focus, visible pasted replacement, untouched/focused-empty preserve key, unsaved replacement discarded on close, empty new key rejected in EN/ZH and both themes')
        # The outcome overlay has one Back control; Escape also preserves the report.
        execute('statistics.open();')
        present('#presentation_stats')
        execute('''
          statistics.stop();callReports.stop();
          const parent=document.getElementById('presentation_stats');
          parent.querySelector('[data-stats-filter=category]').value='external';
          parent.querySelector('[data-stats-filter=session]').value='Keep this filter';
          const root=document.createElement('div');root.id='nav_outcome_fixture';parent.append(root);
          const marker=document.createElement('button');marker.dataset.callOutcomes='';root.append(marker);
          window.navOutcomeDispose=createCallOutcomes({esc,tr,number:String,time:String}).bindOutcomes(root,{success_samples:2,completed:1,failed:1});
          window.navReportNode=parent;
        ''')
        click('#nav_outcome_fixture [data-call-outcomes]')
        present('.call-outcome-dialog')
        assert execute('return document.querySelectorAll(".call-outcome-dialog .modal-navigation button").length===1 && !document.querySelector(".call-outcome-dialog .modal-close");')
        click('.call-outcome-dialog .modal-back')
        assert execute('return !document.querySelector(".call-outcome-dialog") && navReportNode===document.getElementById("presentation_stats");')
        click('#nav_outcome_fixture [data-call-outcomes]')
        browser.escape()
        assert execute('return !document.querySelector(".call-outcome-dialog") && navReportNode===document.getElementById("presentation_stats") && !document.getElementById("modal_backdrop").classList.contains("hidden");')
        assert execute('return navReportNode.querySelector("[data-stats-filter=category]").value==="external" && navReportNode.querySelector("[data-stats-filter=session]").value==="Keep this filter";')
        click('#modal_backdrop .modal-close')
        assert execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && !document.querySelector(".call-outcome-dialog");')
        checks.append('Outcome overlay has only Back; Back and Escape retain the report, whose own X still closes it')
        assert execute('''
          language='en';const view=createMigrationView(()=>state);
          const config={providers:[
            {id:'api',name:'API',base_url:'https://api.example.com',auth_mode:'api_key'},
            {id:'router',name:'Router',base_url:'https://openrouter.ai/api/v1',auth_mode:'api_key'},
            {id:'cpa',name:'CPA',execution_backend:'claude_cli',auth_mode:'api_key'},
            {id:'local',name:'Local',execution_backend:'claude_cli',auth_mode:'claude_login'}],models:[]};
          const html=view.previewRows(view.services(config,true),{mode:'import',rows:[]});
          return (html.match(/External API service/g)||[]).length===3 && html.includes('Local Claude subscription') && !html.includes('General API') && !view.mergeRules().includes('API, OpenRouter');
        ''')
        checks.append('API / OpenRouter / CPA share one classification; local Claude remains a subscription')
        for zoom in [1, 1.5, 1.7]:
            request(browser.base+'/moz/context', {'context':'chrome'})
            request(browser.base+'/execute/sync', {'script':'gBrowser.selectedBrowser.browsingContext.fullZoom=arguments[0];','args':[zoom]})
            request(browser.base+'/moz/context', {'context':'content'})
            for theme in ['light', 'dark']:
                execute('setTheme(arguments[0]);openAccountDetails("nav-account");', theme)
                present('#account_usage')
                click('[data-ui-action="account-reset-credits"]')
                assert execute('const b=document.getElementById("modal_back"),x=document.getElementById("modal_close"),r=b.getBoundingClientRect();return !b.hidden && x.hidden && r.width>0 && r.right<=innerWidth && r.bottom<=innerHeight && getComputedStyle(b,"::before").maskImage!=="none" && getComputedStyle(b).borderTopWidth==="0px" && getComputedStyle(b).backgroundColor==="rgba(0, 0, 0, 0)";')
        checks.append('One Back icon at 100/150/170 percent in both themes; child dialogs hide X')
        assert execute('return !navigationWrites.some(path=>path.includes("quota-reset")||path.includes("/v1/"));')
        assert (root / 'config.json').read_bytes() == before
        assert upstream.calls == 0
        checks.append('No quota redemption, model calls or production config writes')
        args.output.write_text(json.dumps({'checks':checks,'passed':len(checks)},indent=2)+'\n')
        print(json.dumps({'passed':len(checks),'output':str(args.output)}))


if __name__ == '__main__':
    main()
