"""Real OpenAI-provider clients authenticate to EMP without management headers."""
import json

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, turn


def run(codex, emp, root, evidence):
    results = []
    for mode in ['subscription', 'api_key', 'local_key']:
        with Fixture(codex, emp, root/mode, evidence/mode) as fixture:
            if mode != 'subscription':
                operation, body = ('use-api-key', {'api_key':'sk-fixture-openai-not-real'}) if mode == 'api_key' else ('use-local-key', {'account_action':'remove'})
                status, result = request(fixture.base+'/api/native/login/'+operation, body, fixture.headers)
                assert status == 200, result
            auth_before = (fixture.home/'auth.json').read_bytes()
            config = fixture.config.replace('model_provider="fixture"', 'model_provider="openai"')
            config = 'openai_base_url='+json.dumps(fixture.base+'/v1')+'\n'+config
            config = config.split('[model_providers.fixture]')[0]
            for model in fixture.catalog['models']:
                model['prefer_websockets'] = True
            (fixture.home/'models.json').write_text(json.dumps(fixture.catalog))
            (fixture.home/'config.toml').write_text(config)
            client = fixture.client()
            thread = client.rpc('thread/start', {'model':'responses/probe', 'cwd':str(fixture.home),
                                'approvalPolicy':'never', 'sandbox':'read-only'})['thread']['id']
            for index, model in enumerate(['responses/probe', 'chat/probe', 'anthropic/probe']):
                prompt = 'AUTH_OWNER_FIXTURE: reply once.' + (' x' * 65536 if index == 0 else '')
                turn(client, thread, prompt, model=model)
            client.close()
            client = fixture.client()
            client.rpc('thread/resume', {'threadId':thread})
            turn(client, thread, 'AUTH_OWNER_FIXTURE_AFTER_RESTART: reply once.')
            assert (fixture.home/'auth.json').read_bytes() == auth_before
            records = fixture.upstream.requests
            assert len(records) == 4, len(records)
            assert all(record['body']['model'] == 'probe' for record in records)
            assert all('x-emp-session' not in {key.lower() for key in record['headers']} for record in records)
            status, ledger = request(fixture.base+'/api/calls', headers=fixture.headers)
            assert status == 200, ledger
            assert len(ledger['records']) == 4, ledger
            assert all(row['state'] == 'completed' and row['transport'] == 'websocket' for row in ledger['records']), ledger
            result = {'auth_mode':mode, 'transport':'websocket', 'requests':len(records),
                      'large_user_input_bytes':131072,
                      'management_header_used':False, 'credentials_unchanged':True, 'passed':True}
            results.append(result)
            (evidence/mode/'result.json').write_text(json.dumps(result, indent=2)+'\n')
            print(json.dumps(result), flush=True)
    return results
