"""Verify mutable effort and Native-to-external model switches with a real engine."""
import json

from scripts.shadow_frontend.local_backend import request
from .fixture import Fixture, turn


def run(codex, emp, root, evidence, transport='sse'):
    results = []
    for target in ['gpt-6.1-sol', 'responses/probe', 'chat/probe', 'anthropic/probe']:
        label = target.replace('/', '-')
        with Fixture(codex, emp, root/label, evidence/label) as fixture:
            if transport == 'websocket':
                fixture.config = fixture.config.replace('supports_websockets=false', 'supports_websockets=true')
                (fixture.home/'config.toml').write_text(fixture.config)
                for model in fixture.catalog['models']:
                    model['prefer_websockets'] = '/' not in model['slug']
                (fixture.home/'models.json').write_text(json.dumps(fixture.catalog))
            client = fixture.client()
            thread = client.rpc('thread/start', {'model':'gpt-6.1-sol', 'cwd':str(fixture.home),
                                'approvalPolicy':'never', 'sandbox':'read-only'})['thread']['id']
            for index, effort in enumerate(['high', 'low', 'low', 'xhigh']):
                turn(client, thread, f'EFFORT_TURN_{index}', model=target if index else 'gpt-6.1-sol', effort=effort)
            client.close()
            client = fixture.client()
            client.rpc('thread/resume', {'threadId':thread})
            turn(client, thread, 'EFFORT_AFTER_RESUME', effort='medium')
            records = [record for record in fixture.upstream.requests if record['body'].get('generate') is not False]
            observed = []
            effective = {}
            for record in records:
                body = record['body']
                updates = [item for item in body.get('input', []) if isinstance(item, dict)
                           and item.get('type') == 'configuration_update']
                effort = (updates[-1]['reasoning']['effort'] if updates else
                              effective.get(body.get('previous_response_id'),
                              body.get('reasoning_effort', body.get('output_config', {}).get('effort',
                              body.get('reasoning', {}).get('effort')))))
                effective[record['response_id']] = effort
                observed.append({'path':record['path'], 'transport':record.get('transport', 'sse'),
                    'root_effort':body.get('reasoning', {}).get('effort'),
                    'effort':effort, 'updates':updates, 'previous_response_id':body.get('previous_response_id')})
            status, ledger = request(fixture.base+'/api/calls', headers=fixture.headers)
            assert status == 200, ledger
            result = {'target':target, 'observed':observed, 'ledger':ledger}
            (evidence/label/'result.json').write_text(json.dumps(result, indent=2))
            expected = ['high', 'low', 'low', 'xhigh', 'medium']
            assert len(records) == len(expected), (target, len(records))
            assert [item['effort'] for item in observed] == expected, (target, observed)
            completed = [row for row in reversed(ledger['records']) if row['state'] == 'completed']
            assert [row['requested_effort'] for row in completed] == expected, (target, completed)
            assert all(row['state'] in ('completed', 'recovery_required') for row in ledger['records']), ledger
            if transport == 'websocket':
                assert observed[0]['transport'] == 'websocket', result
            print(json.dumps({'target':target, 'transport':transport, 'requests':len(records), 'passed':True}), flush=True)
            results.append({'target':target, 'passed':True, 'requests':len(records)})
    return results
