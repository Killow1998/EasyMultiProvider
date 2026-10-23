"""Service-local model management and chart headroom with synthetic data only."""
import argparse
import base64
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
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();checks=[]
    with tempfile.TemporaryDirectory(prefix='emp-model-manager-') as temp,contextlib.ExitStack() as stack:
        root=Path(temp);base,_,state_dir,_,upstream=start_backend(stack,args.emp.resolve(),root)
        before=(state_dir.parent/'config.json').read_bytes()
        env=dict(os.environ,HOME=str(root/'preview-home'),CODEX_HOME=str(root/'codex_home'));Path(env['HOME']).mkdir()
        log_path=root/'preview.log';log=stack.enter_context(log_path.open('w'))
        process=subprocess.Popen([sys.executable,str(HERE/'shadow-web.py'),'--port','0','--backend-port',base.rsplit(':',1)[1],'--state-dir',str(state_dir)],env=env,stdout=log,stderr=log)
        stack.callback(stop,process)
        url=wait_for(lambda:log_path.read_text().split('EMP shadow: ')[-1].strip() if 'EMP shadow:' in log_path.read_text() else None)
        browser=Browser(stack,root,system_access=True);browser.navigate(url)
        browser.execute('''
          const saved=window.fetch;
          const models=Array.from({length:400},(_,i)=>({upstream_id:'catalog-'+i,display_name:'Model '+i,context_window:128000,input_modalities:['text','image']}));
          window.fetch=async function(path,options){if(String(path)==='/api/providers/demo/models')return new Response(JSON.stringify({models,cached:true}),{status:200,headers:{'Content-Type':'application/json'}});return saved(path,options);};
          providerModelSettings.open('demo');
        ''')
        wait_for(lambda:browser.execute('return document.querySelectorAll("[data-managed-model]").length===401;'))
        assert browser.execute('return !document.getElementById("models") && !document.querySelector("[data-ui-action=provider-toggle-models]") && !document.querySelector("#shadow_model_manager input[type=checkbox]");')
        assert browser.execute('return document.querySelector("[data-managed-model]").dataset.visible==="true" && document.querySelectorAll(".shadow-model-action").length===1203;')
        checks.append('400 cached models; visible first; only icon actions; retired homepage controls absent')
        def search(value):
            browser.execute('const input=document.getElementById("shadow_model_search");input.value=arguments[0];input.dispatchEvent(new Event("input",{bubbles:true}));',value)
        search('catalog-23')
        browser.execute('document.querySelector("[data-manager-action=toggle]").click();')
        wait_for(lambda:browser.execute('return state.models.some(model=>model.upstream_id==="catalog-23"&&model.enabled);'))
        search('catalog-32')
        browser.execute('document.querySelector("[data-manager-action=edit]").click();')
        assert browser.execute('return document.getElementById("modal_model_provider").disabled && document.getElementById("modal_model_upstream").value==="catalog-32";')
        assert browser.execute('const back=document.querySelector("#modal_back"),close=document.getElementById("modal_close");return !back.hidden && close.hidden && back.parentElement===close.parentElement && !document.querySelector(".modal-actions #modal_back");')
        browser.execute('document.querySelector("#modal_back").click();')
        wait_for(lambda:browser.execute('return !!document.getElementById("shadow_model_manager");'))
        assert browser.execute('return document.getElementById("shadow_model_search").value==="catalog-32";')
        browser.execute('document.querySelector("[data-manager-action=edit]").click();')
        browser.escape()
        wait_for(lambda:browser.execute('return document.getElementById("shadow_model_search")?.value==="catalog-32";'))
        browser.execute('document.getElementById("modal_close").click();')
        assert browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");')
        browser.execute('providerModelSettings.open("demo","catalog-32");')
        wait_for(lambda:browser.execute('return !!document.querySelector("[data-manager-action=edit]");'))
        browser.execute('document.querySelector("[data-manager-action=edit]").click();')
        browser.execute('document.getElementById("modal_model_context").value="345000";document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return !!document.getElementById("shadow_model_manager");'))
        assert browser.execute('return state.models.some(model=>model.upstream_id==="catalog-32"&&model.context_window===345000&&model.enabled===false) && document.getElementById("shadow_model_search").value==="catalog-32";')
        browser.execute('document.querySelector("[data-manager-action=add]").click();document.getElementById("modal_model_upstream").value="manual-only";document.getElementById("modal_model_context").value="64000";document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return !!document.getElementById("shadow_model_manager");'))
        assert browser.execute('return state.models.some(model=>model.id==="demo/manual-only") && document.getElementById("modal_back").hidden;')
        search('manual-only');browser.execute('document.querySelector("[data-manager-action=test]").click();')
        assert browser.execute('return document.getElementById("status").textContent.includes("不发送真实模型请求");')
        checks.append('Child has only Back; Back/Escape retain model search; manager X closes; no footer back button')
        checks.append('Show, edit hidden catalog model, service-scoped manual add, return/search retention and guarded test')
        search('')
        for theme in ['light','dark']:
            browser.execute('setTheme(arguments[0]);',theme)
            for width in [640,1280]:
                request(browser.base+'/window/rect',{'width':width,'height':750})
                assert browser.execute('return [...document.querySelectorAll(".shadow-managed-model")].every(node=>{const title=node.firstElementChild.getBoundingClientRect(),buttons=node.lastElementChild.getBoundingClientRect();return title.right<=buttons.left && buttons.right<=node.getBoundingClientRect().right});')
            status,png=request(browser.base+'/screenshot');assert status==200
            args.output.with_name('shadow-model-manager-'+theme+'.png').write_bytes(base64.b64decode(png['value']))
        checks.append('Readable model rows and actions in light/dark at narrow and desktop widths')
        browser.execute('closeModal();const native=[{id:"native-a",display_name:"A",default_context_window:500000,max_context_window:872000,effective_context_window_percent:95},{id:"native-b",display_name:"B",default_context_window:272000,max_context_window:872000,effective_context_window_percent:95}];const previous=window.fetch;window.fetch=async function(path,options){if(String(path).includes("/api/accounts/")&&String(path).endsWith("/models"))return new Response(JSON.stringify({models:native}),{status:200,headers:{"Content-Type":"application/json"}});return previous(path,options);};state.subscription_models=native;editAccount(state.native_account.id);')
        wait_for(lambda:browser.execute('return !!document.getElementById("shadow_usable_0");'))
        assert browser.execute('return !document.querySelector("#subscription_model_list input[type=checkbox]") && document.querySelector(".subscription-list-head .shadow-help").dataset.shadowHelp.includes("在线目录") && document.querySelector("[data-shadow-context-help]").dataset.shadowHelp.includes("475,000");')
        assert browser.execute('return !document.querySelector("#subscription_model_list .subscription-identity small") && !document.getElementById("subscription_model_list").textContent.includes("native-a");')
        browser.execute('const field=document.querySelector("[name=subscription_context]");field.value="400000";field.dispatchEvent(new Event("input",{bubbles:true}));')
        assert browser.execute('return document.querySelector("[data-shadow-context-help]").dataset.shadowHelp.includes("380,000");')
        browser.execute('document.querySelector("[data-shadow-subscription-visibility]").click();document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        browser.execute('editAccount(state.native_account.id);')
        wait_for(lambda:browser.execute('return document.querySelectorAll("[data-shadow-subscription-visibility]").length===2;'))
        assert browser.execute('return [...document.querySelectorAll("[data-shadow-subscription-visibility]")].map(node=>node.getAttribute("aria-pressed")).join(",")==="true,false" && state.native_account.model_context_windows["native-a"]===400000;')
        browser.execute('closeModal();')
        checks.append('Native visibility icons persist and sort; global context help and current per-model calculation agree')
        browser.execute('statistics.open();')
        assert browser.execute('return !document.getElementById("presentation_stats").textContent.includes("历史与计价") && !document.querySelector("#presentation_stats [onclick*=openUsage]");')
        browser.execute('closeModal();')
        checks.append('No duplicate Native model route names or obsolete History and pricing entry')
        # Check the actual plotted peak, not an internal scale variable.
        browser.execute('closeModal();openModal("Fixture chart","<div id=chart_fixture></div>","",null);window.fixtureChart=createPerformanceChart({esc,tr,getLanguage:()=>language,number:value=>String(value),seconds:value=>String(value),tps:value=>String(value)});')
        for metric in ['tokens','cost_nanos','tokens_per_second','ttft_ms']:
            browser.execute('''
              window.fixtureDispose?.();const report={start:0,end:120,periods:[{start:0,tokens:100,cost_nanos:100,tokens_per_second:100,ttft_ms:100},{start:60,tokens:200,cost_nanos:200,tokens_per_second:200,ttft_ms:200}]};
              if(['tokens','cost_nanos'].includes(arguments[0]))report.chartSeries=[{values:[100,100],color:'#537de0'},{values:[0,200],color:'#279e9b'}];
              document.getElementById('chart_fixture').innerHTML=fixtureChart.html(report,arguments[0]);window.fixtureDispose=fixtureChart.bind(document.getElementById('chart_fixture'),report,arguments[0]);
            ''',metric)
            peak=browser.execute('const svg=document.querySelector("#chart_fixture svg");const marks=[...svg.querySelectorAll("[data-stack-value],.presentation-chart-point")];return Math.min(...marks.map(node=>Number(node.getAttribute(node.tagName==="rect"?"y":"cy"))));')
            assert abs(peak-(180-162/1.05))<.06,(metric,peak)
        browser.execute('window.fixtureDispose();closeModal();')
        checks.append('Token/USD stacks and TPS/TTFT peaks use exactly 105 percent headroom')
        assert (state_dir.parent/'config.json').read_bytes()==before and upstream.calls==0
        args.output.write_text(json.dumps({'passed':True,'checks':checks,'model_count':400,'production_configuration_unchanged':True,'model_requests':0},indent=2)+'\n')
        print(json.dumps({'passed':True,'checks':checks}))


if __name__=='__main__':main()
