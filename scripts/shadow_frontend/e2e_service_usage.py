"""Service detail progress, error recovery, partial usage and modal keyboard acceptance."""
import argparse
import base64
import contextlib
import json
import tempfile
from pathlib import Path

from candidate_web import candidate_frontend
from e2e_environment import start_backend, request, wait_for
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks = []
    with tempfile.TemporaryDirectory(prefix='emp-service-usage-') as directory, contextlib.ExitStack() as stack:
        root = Path(directory)
        base, url, _, _, upstream = start_backend(stack, args.emp.resolve(), root, {'auto_enable_on_start': False})
        frontend, _ = candidate_frontend(stack, base, Path(__file__).resolve().parents[2] / 'crates/emp-app/web')
        browser = Browser(stack, root)
        browser.navigate(url.replace(base, frontend))
        browser.execute('managementEvents.stop();clearInterval(quotaStateTimer);dismissNotice();')
        def key(value, shift=False):
            actions = ([{'type':'keyDown','value':'\ue008'}] if shift else []) + [{'type':'keyDown','value':value},{'type':'keyUp','value':value}] + ([{'type':'keyUp','value':'\ue008'}] if shift else [])
            status, result = request(browser.base+'/actions', {'actions':[{'type':'key','id':'keyboard','actions':actions}]})
            assert status == 200, result
        for language in ['zh-CN', 'en']:
            browser.execute('setLanguage(arguments[0]);const button=document.querySelector("[data-i18n=settings_menu]");button.focus();button.click();', language)
            assert browser.execute('return document.activeElement===document.getElementById("modal_close");')
            key('\ue004', True)
            assert browser.execute('return document.getElementById("modal_backdrop").contains(document.activeElement);')
            key('\ue004')
            assert browser.execute('return document.activeElement===document.getElementById("modal_close");')
            key('\ue00c')
            assert browser.execute('return document.activeElement===document.querySelector("[data-i18n=settings_menu]");')
        checks.append('Keyboard enters the dialog, wraps in both directions and returns to the opener on Escape')
        # Exercise the real SSE endpoint and authorization before installing delayed fixtures.
        browser.execute('window.realUsage=null;window.realProgress=[];api("/api/usage?lifetime=true&owner=demo",{onProgress:p=>realProgress.push(p)}).then(r=>realUsage=r);')
        wait_for(lambda: browser.execute('return realUsage!==null;'))
        assert browser.execute('return Array.isArray(realUsage.groups) && realProgress.length>0;')
        status, _ = request(base+'/api/usage?lifetime=true&owner=demo')
        assert status == 401
        checks.append('Real lifetime SSE returns progress and owner aggregates; unauthenticated access is rejected')
        browser.execute('''
          window.originalUsageFetch=window.fetch;window.usageReads=[];
          window.fetch=(path,options={})=>{
            if(String(path).startsWith('/api/calls?'))return Promise.resolve(new Response(JSON.stringify({
              start:1,end:9999999999,offset:0,limit:50,total:2,records:[
                {request_id:'unknown-price',model_id:'fixture',state:'completed',input_tokens:10,output_tokens:2,pricing:{cost_nanos:0,price_issue:'unknown_model'}},
                {request_id:'known-zero',model_id:'fixture',state:'completed',input_tokens:10,output_tokens:2,pricing:{cost_nanos:0,price_issue:null}}
              ]
            }),{headers:{'Content-Type':'application/json'}}));
            if(String(path).startsWith('/api/usage?series=true'))return Promise.resolve(new Response(JSON.stringify({
              ...usageFixture,totals:usageFixture.groups[0],start:1,end:3601,bucket:600,
              series:[{...usageFixture.groups[0],start:1}],periods:[{start:1,requests:10}]
            }),{headers:{'Content-Type':'application/json'}}));
            if(!String(path).startsWith('/api/usage?lifetime=true&'))return originalUsageFetch(path,options);
            let stream;
            const response=new ReadableStream({start(controller){stream=controller;}});
            const entry={path:String(path),signal:options.signal,send:(event,data)=>stream.enqueue(new TextEncoder().encode('event: '+event+'\\ndata: '+JSON.stringify(data)+'\\n\\n'))};
            usageReads.push(entry);
            return Promise.resolve(new Response(response,{headers:{'Content-Type':'text/event-stream'}}));
          };
          window.usageFixture={groups:[{category:'external',owner:'demo',model:'fixture',requests:10,reported_requests:1,input_reports:1,output_reports:1,input_tokens:100,output_tokens:50,priced_requests:1,unpriced_requests:9,cost_nanos:0}],first_record_at:1};
          state.catalog_models.push({id:'fixture',display_name:'ALIAS_FIXTURE'});
          state.models.push({id:'fixture',upstream_id:'UNRELATED_MODEL'});
        ''')
        for language in ['zh-CN', 'en']:
            browser.execute('setLanguage(arguments[0]);openProviderDetails("demo");', language)
            wait_for(lambda: browser.execute('return usageReads.at(-1)?.path.includes("owner=demo");'))
            assert browser.execute('return !!document.querySelector("#service_usage progress:not([value])");')
            browser.execute('usageReads.at(-1).send("progress",{completed:25,total:100});')
            wait_for(lambda: browser.execute('return document.querySelector("#service_usage progress")?.value===25;'))
            assert browser.execute('return document.getElementById("service_usage").textContent.includes("25%");')
            if language=='zh-CN':
                status,png=request(browser.base+'/screenshot');assert status==200
                args.output.with_suffix('.png').write_bytes(base64.b64decode(png['value']))
            browser.execute('usageReads.at(-1).send("error",{error:{message:"Fixture unavailable"}});')
            wait_for(lambda: browser.execute('return !!document.querySelector("#service_usage button");'))
            browser.execute('document.querySelector("#service_usage button").click();')
            wait_for(lambda: browser.execute('return !!document.querySelector("#service_usage progress:not([value])");'))
            browser.execute('usageReads.at(-1).send("result",usageFixture);')
            wait_for(lambda: browser.execute('return !!document.querySelector("#service_usage .usage-total");'))
            assert browser.execute('return document.getElementById("service_usage").textContent.includes(tr("services.partial_usage")) && document.getElementById("service_usage").textContent.includes(tr("services.partial_estimate")) && document.querySelector("#service_usage .usage-total strong").textContent==="150 *";')
            assert browser.execute('return document.querySelector("#service_usage tbody").textContent.includes("fixture") && !document.getElementById("service_usage").textContent.includes("ALIAS_FIXTURE");')
            browser.execute('closeModal();openProviderDetails("demo");')
            wait_for(lambda: browser.execute('return !!document.querySelector("#service_usage progress");'))
            browser.execute('window.oldUsage=usageReads.at(-1);closeModal();openSettings();')
            assert browser.execute('return oldUsage.signal.aborted;')
            browser.execute('oldUsage.send("result",usageFixture);')
            assert browser.execute('return !document.getElementById("service_usage") && !document.getElementById("modal_backdrop").classList.contains("hidden");')
            browser.execute('closeModal();')
            checks.append(language+': owner query, indeterminate/percentage progress, retry, partial totals and cancellation preserve the next dialog')
        browser.execute('document.querySelector("[data-i18n=statistics]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-stats-group]");'))
        browser.execute('const group=document.querySelector("[data-stats-group]");group.value="models";group.dispatchEvent(new Event("change",{bubbles:true}));')
        assert browser.execute('return document.querySelector(".presentation-chart-legend").textContent.includes("fixture") && !document.querySelector(".presentation-chart-legend").textContent.includes("ALIAS_FIXTURE");')
        browser.execute('document.querySelector("[data-stats-view=models]").click();')
        assert browser.execute('return document.querySelector("[data-stats-content] tbody").textContent.includes("fixture") && !document.querySelector("[data-stats-content]").textContent.includes("ALIAS_FIXTURE");')
        checks.append('Service usage, model totals and chart legends use upstream names despite catalog aliases')
        browser.execute('closeModal();state.catalog_models.push({id:state.models[0].id,display_name:"ROUTE_ALIAS_FIXTURE"});callReports.open();')
        assert browser.execute('return !document.querySelector("[data-call-filter=model]").textContent.includes("ROUTE_ALIAS_FIXTURE");')
        wait_for(lambda: browser.execute('return document.querySelectorAll(".call-record").length===2;'))
        for request_id, expected in [('unknown-price','—'),('known-zero','US$0')]:
            browser.execute('document.querySelector(arguments[0]+" > summary").click();', '[data-call-id="'+request_id+':responses"]')
            assert browser.execute('const row=document.querySelector(arguments[0]);const field=[...row.querySelectorAll(".call-fact")].find(node=>node.textContent.includes(tr("reports.api_estimate_usd")));return field?.textContent.includes(arguments[1]);', '[data-call-id="'+request_id+':responses"]', expected)
        checks.append('Calls distinguish an unknown zero placeholder from an explicitly priced zero')
        assert upstream.calls == 0
    result = {'passed':True,'checks':checks,'real_model_requests':0}
    args.output.write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
