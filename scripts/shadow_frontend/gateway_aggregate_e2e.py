"""Real-browser gateway and aggregation flows against a disposable EMP."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from e2e import Browser
from candidate_web import candidate_frontend
from local_backend import request, start_backend, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--web',type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    checks = []
    with contextlib.ExitStack() as stack:
        root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-gateway-browser-')))
        config = {'providers':[{'id':name,'name':name,'base_url':'http://127.0.0.1:9/v1',
            'protocol':'chat_completions','auth_mode':'api_key','api_key':'fixture-only'} for name in ['first','second']],
            'models':[{'id':name+'/'+alias,'provider':name,'upstream_id':family,
                'context_window':100000,'enabled':True} for alias,family in [('alias','shared-upstream'),('other','other-upstream')] for name in ['first','second']],
            'catalog_family_presentations':{'shared-upstream':{'catalog_alias':'Shared upstream'}},
            'codex_base_url':'http://127.0.0.1:9/v1','auto_enable_on_start':False}
        backend,url,_,_ = start_backend(stack,args.binary.resolve(),root,config)
        if args.web:
            frontend,_ = candidate_frontend(stack,backend,args.web)
            url = url.replace(backend,frontend)
        browser = Browser(stack,root)
        browser.navigate(url)
        def click(selector):
            wait_for(lambda: browser.execute('return !!document.querySelector(arguments[0]);',selector))
            browser.execute('document.querySelector(arguments[0]).click();',selector)
        def screenshot(name):
            status,data = request(browser.base+'/screenshot')
            assert status == 200,data
            (args.output/(name+'.png')).write_bytes(base64.b64decode(data['value']))
        for language in ['zh-CN','en']:
            browser.execute('closeModal();setLanguage(arguments[0]);',language)
            click('#catalog_aggregate_open')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-add]");'))
            assert browser.execute('return document.getElementById("modal_back").hidden && !document.getElementById("modal_close").hidden;')
            click('[data-aggregate-add]')
            wait_for(lambda: browser.execute('return !!document.getElementById("aggregate_family");'))
            assert browser.execute('return !document.getElementById("modal_back").hidden;')
            click('#modal_back')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-add]");'))
            click('#modal_close')
            checks.append(language+': home service-toolbar action reaches aggregate creation in two clicks; editor Back returns to list and root Close exits')
            click('button[onclick="openSettings()"]')
            click('[data-settings-category="general"]')
            click('[data-gateway-open]')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-gateway-devices]");'))
            screenshot('gateway-home-'+language)
            request(browser.base+'/window/rect',{'width':420,'height':850})
            screenshot('gateway-mobile-'+language)
            request(browser.base+'/window/rect',{'width':1280,'height':900})
            click('[data-gateway-devices]')
            click('[data-gateway-add]')
            browser.execute('document.querySelector("#gateway_client_name").value=arguments[0];','Test PC '+language)
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector("#gateway_created_key");'))
            secret = browser.execute('return document.querySelector("#gateway_created_key").value;')
            assert secret.startswith('emp-client-')
            screenshot('gateway-key-'+language)
            click('#modal_back')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-gateway-remove]");'))
            assert secret not in browser.execute('return document.querySelector("#modal_body").innerHTML;')
            click('[data-gateway-view]')
            assert browser.execute('return document.querySelector("#gateway_created_key").value;') == secret
            click('#modal_back')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-gateway-remove]");'))
            screenshot('gateway-devices-'+language)
            click('#modal_back')
            click('[data-gateway-usage]')
            wait_for(lambda: browser.execute('return !!document.querySelector("#gateway_usage_days");'))
            click('[data-gateway-days="30"]')
            wait_for(lambda: browser.execute('return document.querySelector(`[data-gateway-days="30"]`)?.getAttribute("aria-pressed")==="true";'))
            screenshot('gateway-usage-'+language)
            click('#modal_back')
            click('[data-gateway-devices]')
            click('[data-gateway-remove]')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !document.querySelector("[data-gateway-remove]");'))
            assert browser.api('/api/gateway/clients')['clients'] == []
            checks.append(language+': create, reopen stored key, usage period, back and revoke')
            browser.execute('closeModal();')
            click('button[onclick="openSettings()"]')
            click('[data-settings-category="general"]')
            assert not browser.execute('return !!document.querySelector("[data-aggregate-open]");')
            browser.execute('closeModal();')
            click('#catalog_aggregate_open')
            click('[data-aggregate-add]')
            wait_for(lambda: browser.execute('return !!document.querySelector("#aggregate_prefix");'))
            browser.execute('document.querySelector("#aggregate_prefix").value="组";')
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="other-upstream";family.dispatchEvent(new Event("change"));')
            entered_prefix=browser.execute('return document.querySelector("#aggregate_prefix").value;')
            assert entered_prefix == '组',entered_prefix
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="shared-upstream";family.dispatchEvent(new Event("change"));')
            click('#aggregate_policy input[value="round_robin"]')
            click('[data-source-down]')
            screenshot('aggregation-edit-'+language)
            browser.execute('document.querySelector("#aggregate_policy .shadow-help").focus();')
            wait_for(lambda: browser.execute('return !!document.querySelector(".shadow-feedback:not([hidden])");'))
            screenshot('aggregation-help-'+language)
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-edit]");'))
            saved = browser.api('/api/config')['model_groups'][0]
            assert saved['id'].startswith('auto/') and saved['name_prefix']=='组',saved
            assert saved['policy'] == 'round_robin' and saved['sources'] == ['second/alias','first/alias'],saved
            catalog = browser.api('/api/config')
            model = next(model for model in catalog['catalog_models'] if model['id'] == saved['id'])
            assert model['source_type'] == 'aggregate' and model['family_id'] == 'shared-upstream',model
            family = next(item for item in catalog['catalog_families'] if any(route['id']==saved['id'] for route in item['routes']))
            assert family['id']==saved['id'] and [route['id'] for route in family['routes']]==[saved['id']],family
            original_name=next(family['display_name'] for family in catalog['catalog_families'] if family['id']=='shared-upstream')
            assert model['display_name'].endswith('组 · '+original_name),model
            assert browser.execute('return [...document.querySelectorAll("[data-ui-action=catalog-display-editor]")].find(button=>button.dataset.id===arguments[0]).closest(".display-row").querySelector("strong").textContent;', family['id']).endswith('组 · '+original_name)
            for original in catalog['catalog_models']:
                if original['id'] in saved['sources']:
                    assert '组' not in original['display_name'],original
            browser.execute('const script=document.createElement("script");script.textContent=\'window.originalSources=state.model_groups[0].sources.slice();state.model_groups[0].sources.push("service-deadbeef/unavailable-model");\';document.head.append(script);script.remove();')
            click('[data-aggregate-edit="0"]')
            unavailable=browser.execute('return document.querySelector("#aggregate_sources").textContent;')
            assert 'unavailable-model' in unavailable and 'service-deadbeef' not in unavailable,unavailable
            browser.execute('const script=document.createElement("script");script.textContent="state.model_groups[0].sources=window.originalSources;delete window.originalSources;";document.head.append(script);script.remove();')
            click('#modal_back')
            click('[data-aggregate-add]')
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="shared-upstream";family.dispatchEvent(new Event("change"));')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector(`[data-aggregate-edit="1"]`);'))
            groups=browser.api('/api/config')['model_groups']
            assert len(groups)==2 and groups[0]['id']!=groups[1]['id'],groups
            assert groups[1]['name_prefix']=='auto',groups[1]
            default_model=next(model for model in browser.api('/api/config')['catalog_models'] if model['id']==groups[1]['id'])
            assert default_model['display_name'].endswith('auto · '+original_name),default_model
            duplicate = {**groups[1], 'id':'auto/duplicate', 'policy':'round_robin',
                         'sources':list(reversed(groups[1]['sources']))}
            before = (root/'config.json').read_bytes()
            status,result = request(backend+'/api/config',
                {**browser.api('/api/config'),'model_groups':groups+[duplicate]},
                {'Origin':backend,'X-EMP-Session':browser.execute('return localStorage.getItem("emp_management_session_v1");')})
            assert status == 400, ('duplicate model and prefix were accepted',status,result.get('error'))
            assert (root/'config.json').read_bytes() == before
            assert browser.api('/api/config')['model_groups'] == groups
            click('[data-aggregate-add]')
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="shared-upstream";family.dispatchEvent(new Event("change"));')
            click('#aggregate_policy input[value="round_robin"]')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector("#modal_status").textContent && !document.querySelector("#modal_submit").disabled;'))
            assert browser.execute('return document.querySelector("#aggregate_prefix").value;') == 'auto'
            assert browser.api('/api/config')['model_groups'] == groups
            assert (root/'config.json').read_bytes() == before
            screenshot('aggregation-duplicate-'+language)
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="other-upstream";family.dispatchEvent(new Event("change"));')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector(`[data-aggregate-edit="2"]`);'))
            different_model = browser.api('/api/config')['model_groups'][2]
            assert different_model['name_prefix'] == 'auto'
            assert different_model['sources'] == ['first/other','second/other']
            click('[data-aggregate-edit="2"]')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector(`[data-aggregate-edit="2"]`);'))
            assert browser.api('/api/config')['model_groups'][2] == different_model
            click('[data-aggregate-edit="2"]')
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="shared-upstream";family.dispatchEvent(new Event("change"));')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector("#modal_status").textContent && !document.querySelector("#modal_submit").disabled;'))
            assert browser.api('/api/config')['model_groups'][2] == different_model
            browser.execute('document.querySelector("#aggregate_prefix").value="Priority";')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector(`[data-aggregate-edit="2"]`);'))
            different_prefix = browser.api('/api/config')['model_groups'][2]
            assert different_prefix['id'] == different_model['id']
            assert different_prefix['sources'] == ['first/alias','second/alias']
            assert different_prefix['name_prefix'] == 'Priority'
            click('[data-aggregate-remove="2"]')
            wait_for(lambda: browser.execute('return !document.querySelector(`[data-aggregate-edit="2"]`);'))
            click('[data-aggregate-edit="1"]')
            browser.execute('const prefix=document.querySelector("#aggregate_prefix");prefix.value="";prefix.dispatchEvent(new Event("input"));')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector(`[data-aggregate-edit="1"]`);'))
            empty_model=next(model for model in browser.api('/api/config')['catalog_models'] if model['id']==groups[1]['id'])
            assert empty_model['display_name']==default_model['display_name'].replace('auto · ','',1),empty_model
            click('[data-aggregate-remove="1"]')
            wait_for(lambda: browser.execute('return !document.querySelector(`[data-aggregate-edit="1"]`);'))
            click('[data-aggregate-edit="0"]')
            assert browser.execute('return document.querySelector("#aggregate_prefix").value;') == '组'
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="other-upstream";family.dispatchEvent(new Event("change"));')
            assert browser.execute('return [...document.querySelectorAll("[data-aggregate-source]")].map(row=>row.dataset.aggregateSource);') == ['first/other','second/other']
            browser.execute('const family=document.querySelector("#aggregate_family");family.value="shared-upstream";family.dispatchEvent(new Event("change"));document.querySelector("#aggregate_prefix").value="精选";')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-edit]");'))
            renamed=browser.api('/api/config')['model_groups'][0]
            assert renamed['id']==saved['id'] and renamed['name_prefix']=='精选',renamed
            assert renamed['sources']==saved['sources'],renamed
            updated=browser.api('/api/config')
            updated_name='Updated shared '+language
            updated['catalog_family_presentations']['shared-upstream']={'catalog_alias':updated_name}
            status,response=request(browser.base+'/execute/async',{'script':
                'const done=arguments[arguments.length-1];persistState("",arguments[0]).then(()=>done({saved:true}),e=>done({error:e.message}));',
                'args':[updated]})
            assert status==200 and response['value'].get('saved'),response
            after_rename=browser.api('/api/config')
            current=next(model for model in after_rename['catalog_models'] if model['id']==saved['id'])
            assert current['display_name'].endswith('精选 · '+updated_name),current
            browser.execute('closeModal();')
            browser.execute('[...document.querySelectorAll("[data-ui-action=catalog-display-editor]")].find(button=>button.dataset.id===arguments[0]).click();', family['id'])
            assert browser.execute('return document.querySelector("#modal_body code").textContent;') == 'second · first'
            browser.execute('document.querySelector("#modal_catalog_alias").value="Aggregate only";')
            click('#modal_submit')
            wait_for(lambda: browser.execute('return document.querySelector("#modal_backdrop").classList.contains("hidden");'))
            aggregate_alias=browser.api('/api/config')
            assert aggregate_alias['catalog_presentations'][saved['id']]['catalog_alias']=='Aggregate only'
            assert next(item for item in aggregate_alias['catalog_models'] if item['id']==saved['id'])['display_name'].endswith('Aggregate only')
            assert next(item for item in aggregate_alias['catalog_families'] if item['id']=='shared-upstream')['display_name']==updated_name
            browser.execute('[...document.querySelectorAll("[data-ui-action=catalog-display-editor]")].find(button=>button.dataset.id===arguments[0]).click();document.querySelector("#modal_catalog_alias").value="";', family['id'])
            click('#modal_submit')
            wait_for(lambda: browser.execute('return document.querySelector("#modal_backdrop").classList.contains("hidden");'))
            assert next(item for item in browser.api('/api/config')['catalog_models'] if item['id']==saved['id'])['display_name'].endswith('精选 · '+updated_name)
            click('#catalog_aggregate_open')
            click('[data-aggregate-edit]')
            for policy in ('priority','balanced','round_robin'):
                click(f'#aggregate_policy input[value="{policy}"]')
                click('#modal_submit')
                if policy == 'balanced':
                    wait_for(lambda: browser.execute('return !!document.querySelector("#modal_status").textContent;'))
                    assert browser.api('/api/config')['model_groups'][0]['policy'] == 'priority'
                    continue
                wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-edit]");'))
                assert browser.api('/api/config')['model_groups'][0]['policy'] == policy
                click('[data-aggregate-edit]')
                assert browser.execute('return document.querySelector("#aggregate_policy input:checked").value;') == policy
            browser.execute('setTheme("dark");')
            screenshot('aggregation-dark-'+language)
            browser.execute('setTheme("light");')
            request(browser.base+'/window/rect',{'width':420,'height':850})
            screenshot('aggregation-mobile-'+language)
            request(browser.base+'/window/rect',{'width':1280,'height':900})
            click('#modal_back')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-edit]");'))
            screenshot('aggregation-list-'+language)
            click('[data-aggregate-remove]')
            wait_for(lambda: browser.execute('return !!document.querySelector("[data-aggregate-add]");'))
            assert browser.api('/api/config')['model_groups'] == []
            checks.append(language+': automatic unique IDs, default/custom/empty prefixes, reject duplicate model/prefix on create and edit without saving, allow shared prefixes across models and distinct prefixes on one model, preserve prefix and source family on model change, unchanged original names, rename propagation, stable IDs, policies and delete')
        assert not browser.diagnostics(),browser.diagnostics()
    report = {'passed':True,'real_provider_calls':0,'checks':checks}
    (args.output/'report.json').write_text(json.dumps(report,indent=2))
    print(json.dumps(report))


if __name__ == '__main__':
    main()
