"""Delayed modal actions against shipped assets and synthetic HTTP replies."""
import argparse
import contextlib
import json
import tempfile
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_environment import start_backend, wait_for
from e2e_refinements import Browser


def run_case(browser, operation, outcome, navigation):
    browser.execute('''
      closeModal();window.completionDone=false;window.releaseCompletion=null;
      window.completionOperation=arguments[0];window.completionOutcome=arguments[1];
      window.completionCalls=[];
      if(completionOperation==='model'){
        state.catalog_families=[{id:'completion-model',default_display_name:'Fixture',routes:[]}];
        openCatalogDisplayEditor('completion-model');
        document.getElementById('modal_catalog_alias').value='Unsaved fixture name';
      }else if(completionOperation==='reset'){
        state.accounts=[{id:'completion-account',name:'Fixture',quota:{credits:{reset_credits:{
          available_count:1,credits:[{id:'fixture-credit',status:'available',expires_at:'2030-01-01T00:00:00Z'}]
        }}}}];
        openResetCredits('completion-account');document.getElementById('modal_submit').click();
      }else confirmIntegrationAction(completionOperation);
      const button=document.getElementById('modal_submit');
      window.completionLabel=button.textContent;
      button.onclick().then(()=>window.completionDone=true);
    ''', operation, outcome)
    wait_for(lambda: browser.execute('return typeof releaseCompletion==="function";'))
    assert browser.execute('return document.getElementById("modal_submit").disabled;')
    browser.execute('''
      const navigation=arguments[0];
      if(navigation==='replace'){
        openModal('Replacement','<input id="completion_draft" value="Keep this draft">','Keep editing',()=>{});
        document.getElementById('modal_status').textContent='Replacement status';
      }else if(navigation==='close'){
        while(!document.getElementById('modal_back').hidden)document.getElementById('modal_back').click();
        document.getElementById('modal_close').click();
      }
      else if(navigation==='back')document.getElementById('modal_back').click();
      window.completionReplacement=document.getElementById('modal_submit').onclick;
      releaseCompletion();
    ''', navigation)
    wait_for(lambda: browser.execute('return completionDone;'))
    if navigation == 'stay':
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");') == (outcome == 'success')
        if outcome == 'failure':
            assert browser.execute('return document.getElementById("modal_status").textContent==="Synthetic request failure" && !document.getElementById("modal_submit").disabled && document.getElementById("modal_submit").textContent===completionLabel;')
            if operation == 'model':
                assert browser.execute('return document.getElementById("modal_catalog_alias").value==="Unsaved fixture name";')
    elif navigation == 'close':
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden") && document.getElementById("modal_status").textContent==="";')
    else:
        assert browser.execute('return !document.getElementById("modal_backdrop").classList.contains("hidden") && document.getElementById("modal_submit").onclick===completionReplacement && !document.getElementById("modal_submit").disabled;')
        if navigation == 'replace':
            assert browser.execute('return document.getElementById("completion_draft").value==="Keep this draft" && document.getElementById("modal_status").textContent==="Replacement status" && document.getElementById("modal_submit").textContent==="Keep editing";')
        else:
            assert browser.execute('return !!document.querySelector(".reset-credit-list") && document.querySelector("[name=reset_credit_choice]:checked").value==="0" && document.getElementById("modal_status").textContent==="";')
    if outcome == 'success':
        if operation == 'model':
            assert browser.execute('return state.catalog_family_presentations["completion-model"].catalog_alias==="Unsaved fixture name";')
        elif operation == 'reset':
            assert browser.execute('return !resetAttempts.has("completion-account") && completionCalls.includes("/api/accounts");')
        else:
            assert browser.execute('return !integrationBusy && integrationInfo.configuration.state===arguments[0];', 'emp_applied' if operation == 'enable' else 'native')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks, scenarios = [], 0
    with tempfile.TemporaryDirectory(prefix='emp-modal-completion-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, url, _, _, upstream = start_backend(stack, args.emp.resolve(), root, {'auto_enable_on_start': False})
        web = Path(__file__).resolve().parents[2] / 'crates/emp-app/web'
        frontend, _ = candidate_frontend(stack, base, web)
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        before = (root / 'config.json').read_bytes()
        browser.execute('''
          managementEvents.stop();clearInterval(quotaStateTimer);
          window.completionCalls=[];
          window.completionConfig=structuredClone(state);
          window.completionIntegration={configuration:{state:'native'},runtime:{state:'not_checked'}};
          const response=(data,status=200)=>new Response(JSON.stringify(data),{status,headers:{'Content-Type':'application/json'}});
          window.fetch=async(path,options={})=>{
            const route=String(path);completionCalls.push(route);
            if(options.method==='POST' && (route==='/api/config' || ['/api/integration/enable','/api/integration/restore','/api/integration/reload'].includes(route) || route.endsWith('/quota-reset'))){
              return new Promise(resolve=>{
                window.releaseCompletion=()=>{
                  if(completionOutcome==='failure'){resolve(response({error:{message:'Synthetic request failure'}},503));return;}
                  if(route==='/api/config'){
                    completionConfig=JSON.parse(options.body);resolve(response(completionConfig));
                  }else if(route.endsWith('/quota-reset')){
                    const payload=JSON.parse(options.body);
                    if(!payload.idempotency_key || payload.credit_id!=='fixture-credit')throw new Error('Invalid reset fixture request');
                    resolve(response({outcome:'reset'}));
                  }else{
                    completionIntegration={configuration:{state:route.endsWith('/enable')?'emp_applied':'native'},runtime:{state:'not_checked'}};
                    resolve(response(completionIntegration));
                  }
                };
              });
            }
            if(route==='/api/config')return response(completionConfig);
            if(route==='/api/integration' || route==='/api/integration/verify')return response(completionIntegration);
            if(route==='/api/accounts')return response({native_account:state.native_account,accounts:state.accounts});
            return response({});
          };
        ''')
        for language in ['zh-CN', 'en']:
            browser.execute('language=arguments[0];applyTranslations();', language)
            for operation in ['model', 'enable', 'restore', 'reload', 'reset']:
                for outcome in ['success', 'failure']:
                    for navigation in ['stay', 'replace', 'close'] + (['back'] if operation == 'reset' else []):
                        try:
                            run_case(browser, operation, outcome, navigation)
                        except Exception:
                            print(json.dumps({'language': language, 'operation': operation, 'outcome': outcome, 'navigation': navigation}), flush=True)
                            raise
                        scenarios += 1
                checks.append(f'{language} / {operation}: normal completion, editable errors, replacement and closed dialogs' + (', Back preserves reset selection' if operation == 'reset' else ''))
        assert (root / 'config.json').read_bytes() == before
        assert upstream.calls == 0
    result = {'passed': True, 'scenarios': scenarios, 'checks': checks, 'real_model_requests': 0, 'real_quota_resets': 0}
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
