"""A real Codex task executes tools while EMP adjusts effort between inferences."""
import json

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, message, turn


def run(codex, emp, root, evidence, transport='sse', source='native'):
    with Fixture(codex, emp, root, evidence, configuration={
            'adaptive_effort':{'enabled':True,'classifier_model':'gpt-6-luna','max_effort':'xhigh'}}) as fixture:
        selected_model = 'gpt-6.1-sol'
        if source == 'imported':
            fixture.import_account()
            selected_model = 'imported/' + selected_model
            template = next(item for item in fixture.catalog['models'] if item['slug'] == 'gpt-6.1-sol')
            fixture.catalog['models'].append({**template, 'slug':selected_model, 'display_name':selected_model})
        if transport == 'websocket':
            (fixture.home/'config.toml').write_text(fixture.config.replace(
                'supports_websockets=false', 'supports_websockets=true'))
            for model in fixture.catalog['models']:
                model['prefer_websockets'] = True
        (fixture.home/'models.json').write_text(json.dumps(fixture.catalog))
        main, classifications, tools = [], [], []

        def respond(record):
            body = record['body']
            if body.get('generate') is False:
                return message()
            if body['model'] == 'gpt-6-luna':
                context = body['input'][0]['content']
                selected = 'low' if 'tests passed' in context else 'xhigh' if 'conflicting failure' in context else 'low'
                classifications.append({'effort':selected,'context':json.loads(context)})
                return message(json.dumps({'effort':selected}))
            main.append(record)
            if len(main) == 3:
                return message('TASK_COMPLETE')
            assert len(main) < 3, main
            declarations = body.get('tools', []) + [tool for item in body['input']
                if item.get('type') == 'additional_tools' for tool in item['tools']]
            tools.extend((item.get('name') if item['type'] == 'namespace' else None, tool)
                         for item in declarations for tool in item.get('tools', [item]))
            namespace, tool = next((namespace, item) for namespace, item in tools
                if item.get('name') in ('shell_command','exec_command'))
            command = "printf 'conflicting failure\\n'; exit 1" if len(main) == 1 else "printf 'tests passed\\n'"
            arguments = {'command' if tool['name'] == 'shell_command' else 'cmd':command,
                         'workdir':str(fixture.home)}
            return {'type':'function_call','namespace':namespace,'name':tool['name'],'call_id':f'call_{len(main)}',
                    'arguments':json.dumps(arguments)}

        fixture.upstream.respond = respond
        client = fixture.client()
        environment = fixture.executor(client)
        thread = client.rpc('thread/start', {'model':selected_model, 'cwd':str(fixture.home),
                            'approvalPolicy':'never', 'sandbox':'read-only'})['thread']['id']
        turn(client, thread, 'Inspect the failing test, resolve it, and verify the result.',
             environments=[environment])
        assert len(main) == 3 and len(classifications) == 3, (len(main), classifications)
        effective = [next(item['reasoning']['effort'] for item in reversed(record['body']['input'])
                         if item.get('type') == 'configuration_update') for record in main]
        assert effective == ['low','xhigh','low'], effective
        assert all(record['body']['reasoning']['effort'] == 'high' for record in main), main
        assert all(item['context']['user'] for item in classifications), classifications
        assert 'conflicting failure' in json.dumps(classifications[1]['context']['tool_results'])
        assert 'tests passed' in json.dumps(classifications[2]['context']['tool_results'])
        if transport == 'websocket':
            assert all(record.get('transport') == 'websocket' for record in main)
        status, ledger = request(fixture.base+'/api/calls', headers=fixture.headers)
        assert status == 200, ledger
        selected = [row for row in reversed(ledger['records']) if row['route'] == 'responses']
        classifier_calls = [row for row in ledger['records'] if row['route'] == 'effort_classification']
        assert [row['requested_effort'] for row in selected] == effective, selected
        assert len(classifier_calls) == 3 and all(row['thread_id'] == thread for row in classifier_calls), classifier_calls
        assert all(row['output_tokens'] > 0 for row in classifier_calls), classifier_calls
        expected_token = 'imported-fixture-only' if source == 'imported' else 'fixture-only'
        assert all(next(value for key,value in record['headers'].items() if key.lower() == 'authorization')
                   == 'Bearer '+expected_token for record in fixture.upstream.requests), fixture.upstream.requests
        result = {'passed':True,'requests':6,'transport':transport,'user_turns':1,
                  'source':source,
                  'effective_effort':effective,'classifications':classifications,'ledger':ledger}
        (evidence/'result.json').write_text(json.dumps(result, indent=2))
        print(json.dumps({key:value for key,value in result.items() if key not in ('classifications','ledger')}), flush=True)
        return result
