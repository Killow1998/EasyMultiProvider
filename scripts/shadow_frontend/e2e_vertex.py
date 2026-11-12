"""Exercise the Vertex service form and credential changes with synthetic data."""
import argparse
import base64
import contextlib
import json
import tempfile
from pathlib import Path

from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--screenshot', type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='emp-vertex-ui-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        base, url, state, headers, _ = start_backend(stack, args.emp.resolve(), root, env_overrides={
            'HTTP_PROXY':'http://127.0.0.1:9', 'HTTPS_PROXY':'http://127.0.0.1:9',
            'ALL_PROXY':'http://127.0.0.1:9', 'NO_PROXY':'127.0.0.1,localhost', 'PATH':'/usr/bin:/bin',
        })
        headers['Origin'] = base
        browser = Browser(stack, root)
        browser.navigate(url)
        headers['X-EMP-Session'] = browser.execute('return localStorage.getItem("emp_management_session_v1");')

        browser.execute('openServiceChooser();document.querySelector("[data-ui-action=official-provider-choose][data-provider=zhipu]").click();document.getElementById("modal_provider_key").value="synthetic-glm-key";document.getElementById("modal_submit").click();')
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        glm=next(provider for provider in request(base+'/api/config',headers=headers)[1]['providers'] if provider['id']=='zhipu-glm')
        assert '/' not in glm['name'] and glm['api_key_set'],glm

        browser.execute('openServiceChooser();')
        assert browser.execute('return !!document.querySelector("[data-ui-action=vertex-provider-choose]");')
        browser.execute('document.querySelector("[data-ui-action=vertex-provider-choose]").click();')
        assert browser.execute('return document.getElementById("modal_vertex_auth").value==="vertex_api_key" && document.getElementById("modal_vertex_project").value==="";')
        assert browser.execute('return document.getElementById("modal_vertex_location_group").hidden;')
        if args.screenshot:
            status, image = request(browser.base+'/screenshot')
            assert status == 200
            args.screenshot.write_bytes(base64.b64decode(image['value']))
        browser.execute('const mode=document.getElementById("modal_vertex_auth");mode.value="vertex_service_account";mode.dispatchEvent(new Event("change"));const text=document.getElementById("modal_vertex_json");text.value=JSON.stringify({project_id:"fixture-autofill"});text.dispatchEvent(new Event("input"));')
        assert browser.execute('return document.getElementById("modal_vertex_project").value==="fixture-autofill";')
        browser.execute('''
          window.originalVertexFileText=File.prototype.text;
          window.vertexFileErrors=[];
          window.addEventListener('unhandledrejection',event=>vertexFileErrors.push(String(event.reason)));
          File.prototype.text=function(){return new Promise(resolve=>window.finishVertexFileRead=resolve);};
        ''')
        file_content=json.dumps({'project_id':'file-project','type':'service_account','client_email':'fixture@example.invalid','private_key':'fixture-only'})
        for destination in ['another-form','closed','same-form']:
            browser.execute('''
              closeModal();openVertexProvider();
              const mode=document.getElementById('modal_vertex_auth');mode.value='vertex_service_account';mode.dispatchEvent(new Event('change'));
              const transfer=new DataTransfer();transfer.items.add(new File(['fixture'],'service-account.json',{type:'application/json'}));
              const input=document.getElementById('modal_vertex_file');input.files=transfer.files;input.dispatchEvent(new Event('change'));
            ''')
            if destination!='same-form':
                browser.execute('closeModal();')
            if destination=='another-form':
                browser.execute('openVertexProvider();document.getElementById("modal_vertex_project").value="keep-project";document.getElementById("modal_vertex_json").value="keep-draft";')
            browser.execute('window.finishVertexFileRead(arguments[0]);',file_content)
            assert browser.execute('return vertexFileErrors.length===0;')
            if destination=='another-form':
                assert browser.execute('return document.getElementById("modal_vertex_json").value==="keep-draft" && document.getElementById("modal_vertex_project").value==="keep-project";')
            elif destination=='same-form':
                assert browser.execute('return document.getElementById("modal_vertex_json").value===arguments[0] && document.getElementById("modal_vertex_project").value==="file-project";',file_content)
        browser.execute('File.prototype.text=originalVertexFileText;closeModal();openVertexProvider();')
        browser.execute('const mode=document.getElementById("modal_vertex_auth");mode.value="vertex_api_key";mode.dispatchEvent(new Event("change"));document.getElementById("modal_vertex_project").value="";')
        browser.execute('document.getElementById("modal_vertex_api_key").value="synthetic-vertex-key";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        status, config = request(base+'/api/config', headers=headers)
        assert status == 200, (status, config)
        provider = next(row for row in config['providers'] if row.get('vertex'))
        provider_id = provider['id']
        assert provider['api_key_set'] and provider['auth_mode'] == 'vertex_api_key'
        assert 'synthetic-vertex-key' not in json.dumps(config)
        browser.execute('providerModelSettings.open(arguments[0]);', provider_id)
        browser.execute('document.querySelector("[data-manager-action=refresh]").click();')
        wait_for(lambda: browser.execute('return document.querySelectorAll("[data-managed-model]").length>0;'))
        status, models = request(base+f'/api/providers/{provider_id}/models', headers=headers)
        assert status == 200 and any(row['upstream_id'].startswith('gemini-') for row in models['models']), (status, models)

        browser.execute('closeModal();')
        browser.execute('openVertexProvider(state.providers.find(provider=>provider.id===arguments[0]));', provider_id)
        browser.execute('document.getElementById("modal_vertex_project").value="fixture-project";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        config = request(base+'/api/config', headers=headers)[1]
        provider = next(row for row in config['providers'] if row['id'] == provider_id)
        assert provider['auth_mode'] == 'vertex_api_key' and provider['api_key_set']
        assert provider['vertex']['project'] == 'fixture-project'
        browser.execute('providerModelSettings.open(arguments[0]);', provider_id)
        browser.execute('document.querySelector("[data-manager-action=refresh]").click();')
        wait_for(lambda: browser.execute('return [...document.querySelectorAll("[data-managed-model]")].some(row=>row.textContent.includes("gemini-4-carbon"));'))
        status, models = request(base+f'/api/providers/{provider_id}/models', headers=headers)
        ids = {row['upstream_id'] for row in models['models']}
        assert status == 200 and {'gemini-4-carbon', 'gemini-4-argon', 'gemini-3.8-flash'} <= ids
        assert all(model.startswith('gemini-') and 'image' not in model for model in ids)

        model_id = provider_id + '/gemini-3.5-flash-lite'
        browser.execute('document.querySelector(\'[data-manager-action="toggle"][data-upstream="gemini-3.5-flash-lite"]\').click();')
        wait_for(lambda: browser.execute('return state.models.some(model=>model.id===arguments[0] && model.enabled);', model_id))
        browser.execute('closeModal();document.getElementById("catalog_search_toggle").click();const input=document.getElementById("catalog_display_search");input.value="no-matching-fixture-model";input.dispatchEvent(new Event("input",{bubbles:true}));')
        assert browser.execute('return document.getElementById("catalog_search_toggle").getAttribute("aria-expanded")==="true" && document.querySelectorAll("#catalog_display_models .display-row").length===0;')
        browser.escape()
        assert browser.execute('return document.getElementById("catalog_search_toggle").getAttribute("aria-expanded")==="false" && document.querySelectorAll("#catalog_display_models .display-row").length>0;')
        original_context = config['catalog_show_context']
        for expected in [not original_context, original_context]:
            browser.execute('document.getElementById("catalog_context_toggle").click();')
            wait_for(lambda: browser.execute('return !document.getElementById("catalog_context_toggle").disabled;'))
            assert request(base+'/api/config', headers=headers)[1]['catalog_show_context'] == expected
            assert browser.execute('return document.getElementById("catalog_context_toggle").getAttribute("aria-pressed");') == str(expected).lower()
        browser.execute('''
          window.probeFetch=window.fetch;
          window.fetch=(url,options)=>{
            if(url==='/v1/responses') {
              window.probeRequest=JSON.parse(options.body);
              return new Promise(resolve=>window.finishProbe=()=>resolve(new Response(JSON.stringify({output_text:'fixture'}),{status:200,headers:{'Content-Type':'application/json'}})));
            }
            return window.probeFetch(url,options);
          };
        ''')
        for probe in ['testModel', 'testModelVision', 'testModelAudio']:
            browser.execute('window.probeRequest=null;window[arguments[0]](arguments[1]);', probe, model_id)
            wait_for(lambda: browser.execute('return !!window.probeRequest;'))
            name = browser.execute('return document.getElementById("status").textContent;')
            assert 'Vertex' in name and 'gemini-3.5-flash-lite' in name and provider_id not in name, (probe, name)
            assert browser.execute('return window.probeRequest.model;') == model_id
            browser.execute('window.finishProbe();')
        browser.execute('window.fetch=window.probeFetch;')
        html = browser.execute('return callReports.recordHtml(arguments[0]);', {
            'model_id':model_id, 'selected_name':model_id, 'provider_id':provider_id,
            'request_id':'fixture', 'route':'responses', 'state':'failed',
        })
        assert 'Vertex' in html and 'gemini-3.5-flash-lite' in html and provider_id not in html
        browser.execute('closeModal();')
        browser.execute('openVertexProvider(state.providers.find(provider=>provider.id===arguments[0]));', provider_id)
        browser.execute('document.getElementById("modal_vertex_name").value="Vertex renamed";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        renamed = browser.execute('return modelDisplayName(arguments[0]);', model_id)
        assert 'Vertex renamed' in renamed and provider_id not in renamed, renamed
        unknown = browser.execute('return modelDisplayName("service-deadbeef/legacy-model");')
        assert 'legacy-model' in unknown and 'service-deadbeef' not in unknown, unknown
        browser.execute('openVertexProvider(state.providers.find(provider=>provider.id===arguments[0]));', provider_id)
        browser.execute('const mode=document.getElementById("modal_vertex_auth");mode.value="vertex_adc";mode.dispatchEvent(new Event("change"));document.getElementById("modal_vertex_project").value="fixture-project";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        config = request(base+'/api/config', headers=headers)[1]
        provider = next(row for row in config['providers'] if row['id'] == provider_id)
        assert provider['auth_mode'] == 'vertex_adc' and not provider['api_key_set']

        versioned_id=provider_id+'/claude-sonnet-4@20250514'
        config['models'].append({'id':versioned_id,'provider':provider_id,'upstream_id':'claude-sonnet-4@20250514'})
        status,saved=request(base+'/api/config',config,headers)
        assert status==200,saved
        browser.navigate(url)
        wait_for(lambda:browser.execute('return state.catalog_models.some(model=>model.id===arguments[0]);',versioned_id))
        family=browser.execute('return state.catalog_models.find(model=>model.id===arguments[0]).family_id;',versioned_id)
        assert '@' in family,family
        browser.execute('openCatalogDisplayEditor(arguments[0]);document.getElementById("modal_catalog_alias").value="Versioned Claude";document.getElementById("modal_submit").click();',family)
        wait_for(lambda:browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        assert request(base+'/api/config',headers=headers)[1]['catalog_family_presentations'][family]['catalog_alias']=='Versioned Claude'

        browser.execute('openVertexProvider(state.providers.find(provider=>provider.id===arguments[0]));', provider_id)
        browser.execute('const mode=document.getElementById("modal_vertex_auth");mode.value="vertex_service_account";mode.dispatchEvent(new Event("change"));document.getElementById("modal_vertex_json").value=JSON.stringify({type:"service_account",client_email:"fixture@example.invalid",private_key:"fixture-private-key"});document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        config = request(base+'/api/config', headers=headers)[1]
        provider = next(row for row in config['providers'] if row['id'] == provider_id)
        assert provider['auth_mode'] == 'vertex_service_account' and provider['api_key_set']
        assert 'fixture-private-key' not in json.dumps(config)
        assert 'fixture-private-key' not in (state.parent/'config.json').read_text()

        provider['api_key'] = '{invalid-json'
        status, _ = request(base+'/api/config', config, headers)
        assert status == 400
        assert request(base+'/api/config', headers=headers)[1]['providers'][-1]['api_key_set']
        print('GLM preset creation, Vertex catalogs, readable model names, probe routing, ADC, credential changes, versioned model display saves, delayed credential-file ownership, and invalid JSON rejection: PASS')


if __name__ == '__main__':
    main()
