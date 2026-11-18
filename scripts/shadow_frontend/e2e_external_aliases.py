"""Readable catalog IDs preserve canonical routing, billing and resumed sessions."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import shutil
import sys
import tempfile
import urllib.request
from urllib.parse import quote
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from scripts.codex_feature_acceptance import fixture as fixtures
from e2e_request_modes import websocket_request
from local_backend import request, wait_for


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp',type=Path,required=True)
    parser.add_argument('--codex',type=Path,default=Path(shutil.which('codex')))
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    checks=[]
    def post(handler):
        raw=handler.rfile.read(int(handler.headers['Content-Length']))
        if handler.headers.get('Content-Encoding')=='zstd':
            from compression import zstd
            raw=zstd.decompress(raw)
        body=json.loads(raw)
        identity='alias-response-'+str(next(handler.server.sequence))
        handler.server.requests.append({'path':handler.path,'body':body,'headers':dict(handler.headers)})
        item=fixtures.message();item['id']='message-'+identity
        if body.get('stream'):
            payload=b''.join(b'data: '+json.dumps(event).encode()+b'\n\n' for event in fixtures.response_events(item,identity))
            content_type='text/event-stream'
        else:
            payload=json.dumps({'id':identity,'object':'response','status':'completed','model':body['model'],
                'output':[item],'usage':{'input_tokens':10,'output_tokens':3,'total_tokens':13}}).encode()
            content_type='application/json'
        handler.send_response(200);handler.send_header('Content-Type',content_type)
        handler.send_header('Content-Length',str(len(payload)));handler.end_headers();handler.wfile.write(payload)
    with contextlib.ExitStack() as stack:
        root=Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='emp-readable-routes-')))
        stack.enter_context(patch.object(fixtures.Upstream,'do_POST',post))
        fixture=stack.enter_context(fixtures.Fixture(args.codex.resolve(),args.emp.resolve(),root/'fixture',root/'evidence'))
        fixture.config=fixture.config.replace('supports_websockets=false','supports_websockets=true').replace(
            'requires_openai_auth=false\nenv_key="EMP_TEST_KEY"','requires_openai_auth=true')
        (fixture.home/'config.toml').write_text(fixture.config)
        def generations():
            return [record for record in fixture.upstream.requests if record['body'].get('generate') is not False]
        status,config=request(fixture.base+'/api/config',headers=fixture.headers);assert status==200
        endpoint=config['providers'][0]['base_url']
        canonical='service-1234abcd/vendor/deepseek';alias='床/vendor/deepseek';wire_alias='%E5%BA%8A/vendor/deepseek'
        config['providers'].append({'id':'service-1234abcd','name':'床','base_url':endpoint,'protocol':'responses','auth_mode':'api_key','api_key':'fixture-only'})
        config['models'].append({'id':canonical,'provider':'service-1234abcd','upstream_id':'vendor/deepseek','context_window':100000})
        status,saved=request(fixture.base+'/api/config',config,fixture.headers);assert status==200,saved
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        assert status==200 and wire_alias in [model['slug'] for model in catalog['models']]
        named=next(model for model in catalog['models'] if model['slug']==wire_alias)
        assert named['display_name'].endswith('床 · vendor/deepseek'),named
        assert canonical not in [model['slug'] for model in catalog['models']]
        assert canonical in [model['id'] for model in saved['catalog_models']]
        for model in [canonical,alias,wire_alias]:
            status,detail=request(fixture.base+'/v1/models/'+quote(model,safe=''),headers=fixture.headers)
            assert status==200 and detail['id']==model,detail
        status,refreshed=request(fixture.base+'/api/catalog/refresh',{},fixture.headers);assert status==200,refreshed
        generated=json.loads(Path(refreshed['catalog_path']).read_text())
        assert wire_alias in [model['slug'] for model in generated['models']]
        assert all(model['slug'].isascii() for model in generated['models'])
        checks.append('Wire model IDs are ASCII-safe, Chinese display names remain readable, and management retains canonical IDs')
        opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
        headers={**fixture.headers,'Authorization':'Bearer fixture-only','Content-Type':'application/json'}
        for model in [canonical,alias,wire_alias]:
            for transport in ['http','sse','websocket']:
                body={'model':model,'stream':transport!='http','input':[{'role':'user','content':'ALIAS_CHECK'}]}
                before=len(generations())
                if transport=='websocket':websocket_request(fixture.base,headers,body)
                else:
                    call=urllib.request.Request(fixture.base+'/v1/responses',json.dumps(body).encode(),headers)
                    with opener.open(call,timeout=20) as response:
                        payload=response.read();assert response.status==200 and b'FIXTURE_COMPLETE' in payload,payload
                assert len(generations())==before+1
                assert generations()[-1]['body']['model']=='vendor/deepseek'
        checks.append('Canonical, legacy Chinese and encoded IDs each complete HTTP, SSE and WebSocket without duplicate upstream calls')
        engine_catalog={**catalog,'models':[model for model in catalog['models'] if '/' in model['slug']]
            +[model for model in fixture.catalog['models'] if '/' not in model['slug']]}
        (fixture.home/'models.json').write_text(json.dumps(engine_catalog))
        client=fixture.client()
        started=client.rpc('thread/start',{'model':wire_alias,'cwd':str(fixture.home)})
        assert started['model']==wire_alias,started
        thread=started['thread']['id'];fixtures.turn(client,thread,'ALIAS_ENGINE_CHECK')
        status,config=request(fixture.base+'/api/config',headers=fixture.headers)
        next(provider for provider in config['providers'] if provider['id']=='service-1234abcd')['name']='新服务'
        status,saved=request(fixture.base+'/api/config',config,fixture.headers);assert status==200,saved
        status,current=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        assert '%E6%96%B0%E6%9C%8D%E5%8A%A1/vendor/deepseek' in [model['slug'] for model in current['models']]
        fixtures.turn(client,thread,'ALIAS_AFTER_RENAME')
        assert generations()[-1]['body']['model']=='vendor/deepseek'
        checks.append('The installed Codex selects the Unicode service name and continues its original thread after service rename')
        calls=wait_for(lambda:(value if (value:=request(fixture.base+'/api/calls?limit=100',headers=fixture.headers)[1])['total']==11 else None))
        assert all(row['model_id']==canonical and row['state']=='completed' for row in calls['records']),calls
        assert len(generations())==11
        checks.append('Eleven calls share the original canonical billing model and execute exactly once')
        fixture.import_account()
        auth=json.loads((fixture.home/'auth.json').read_text())
        auth['tokens']['account_id']='imported-fixture-account'
        auth['tokens']['access_token']='imported-fixture-only'
        claims=json.loads(base64.urlsafe_b64decode(auth['tokens']['id_token'].split('.')[1]+'=='))
        claims['email']='imported@example.test'
        claims['https://api.openai.com/auth']['chatgpt_account_id']='imported-fixture-account'
        encoded=base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=')
        auth['tokens']['id_token']='eyJhbGciOiJub25lIn0.'+encoded+'.c2ln'
        status,result=request(fixture.base+'/api/accounts/import',{'name':'工作 fixture','auth_json':auth},fixture.headers)
        assert status==200,result
        status,config=request(fixture.base+'/api/config',headers=fixture.headers)
        assert len(config['accounts'])==1
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        subscription_alias='%E5%B7%A5%E4%BD%9C fixture/gpt-6.1-sol'
        assert status==200 and any(model['slug']==subscription_alias for model in catalog['models']),catalog
        assert not any(model['slug']=='imported/gpt-6.1-sol' for model in catalog['models'])
        for model in [subscription_alias,'imported/gpt-6.1-sol']:
            status,detail=request(fixture.base+'/v1/models/'+quote(model,safe=''),headers=fixture.headers)
            assert status==200 and detail['id']==model,detail
        auth=json.loads((fixture.home/'auth.json').read_text())
        duplicate={'id':'different-account','prefix':'different-account','name':' imported FIXTURE ',
                   'auth_json':auth,'enabled':True}
        status,error=request(fixture.base+'/api/accounts/import',duplicate,fixture.headers)
        assert status==400 and error['error']['message']=='core.service_name_already_in_use',error
        duplicate['name']='Invalid/Name'
        status,error=request(fixture.base+'/api/accounts/import',duplicate,fixture.headers)
        assert status==400 and error['error']['message']=='core.service_name_contains_slash',error
        status,config=request(fixture.base+'/api/config',headers=fixture.headers)
        assert len(config['accounts'])==1
        for field in ['providers','accounts']:
            candidate=json.loads(json.dumps(config))
            candidate[field][0]['name']='Invalid/Name'
            status,error=request(fixture.base+'/api/config',candidate,fixture.headers)
            assert status==400 and error['error']['message']=='core.service_name_contains_slash',error
        candidate=json.loads(json.dumps(config))
        candidate['providers'].append({'id':'historical-conflict','name':'New service','route_names':['床'],'base_url':endpoint,'protocol':'responses','auth_mode':'api_key'})
        status,error=request(fixture.base+'/api/config',candidate,fixture.headers)
        assert status==400 and error['error']['message']=='core.service_name_already_in_use',error
        checks.append('Account/provider names reject slashes and new services cannot take existing historical names')
        config['providers'].append({'id':'different-provider','name':'工作 fixture','base_url':endpoint,'protocol':'responses','auth_mode':'api_key'})
        status,error=request(fixture.base+'/api/config',config,fixture.headers)
        assert status==400 and error['error']['message']=='core.service_name_already_in_use',error
        template=next(model for model in fixture.catalog['models'] if model['slug']=='responses/probe')
        catalog['models']=[{**template,**model,'apply_patch_tool_type':'freeform'} for model in catalog['models']]
        (fixture.home/'models.json').write_text(json.dumps(catalog))
        subscription_client=fixture.client()
        models=subscription_client.rpc('model/list',{'includeHidden':True})['data']
        assert any(model['id']==subscription_alias for model in models),models
        subscription_thread=subscription_client.rpc('thread/start',{'model':subscription_alias,'cwd':str(fixture.home)})['thread']['id']
        fixtures.turn(subscription_client,subscription_thread,'SUBSCRIPTION_ROUTE_CHECK')
        forwarded=generations()[-1]
        assert forwarded['body']['model']=='gpt-6.1-sol'
        assert forwarded['transport']=='websocket',forwarded
        assert any(value=='model=gpt-6.1-sol' for key,value in forwarded['headers'].items() if key.lower()=='x-codex-routing-hint'),forwarded
        assert any(value=='Bearer imported-fixture-only' for key,value in forwarded['headers'].items() if key.lower()=='authorization')
        status,config=request(fixture.base+'/api/config',headers=fixture.headers)
        config['accounts'][0]['name']='新账号'
        status,saved=request(fixture.base+'/api/config',config,fixture.headers);assert status==200,saved
        fixtures.turn(subscription_client,subscription_thread,'SUBSCRIPTION_AFTER_RENAME')
        assert generations()[-1]['body']['model']=='gpt-6.1-sol'
        status,detail=request(fixture.base+'/v1/models/'+quote(subscription_alias,safe=''),headers=fixture.headers)
        assert status==200 and detail['id']==subscription_alias,detail
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        assert any(model['slug']=='%E6%96%B0%E8%B4%A6%E5%8F%B7/gpt-6.1-sol' for model in catalog['models'])
        checks.append('Subscription names appear in the installed Codex; duplicate names are rejected and existing conversations survive renames')
        legacy_client=fixture.client()
        legacy_thread=legacy_client.rpc('thread/start',{'model':'工作 fixture/gpt-6.1-sol','cwd':str(fixture.home)})['thread']['id']
        fixtures.turn(legacy_client,legacy_thread,'LEGACY_ROUTING_HINT_CHECK')
        legacy_client.close()
        restored_client=fixture.client()
        restored=restored_client.rpc('thread/resume',{'threadId':legacy_thread,'model':subscription_alias})
        assert restored['thread']['id']==legacy_thread,restored
        fixtures.turn(restored_client,legacy_thread,'RESTORED_WEBSOCKET_CHECK')
        assert generations()[-1]['transport']=='websocket',generations()[-1]
        assert generations()[-1]['body']['model']=='gpt-6.1-sol'
        history=restored_client.rpc('thread/read',{'threadId':legacy_thread,'includeTurns':True})['thread']
        history_text=json.dumps(history)
        assert 'LEGACY_ROUTING_HINT_CHECK' in history_text and 'RESTORED_WEBSOCKET_CHECK' in history_text
        checks.append('A legacy Unicode thread resumes under the same ID over WebSocket after restarting its Codex runtime')
        status,config=request(fixture.base+'/api/config',headers=fixture.headers);assert status==200
        config['catalog_family_presentations']={'gpt-6.1-sol':{'catalog_alias':'⭐'}}
        config['catalog_show_context']=False
        config['model_groups']=[{'id':'auto/stable','name_prefix':'auto222','policy':'priority',
            'sources':['gpt-6.1-sol','imported/gpt-6.1-sol']}]
        status,saved=request(fixture.base+'/api/config',config,fixture.headers);assert status==200,saved
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        aggregate='auto222 %C2%B7 %E2%AD%90'
        assert status==200 and any(model['slug']==aggregate and model['display_name']=='auto222 · ⭐' for model in catalog['models'])
        template=next(model for model in fixture.catalog['models'] if model['slug']=='responses/probe')
        catalog['models']=[{**template,**model,'apply_patch_tool_type':'freeform'} for model in catalog['models']]
        (fixture.home/'models.json').write_text(json.dumps(catalog))
        aggregate_client=fixture.client()
        models=aggregate_client.rpc('model/list',{'includeHidden':True})['data']
        assert any(model['id']==aggregate and model['displayName']=='auto222 · ⭐' for model in models),models
        thread=aggregate_client.rpc('thread/start',{'model':aggregate,'cwd':str(fixture.home)})['thread']['id']
        fixtures.turn(aggregate_client,thread,'AGGREGATE_ROUTE_CHECK')
        assert generations()[-1]['body']['model']=='gpt-6.1-sol'
        assert generations()[-1]['transport']=='websocket'
        status,config=request(fixture.base+'/api/config',headers=fixture.headers);assert status==200
        config['model_groups'][0]['name_prefix']='auto333'
        status,saved=request(fixture.base+'/api/config',config,fixture.headers);assert status==200,saved
        assert 'auto222 · ⭐' in saved['model_groups'][0]['route_names']
        status,detail=request(fixture.base+'/v1/models/'+quote(aggregate,safe=''),headers=fixture.headers)
        assert status==200 and detail['id']==aggregate,detail
        fixtures.turn(aggregate_client,thread,'AGGREGATE_AFTER_RENAME')
        assert generations()[-1]['body']['model']=='gpt-6.1-sol'
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        renamed='auto333 %C2%B7 %E2%AD%90'
        assert status==200 and any(model['slug']==renamed for model in catalog['models'])
        catalog['models']=[{**template,**model,'apply_patch_tool_type':'freeform'} for model in catalog['models']]
        (fixture.home/'models.json').write_text(json.dumps(catalog))
        next_client=fixture.client()
        next_thread=next_client.rpc('thread/start',{'model':renamed,'cwd':str(fixture.home)})['thread']['id']
        fixtures.turn(next_client,next_thread,'AGGREGATE_RENAMED_ROUTE_CHECK')
        assert generations()[-1]['body']['model']=='gpt-6.1-sol'
        assert len(generations())==18
        checks.append('Aggregate display names route correctly, and an existing Codex thread continues after the prefix changes')
        status,result=request(fixture.base+'/api/native/login/use-local-key',{'account_action':'keep'},fixture.headers)
        assert status==200,result
        status,catalog=request(fixture.base+'/v1/models?client_version='+fixture.codex_version,headers=fixture.headers)
        assert status==200,catalog
        assert next(model for model in catalog['models'] if model['slug']=='gpt-6.1-sol')['visibility']=='hide'
        assert any(model['slug']==renamed and model.get('visibility','list')=='list' for model in catalog['models'])
        catalog['models']=[{**template,**model,'apply_patch_tool_type':'freeform'} for model in catalog['models']]
        (fixture.home/'models.json').write_text(json.dumps(catalog))
        (fixture.home/'config.toml').write_text(
            f'model_catalog_json={json.dumps(str(fixture.home/"models.json"))}\n'
            f'openai_base_url={json.dumps(fixture.base+"/v1")}\n'
            'approval_policy="never"\nsandbox_mode="read-only"\n'
            '[features]\nplugins=false\napps=false\nmemories=false\n')
        local_client=fixture.client()
        visible=local_client.rpc('model/list',{'includeHidden':False})['data']
        ids={model['id'] for model in visible}
        assert 'gpt-6.1-sol' not in ids and '%E6%96%B0%E8%B4%A6%E5%8F%B7/gpt-6.1-sol' in ids and renamed in ids,ids
        local_thread=local_client.rpc('thread/start',{'model':renamed,'cwd':str(fixture.home)})['thread']['id']
        before=len(generations())
        fixtures.turn(local_client,local_thread,'LOCAL_KEY_AGGREGATE_CHECK')
        forwarded=generations()[before:]
        assert len([record for record in forwarded if record['body'].get('generate') is not False])==1, forwarded
        assert all(record['body']['model']=='gpt-6.1-sol' for record in forwarded), forwarded
        checks.append('Local Key hides unavailable Native in the installed Codex picker; Other and aggregate remain selectable and aggregate completes on Other')
        report={'passed':True,'codex_version':fixture.codex_version,'real_provider_calls':0,'requests':len(generations()),'websocket_warmups':len(fixture.upstream.requests)-len(generations()),'checks':checks}
    (args.output/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))


if __name__=='__main__':main()
